//! Shared codegen helpers used by every block's runtime emission.
//!
//! Each block in a route's `process` produces a chunk of generated
//! handler code. The orchestration lives in `crate::codegen`: it iterates
//! the route's blocks, builds a [`BlockCodegenCtx`] per step, and calls
//! [`super::BlockInstance::emit_code`]. Per-block code embeds the right
//! sqlx queries / CEL evaluations / error short-circuits, with all
//! `$ref` references resolved against the running scope.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::blocks::{BlockInstance, LogValue};
use crate::manifest::DbKind;
use crate::models::Model;
use crate::routes::RouteKind;
use crate::value_ref::{BindingKind, EmittedExpr, ValueScope};
use crate::where_clause::{ColumnClause, WhereOp, WhereSpec};

/// Per-block codegen context passed to [`super::BlockInstance::emit_code`].
///
/// Holds the immutable parts of the route's compilation environment
/// (models, database backend, route kind). The mutable
/// [`crate::value_ref::ValueScope`] is passed separately so each block
/// can register a fresh binding for downstream resolution.
pub struct BlockCodegenCtx<'a> {
    pub models: &'a [Model],
    pub db_kind: Option<DbKind>,
    pub route_kind: RouteKind,
    /// Index of this block in `process` — used in diagnostic labels.
    pub index: usize,
}

/// sqlx::Database type literal for the project's backend. Drives the
/// `QueryBuilder<'a, Database>` parameterisation in emitted code.
pub fn sqlx_database_ty(kind: DbKind) -> TokenStream {
    match kind {
        DbKind::Postgres => quote! { sqlx::Postgres },
        DbKind::Mysql | DbKind::Mariadb => quote! { sqlx::MySql },
        // sqlx 0.8 dropped the mssql driver; the build fails earlier
        // when a manifest asks for it. Emit a stub so this helper still
        // type-checks when it's traversed.
        DbKind::Mssql => quote! { () },
    }
}

/// Emit the SQL fragment + bind tokens for one structured `WHERE` clause.
///
/// The generated code uses `sqlx::QueryBuilder` to interleave literal SQL
/// text and `.push_bind(...)` calls for every reference / literal value.
/// Returns `None` when the spec has no clauses (e.g. an empty object).
pub fn emit_where(
    spec: &WhereSpec,
    table: &str,
    scope: &ValueScope,
    builder: &proc_macro2::Ident,
    models: &[Model],
) -> Result<Option<TokenStream>, String> {
    match spec {
        WhereSpec::Cel(expr) => {
            let cols = column_names(models, table)?;
            let cols_ref: Vec<&str> = cols.iter().map(String::as_str).collect();
            let sql = crate::sql_where::compile(expr, &cols_ref)?;
            // Compile-time: every literal is part of the SQL string, no
            // bindings. The current CEL-form translator already shapes
            // the placeholders as $1.. which we re-key off the builder.
            let pieces = postgres_placeholders_to_pushes(&sql.sql, &sql.params, builder);
            Ok(Some(pieces))
        }
        WhereSpec::Structured(clauses) => {
            if clauses.is_empty() {
                return Ok(None);
            }
            let cols = column_names(models, table)?;
            for c in clauses {
                if !cols.iter().any(|k| k == &c.column) {
                    return Err(format!(
                        "unknown column `{}` — known: {}",
                        c.column,
                        cols.join(", ")
                    ));
                }
            }
            let mut tokens = Vec::with_capacity(clauses.len());
            for (i, clause) in clauses.iter().enumerate() {
                let prefix = if i == 0 { "" } else { " AND " };
                tokens.push(emit_clause(prefix, clause, scope, builder, models)?);
            }
            Ok(Some(quote! { #(#tokens)* }))
        }
    }
}

fn emit_clause(
    prefix: &str,
    clause: &ColumnClause,
    scope: &ValueScope,
    builder: &proc_macro2::Ident,
    models: &[Model],
) -> Result<TokenStream, String> {
    let col = &clause.column;
    match &clause.op {
        WhereOp::IsNull => {
            let lit = format!("{prefix}\"{col}\" IS NULL");
            Ok(quote! { #builder.push(#lit); })
        }
        WhereOp::IsNotNull => {
            let lit = format!("{prefix}\"{col}\" IS NOT NULL");
            Ok(quote! { #builder.push(#lit); })
        }
        WhereOp::Eq(v)
        | WhereOp::Ne(v)
        | WhereOp::Lt(v)
        | WhereOp::Le(v)
        | WhereOp::Gt(v)
        | WhereOp::Ge(v) => {
            let op = match &clause.op {
                WhereOp::Eq(_) => "=",
                WhereOp::Ne(_) => "<>",
                WhereOp::Lt(_) => "<",
                WhereOp::Le(_) => "<=",
                WhereOp::Gt(_) => ">",
                WhereOp::Ge(_) => ">=",
                _ => unreachable!(),
            };
            let lhs = format!("{prefix}\"{col}\" {op} ");
            let value = v.emit_expr(scope)?;
            let _ = models;
            let bind = bind_token(&value);
            Ok(quote! {
                #builder.push(#lhs);
                #builder.push_bind(#bind);
            })
        }
        WhereOp::In(refs) => {
            if refs.is_empty() {
                // `IN ()` is invalid SQL; expand to a always-false
                // literal so the row count matches a zero-element list.
                let lit = format!("{prefix}1 = 0");
                return Ok(quote! { #builder.push(#lit); });
            }
            let head = format!("{prefix}\"{col}\" IN (");
            let mut pieces = Vec::with_capacity(refs.len() * 2 + 2);
            pieces.push(quote! { #builder.push(#head); });
            for (i, r) in refs.iter().enumerate() {
                let sep = if i == 0 { "" } else { ", " };
                if !sep.is_empty() {
                    pieces.push(quote! { #builder.push(#sep); });
                }
                let v = r.emit_expr(scope)?;
                let bind = bind_token(&v);
                pieces.push(quote! { #builder.push_bind(#bind); });
            }
            pieces.push(quote! { #builder.push(")"); });
            Ok(quote! { #(#pieces)* })
        }
    }
}

/// Bind a [`crate::value_ref::EmittedExpr`] to a sqlx-compatible owned
/// value. Strings/Uuids/timestamps clone; copy types pass through.
fn bind_token(value: &EmittedExpr) -> TokenStream {
    let expr = &value.expr;
    quote! { #expr }
}

/// Translate the postgres-style `$N` placeholders produced by
/// [`crate::sql_where::compile`] into the `QueryBuilder` push sequence
/// used at runtime.
fn postgres_placeholders_to_pushes(
    sql: &str,
    params: &[crate::sql_where::Param],
    builder: &proc_macro2::Ident,
) -> TokenStream {
    let mut out: Vec<TokenStream> = Vec::new();
    let mut chunk = String::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek().is_some_and(|n| n.is_ascii_digit()) {
            if !chunk.is_empty() {
                let lit = std::mem::take(&mut chunk);
                out.push(quote! { #builder.push(#lit); });
            }
            let mut idx_str = String::new();
            while let Some(n) = chars.peek() {
                if n.is_ascii_digit() {
                    idx_str.push(*n);
                    chars.next();
                } else {
                    break;
                }
            }
            let idx: usize = idx_str.parse().expect("digits");
            let p = &params[idx - 1];
            let bind = match p {
                crate::sql_where::Param::String(s) => quote! { #s.to_string() },
                crate::sql_where::Param::Int(i) => quote! { #i },
                crate::sql_where::Param::Bool(b) => quote! { #b },
            };
            out.push(quote! { #builder.push_bind(#bind); });
        } else {
            chunk.push(c);
        }
    }
    if !chunk.is_empty() {
        out.push(quote! { #builder.push(#chunk); });
    }
    quote! { #(#out)* }
}

/// Look up the columns of `table` in the loaded model set. Returned as
/// owned `Vec<String>` so callers can borrow without lifetime acrobatics.
pub fn column_names(models: &[Model], table: &str) -> Result<Vec<String>, String> {
    let model = models
        .iter()
        .find(|m| m.table == table)
        .ok_or_else(|| format!("no model declares table `{table}`"))?;
    Ok(model.fields.keys().cloned().collect())
}

/// Emit a `SELECT <cols> FROM "<table>"` head suitable for prefixing
/// onto a `QueryBuilder`.
pub fn select_head(table: &str, models: &[Model]) -> Result<String, String> {
    let cols = column_names(models, table)?;
    let cols = cols
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("SELECT {cols} FROM \"{table}\""))
}

/// Convenience: build a [`BindingKind`] for a `db.find_one` block.
pub fn find_one_binding(table: &str) -> BindingKind {
    BindingKind::FindOne {
        table: table.to_string(),
    }
}

/// Convenience: build a [`BindingKind`] for a `db.find_many` block.
pub fn find_many_binding(table: &str) -> BindingKind {
    BindingKind::FindMany {
        table: table.to_string(),
    }
}

/// Identifier of the per-block `Instant::now()` local. Re-used by both the
/// prelude (which creates it) and every error/success event (which reads its
/// `elapsed()`), so the duration always tracks from the block's first byte.
pub fn block_start_ident(index: usize) -> proc_macro2::Ident {
    format_ident!("__rb_block_start_{index}")
}

/// Identifier of the per-block `tracing::span::Span` local. Holding it on a
/// `let` makes the `.entered()` guard outlive every event emitted from inside
/// the block body — including nested `on_missing` sub-blocks.
fn block_span_ident(index: usize) -> proc_macro2::Ident {
    format_ident!("__rb_block_span_{index}")
}

/// Tokens that fold one [`LogValue`] into a `tracing` macro field list.
///
/// `tracing` accepts `key = value` pairs where `value` must implement
/// `tracing::Value`. Strings flow in as `&str` (zero-cost when the value is a
/// literal); integers as `i64` (the widest native discrete-numeric).
fn log_value_tokens(value: &LogValue) -> TokenStream {
    match value {
        LogValue::Str(s) => quote! { #s },
        LogValue::Int(i) => quote! { #i as i64 },
    }
}

/// Build the comma-separated `key = value` field list shared by every event
/// the block emits. The `block` field comes first so logs sort/group by kind
/// without depending on subscriber-side field ordering.
fn static_field_list(kind_id: &'static str, fields: &[(&'static str, LogValue)]) -> TokenStream {
    let entries = fields.iter().map(|(k, v)| {
        let key_ident = format_ident!("{k}");
        let value_tokens = log_value_tokens(v);
        quote! { #key_ident = #value_tokens }
    });
    quote! { block = #kind_id, #(#entries),* }
}

/// Codegen prelude wrapped around every block's body — issue #17.
///
/// Records the start instant and enters a `tracing::info_span!` that carries
/// the kind id plus the block's static [`LogValue`] metadata. The span guard
/// is held on a `let` binding so every nested `tracing::*!` call (including
/// the block's own error-path emissions) inherits the span's fields.
pub fn log_block_prelude(
    kind_id: &'static str,
    fields: &[(&'static str, LogValue)],
    index: usize,
) -> TokenStream {
    let start = block_start_ident(index);
    let span = block_span_ident(index);
    let guard = format_ident!("_rb_block_guard_{index}");
    let field_list = static_field_list(kind_id, fields);
    quote! {
        let #start: ::std::time::Instant = ::std::time::Instant::now();
        let #span = ::tracing::info_span!(target: "rublocks::block", "block", #field_list);
        let #guard = #span.enter();
    }
}

/// Success-side event — emitted after the block body when execution falls
/// through. The current span supplies `block` + static fields; this call
/// just adds the timing and the `msg: "ok"` payload.
pub fn log_block_success(index: usize) -> TokenStream {
    let start = block_start_ident(index);
    quote! {
        ::tracing::info!(
            target: "rublocks::block",
            duration_us = #start.elapsed().as_micros() as u64,
            "ok"
        );
    }
}

/// Error-side event — call before each `return ...` exit inside a block body.
///
/// Takes an `error_expr` token stream pointing at a value that implements
/// `std::error::Error`. The expression is read by reference so the caller
/// can still `return _rb_runtime::db_error(e)` after — `sqlx::Error` is not
/// `Copy`, so moving it for logging would break the response build.
///
/// The current span carries the static block fields, so the event line
/// ships them automatically. The `source()` chain walk and best-effort
/// backtrace capture happen through helpers in the dist-side `_rb_log`.
pub fn log_block_error(index: usize, error_expr: TokenStream) -> TokenStream {
    let start = block_start_ident(index);
    quote! {
        {
            let __rb_err_ref: &(dyn ::std::error::Error + 'static) = &(#error_expr);
            let __rb_err_chain: ::std::vec::Vec<::std::string::String> =
                crate::_rb_log::error_chain(__rb_err_ref);
            let __rb_err_backtrace: ::std::option::Option<::std::string::String> =
                crate::_rb_log::error_backtrace();
            ::tracing::error!(
                target: "rublocks::block",
                duration_us = #start.elapsed().as_micros() as u64,
                error = %__rb_err_ref,
                error.chain = ?__rb_err_chain,
                backtrace = __rb_err_backtrace.as_deref(),
                "block failed"
            );
        }
    }
}

/// Event for block errors that don't carry a `std::error::Error` value —
/// e.g. `error`, `guard`, `field_validation_error`. The caller passes the
/// pre-formatted message that becomes the `error =` field.
pub fn log_block_error_message(index: usize, message_expr: TokenStream) -> TokenStream {
    let start = block_start_ident(index);
    quote! {
        ::tracing::error!(
            target: "rublocks::block",
            duration_us = #start.elapsed().as_micros() as u64,
            error = %#message_expr,
            "block failed"
        );
    }
}

/// Emit one block's tokens wrapped with the logging prelude + success
/// event. The block body is responsible for emitting its own `error!`
/// events on failure paths (helpers above) before each `return`. Used both
/// from the per-route codegen loop and from blocks that own nested
/// sub-blocks (`db.find_one`'s `on_missing`).
pub fn emit_block_with_logging(
    block: &dyn BlockInstance,
    ctx: &BlockCodegenCtx,
    scope: &mut ValueScope,
) -> Result<TokenStream, String> {
    let prelude = log_block_prelude(block.kind_id(), &block.log_fields(), ctx.index);
    let body = block.emit_code(ctx, scope)?;
    let success = if block.has_success_path() {
        log_block_success(ctx.index)
    } else {
        TokenStream::new()
    };
    Ok(quote! {
        #prelude
        #body
        #success
    })
}
