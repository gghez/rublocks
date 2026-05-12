//! `sftp.write` — write bytes to a remote path on an SFTP server.
//!
//! Third of the four `sftp.*` operation blocks. Per the project rule
//! "exactly one way to spell each thing", a single block covers
//! create-or-overwrite — there is no separate `sftp.create` /
//! `sftp.update`. Idempotent overwrite is the default; opt-in error on
//! existing target via `if_exists: "error"`.
//!
//! See `docs/blocks/sftp.write.md`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::runtime::BlockCodegenCtx;
use super::{BlockInstance, BlockKind, LogValue, RawBlock};
use crate::manifest::ManifestError;
use crate::models::Model;
use crate::sftp::{SftpConnectionRef, SftpFieldValue, parse_connection_ref};
use crate::value_ref::{BindingKind, ScopeBinding, ValueRef, ValueScope};

/// POSIX mode applied when the user omits `mode`. Matches the default
/// behaviour of most SFTP clients (e.g. `sftp -P`, `scp`) so a written
/// file does not surprise downstream consumers expecting world-readable
/// content under the standard umask.
const DEFAULT_MODE: u32 = 0o644;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "sftp.write")]
    Tag,
}

/// Conflict policy for an `sftp.write`. The three variants mirror the
/// only sensible reactions to "target already exists" — overwrite (the
/// default, idempotent), error out (so the handler can dispatch to an
/// `on_conflict` sub-block or surface the canonical 409), or skip (no-op
/// ack with `size: 0`).
#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IfExists {
    #[default]
    Overwrite,
    Error,
    Skip,
}

/// Raw shape of an `sftp.write` block. `connection` is kept as opaque
/// JSON because every leaf accepts the shared `$ref`/`env:`/literal
/// trio — that parse happens in [`crate::sftp::parse_connection_ref`].
// `block` and `connection` are serde discriminators / staging fields
// consumed inside [`Kind::parse`] only.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: sftp.write")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to
    /// `crate::_rb_sftp::SftpWriteAck` for downstream blocks / `view`.
    pub name: String,
    /// `services.<name>` of `kind: "sftp"` — mutually exclusive with
    /// `connection`. Exactly one is required.
    #[serde(default)]
    pub service: Option<String>,
    /// Inline declaration — leaves may bind from a prior block via `$ref`.
    /// Mutually exclusive with `service`.
    #[serde(default)]
    pub connection: Option<Value>,
    /// Absolute destination path. Literal or `$ref`/`env:` form.
    pub path: Value,
    /// Reference to a bytes-shaped binding (typically from
    /// `csv.write`, `xlsx.write`, `pdf.render`, or `$input.body`).
    pub body: Value,
    /// POSIX mode applied to the created file, e.g. `"0o640"`. Validated
    /// as `0o[0-7]{3,4}` at load time. Default `"0o644"`.
    #[serde(default)]
    pub mode: Option<String>,
    /// When `true`, missing parent directories are created with mode
    /// `0o755`. Default `false`.
    #[serde(default)]
    pub mkdir_parents: bool,
    /// Conflict policy. Default `"overwrite"`.
    #[serde(default)]
    pub if_exists: IfExists,
    /// Sub-block executed when `if_exists: "error"` and the target
    /// exists. Same semantics as `db.find_one.on_missing`.
    #[serde(default)]
    pub on_conflict: Option<Value>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "sftp.write"
    }

    fn json_schema(&self) -> RootSchema {
        schema_for!(Spec)
    }

    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
        let spec: Spec =
            serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        if spec.name.trim().is_empty() {
            return Err(raw.validation_error("`name` must not be empty"));
        }
        let connection = parse_connection_ref(&raw.fields, &raw.source, &raw.label)?;
        let path = parse_path(&spec.path, raw)?;
        validate_absolute(&path, raw)?;
        let body = ValueRef::parse(&spec.body)
            .map_err(|e| raw.validation_error(format!("`body`: {e}")))?;
        let mode = match spec.mode.as_deref() {
            Some(s) => Some(parse_octal_mode(s, raw)?),
            None => None,
        };
        // `on_conflict` is only reachable when `if_exists: "error"` —
        // every other policy resolves the conflict in-place. Surfacing
        // this as a build error keeps the dist code free of an
        // unreachable handler branch.
        if spec.on_conflict.is_some() && spec.if_exists != IfExists::Error {
            return Err(
                raw.validation_error("`on_conflict` is only reachable when `if_exists: \"error\"`")
            );
        }
        let on_conflict = match spec.on_conflict.as_ref() {
            Some(v) => {
                let nested =
                    RawBlock::from_value(v, &raw.source, &format!("{}.on_conflict", raw.label))?;
                Some(super::parse(&nested)?)
            }
            None => None,
        };
        Ok(Box::new(Instance {
            spec,
            connection,
            path,
            body,
            mode,
            on_conflict,
        }))
    }
}

/// `path` accepts a literal absolute path or a `$ref`/`env:` form — same
/// trio as the inline connection leaves. Mirrors `sftp.list` so both
/// blocks share one parse contract.
fn parse_path(value: &Value, raw: &RawBlock) -> Result<SftpFieldValue, ManifestError> {
    match value {
        Value::String(s) => {
            if let Some(rest) = s.strip_prefix("env:") {
                Ok(SftpFieldValue::Env(rest.to_string()))
            } else if let Some(rest) = s.strip_prefix('$') {
                Ok(SftpFieldValue::Ref(rest.to_string()))
            } else {
                Ok(SftpFieldValue::Literal(s.clone()))
            }
        }
        _ => Err(raw.validation_error("`path` must be a string")),
    }
}

/// Only literal paths are checked for the leading slash at load time;
/// `$ref` / `env:` forms resolve at request time and a missing slash
/// there surfaces as a remote error.
fn validate_absolute(path: &SftpFieldValue, raw: &RawBlock) -> Result<(), ManifestError> {
    if let SftpFieldValue::Literal(s) = path
        && !s.starts_with('/')
    {
        return Err(raw.validation_error(format!(
            "`path` must be an absolute remote path (got `{s}`)"
        )));
    }
    Ok(())
}

/// Parse an octal mode literal of the form `0o[0-7]{3,4}`. Anything else
/// is a build error so a typo (e.g. `"640"` without the `0o` prefix, or
/// `"0o9"`) surfaces in the dev overlay instead of silently round-tripping
/// to a nonsensical mode at runtime.
fn parse_octal_mode(s: &str, raw: &RawBlock) -> Result<u32, ManifestError> {
    let rest = s
        .strip_prefix("0o")
        .ok_or_else(|| raw.validation_error(format!("`mode` must start with `0o` (got `{s}`)")))?;
    let len = rest.len();
    if !(3..=4).contains(&len) || !rest.chars().all(|c| ('0'..='7').contains(&c)) {
        return Err(raw.validation_error(format!("`mode` must match `0o[0-7]{{3,4}}` (got `{s}`)")));
    }
    u32::from_str_radix(rest, 8)
        .map_err(|_| raw.validation_error(format!("`mode` invalid octal (got `{s}`)")))
}

#[derive(Debug)]
pub struct Instance {
    spec: Spec,
    connection: SftpConnectionRef,
    path: SftpFieldValue,
    body: ValueRef,
    mode: Option<u32>,
    on_conflict: Option<Box<dyn BlockInstance>>,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "sftp.write"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        Some(quote! { crate::_rb_sftp::SftpWriteAck })
    }

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        let target = match &self.connection {
            SftpConnectionRef::Service(name) => format!("service:{name}"),
            SftpConnectionRef::Inline(_) => "connection:inline".to_string(),
        };
        let path = match &self.path {
            SftpFieldValue::Literal(s) => s.clone(),
            SftpFieldValue::Env(v) => format!("env:{v}"),
            SftpFieldValue::Ref(r) => format!("${r}"),
        };
        let if_exists = match self.spec.if_exists {
            IfExists::Overwrite => "overwrite",
            IfExists::Error => "error",
            IfExists::Skip => "skip",
        };
        vec![
            ("target", LogValue::Str(target)),
            ("path", LogValue::Str(path)),
            ("if_exists", LogValue::Str(if_exists.to_string())),
            (
                "mkdir_parents",
                LogValue::Int(self.spec.mkdir_parents as i64),
            ),
        ]
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        let name_ident = format_ident!("__block_{}", self.spec.name);
        let sftp_handle = emit_sftp_handle(&self.connection)?;
        let path_expr = emit_field_string(&self.path, scope)?;
        let body_emitted = self
            .body
            .emit_expr(scope)
            .map_err(|e| format!("sftp.write: {e}"))?;
        let body_expr = body_emitted.expr;
        let mode_lit = self.mode.unwrap_or(DEFAULT_MODE);
        let mkdir_parents = self.spec.mkdir_parents;
        let if_exists_tokens = match self.spec.if_exists {
            IfExists::Overwrite => quote! { crate::_rb_sftp::IfExists::Overwrite },
            IfExists::Error => quote! { crate::_rb_sftp::IfExists::Error },
            IfExists::Skip => quote! { crate::_rb_sftp::IfExists::Skip },
        };

        // `Conflict` is the only error branch that opts into the
        // `on_conflict` sub-block (or the default 409). Every other
        // error logs and returns the dev-surfaced 500 — same shape as
        // `db.find_one.on_missing` for read-side blocks.
        let conflict_tokens = if let Some(sub) = self.on_conflict.as_ref() {
            let mut sub_scope = ValueScope {
                input: scope.input,
                bindings: scope.bindings.clone(),
                models: scope.models,
            };
            super::runtime::emit_block_with_logging(sub.as_ref(), ctx, &mut sub_scope)?
        } else if self.spec.if_exists == IfExists::Error {
            super::error::render_logged_error_return(
                ctx.index,
                ctx.route_kind,
                409,
                "conflict",
                Some("remote target already exists"),
            )
        } else {
            // Unreachable under any policy other than `if_exists:
            // "error"` — the runtime never produces `Conflict` for
            // `overwrite` / `skip`. Build-time check above guarantees
            // no user-visible `on_conflict` lands here.
            TokenStream::new()
        };

        let log_err = super::runtime::log_block_error(ctx.index, quote! { e });

        let tokens = quote! {
            let #name_ident: crate::_rb_sftp::SftpWriteAck = {
                let __sftp = #sftp_handle;
                let __path: String = #path_expr;
                let __body = #body_expr;
                match __sftp.write(
                    &__path,
                    (&__body).as_ref(),
                    #mode_lit,
                    #mkdir_parents,
                    #if_exists_tokens,
                ).await {
                    Ok(ack) => ack,
                    Err(e) if e.is_conflict() => {
                        #conflict_tokens
                    }
                    Err(e) => {
                        #log_err
                        return crate::_rb_runtime::sftp_error(e);
                    }
                }
            };
        };

        scope.bindings.insert(
            self.spec.name.clone(),
            ScopeBinding {
                ident: name_ident,
                kind: BindingKind::Scalar {
                    ty: quote! { crate::_rb_sftp::SftpWriteAck },
                },
            },
        );

        Ok(tokens)
    }
}

fn emit_sftp_handle(conn: &SftpConnectionRef) -> Result<TokenStream, String> {
    match conn {
        SftpConnectionRef::Service(name) => {
            let ident = format_ident!("{}", name);
            Ok(quote! { std::sync::Arc::clone(&__state.#ident) })
        }
        SftpConnectionRef::Inline(_) => Err(
            "sftp.write: inline `connection` codegen lands with the next sftp.* PR — \
             use `service: \"<name>\"` for now"
                .to_string(),
        ),
    }
}

/// Lower an [`SftpFieldValue`] to a `String`-typed expression. `$ref`
/// resolution uses the same scope machinery the rest of the language
/// shares, then funnels every concrete type through `to_string()` so the
/// dist code can treat every leaf uniformly.
fn emit_field_string(v: &SftpFieldValue, scope: &ValueScope) -> Result<TokenStream, String> {
    match v {
        SftpFieldValue::Literal(s) => Ok(quote! { #s.to_string() }),
        SftpFieldValue::Env(var) => Ok(quote! { std::env::var(#var)? }),
        SftpFieldValue::Ref(path) => {
            let value = crate::value_ref::ValueRef::parse(&Value::String(format!("${path}")))
                .map_err(|e| format!("sftp.write: {e}"))?;
            let emitted = value.emit_expr(scope)?;
            let expr = emitted.expr;
            Ok(quote! { (#expr).to_string() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_path() -> PathBuf {
        PathBuf::from("/fake/route.json")
    }

    fn raw(body: &str) -> RawBlock {
        let v: Value = serde_json::from_str(body).unwrap();
        RawBlock::from_value(&v, &fake_path(), "process[0]").unwrap()
    }

    #[test]
    fn parses_minimal_service_form() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload"
            }"#);
        let parsed = Kind.parse(&r).unwrap();
        assert_eq!(parsed.kind_id(), "sftp.write");
        assert_eq!(parsed.name(), Some("uploaded"));
    }

    #[test]
    fn parses_full_service_form() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.xlsx",
                "body": "$report",
                "mode": "0o640",
                "mkdir_parents": true,
                "if_exists": "overwrite"
            }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn parses_inline_connection() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "connection": {
                    "host": "$tenant.sftp_host",
                    "user": "$tenant.sftp_user",
                    "auth": { "password": "$tenant.sftp_password" }
                },
                "path": "/outbox/report.csv",
                "body": "$payload"
            }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_both_service_and_connection() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "connection": {
                    "host": "h", "user": "u",
                    "auth": { "password": "p" }
                },
                "path": "/outbox/report.csv",
                "body": "$payload"
            }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("not both"), "got: {}", err.message);
    }

    #[test]
    fn rejects_neither_service_nor_connection() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "path": "/outbox/report.csv",
                "body": "$payload"
            }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("exactly one is required"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload",
                "junk": true
            }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("unknown field"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_non_absolute_literal_path() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "outbox/report.csv",
                "body": "$payload"
            }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("absolute"), "got: {}", err.message);
    }

    #[test]
    fn accepts_ref_path_without_absolute_check() {
        // Refs and env reads only resolve at request time — load-time
        // absolute-path enforcement is intentionally skipped for them.
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "$tenant.drop_path",
                "body": "$payload"
            }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_bad_octal_mode_missing_prefix() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload",
                "mode": "640"
            }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("must start with `0o`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_bad_octal_mode_bad_digits() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload",
                "mode": "0o9"
            }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("0o[0-7]"), "got: {}", err.message);
    }

    #[test]
    fn accepts_four_digit_octal_mode() {
        // POSIX setuid/setgid/sticky bits live in the fourth octal
        // digit; `0o[0-7]{3,4}` accepts them so a manifest can ship a
        // `0o4755` mode without the parser rejecting it.
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload",
                "mode": "0o4755"
            }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn parses_if_exists_skip() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload",
                "if_exists": "skip"
            }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn parses_on_conflict_recursively() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload",
                "if_exists": "error",
                "on_conflict": {
                    "block": "error",
                    "status": 409,
                    "code": "remote_exists",
                    "description": "Target already on disk."
                }
            }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_on_conflict_without_error_policy() {
        // Build-time guard: pairing `on_conflict` with a non-`error`
        // policy leaves the handler with an unreachable branch.
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload",
                "if_exists": "overwrite",
                "on_conflict": {
                    "block": "error",
                    "status": 409,
                    "code": "remote_exists"
                }
            }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message
                .contains("only reachable when `if_exists: \"error\"`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_on_conflict_with_unknown_block() {
        let r = raw(r#"{
                "block": "sftp.write",
                "name": "uploaded",
                "service": "files",
                "path": "/outbox/report.csv",
                "body": "$payload",
                "if_exists": "error",
                "on_conflict": { "block": "nope" }
            }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("unknown block"),
            "got: {}",
            err.message
        );
    }
}
