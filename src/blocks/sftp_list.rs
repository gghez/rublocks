//! `sftp.list` — list entries under a remote directory on an SFTP server.
//!
//! First of the four `sftp.*` operation blocks. Consumes the shared
//! connection contract from `crate::sftp`: every call points at either a
//! `services.<name>` of `kind: "sftp"` or an inline `connection` whose
//! leaves bind from a prior block. The block returns
//! `Vec<crate::_rb_sftp::SftpEntry>` — one entry per file/dir/link the
//! server reported under `path`, optionally filtered by a glob `pattern`
//! and recursed into subdirectories breadth-first.
//!
//! See `docs/blocks/sftp.list.md`.

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
use crate::value_ref::{BindingKind, ScopeBinding, ValueScope};

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "sftp.list")]
    Tag,
}

/// Raw shape of an `sftp.list` block. `connection` is kept as opaque
/// JSON because every leaf accepts the shared `$ref`/`env:`/literal
/// trio — that parse happens in [`crate::sftp::parse_connection_ref`].
// `block` and `connection` are serde discriminators / staging fields
// consumed inside [`Kind::parse`] only.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: sftp.list")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to
    /// `Vec<crate::_rb_sftp::SftpEntry>` for downstream blocks / `view`.
    pub name: String,
    /// `services.<name>` of `kind: "sftp"` — mutually exclusive with
    /// `connection`. Exactly one is required.
    #[serde(default)]
    pub service: Option<String>,
    /// Inline declaration — leaves may bind from a prior block via `$ref`.
    /// Mutually exclusive with `service`.
    #[serde(default)]
    pub connection: Option<Value>,
    /// Remote directory path. Literal absolute path or `$ref` form.
    pub path: Value,
    /// When `true`, walk subdirectories breadth-first. Default `false`.
    #[serde(default)]
    pub recursive: bool,
    /// Glob pattern applied to each entry's path relative to `path`.
    /// Examples: `"*.csv"`, `"**/2026/*.json"`. Validated at load time.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Sub-block executed when `path` does not exist on the remote. Parsed
    /// recursively against the registry. Without it, ENOENT propagates as
    /// 500 surfaced in dev mode.
    #[serde(default)]
    pub on_missing: Option<Value>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "sftp.list"
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
        if let Some(p) = spec.pattern.as_deref() {
            glob::Pattern::new(p)
                .map_err(|e| raw.validation_error(format!("invalid `pattern` glob `{p}`: {e}")))?;
        }
        let on_missing = match spec.on_missing.as_ref() {
            Some(v) => {
                let nested =
                    RawBlock::from_value(v, &raw.source, &format!("{}.on_missing", raw.label))?;
                Some(super::parse(&nested)?)
            }
            None => None,
        };
        Ok(Box::new(Instance {
            spec,
            connection,
            path,
            on_missing,
        }))
    }
}

/// Path can be a literal absolute path or a `$ref`/`env:` form — same trio
/// as the inline connection leaves. Parsed via [`SftpFieldValue`] so the
/// resolver and the codegen share one shape.
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

/// Only literal paths can be checked for the leading slash at load time;
/// `$ref` / `env:` forms resolve at request time and a missing slash there
/// surfaces as a remote error.
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

#[derive(Debug)]
pub struct Instance {
    spec: Spec,
    connection: SftpConnectionRef,
    path: SftpFieldValue,
    on_missing: Option<Box<dyn BlockInstance>>,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "sftp.list"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        Some(quote! { Vec<crate::_rb_sftp::SftpEntry> })
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
        vec![
            ("target", LogValue::Str(target)),
            ("path", LogValue::Str(path)),
            ("recursive", LogValue::Int(self.spec.recursive as i64)),
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
        let pattern_expr = match self.spec.pattern.as_deref() {
            Some(p) => quote! { Some(#p) },
            None => quote! { None::<&str> },
        };
        let recursive = self.spec.recursive;

        // Error dispatch: ENOENT-on-path is the only branch that opts into
        // `on_missing` (or the default 404). Every other error logs and
        // returns the dev-surfaced 500 — same shape as `db.find_one`.
        let on_missing_tokens = if let Some(sub) = self.on_missing.as_ref() {
            let mut sub_scope = ValueScope {
                input: scope.input,
                bindings: scope.bindings.clone(),
                models: scope.models,
            };
            super::runtime::emit_block_with_logging(sub.as_ref(), ctx, &mut sub_scope)?
        } else {
            super::error::default_not_found(ctx.index, ctx.route_kind)
        };

        let log_err = super::runtime::log_block_error(ctx.index, quote! { e });

        let tokens = quote! {
            let #name_ident: Vec<crate::_rb_sftp::SftpEntry> = {
                let __sftp = #sftp_handle;
                let __path: String = #path_expr;
                let __pattern: Option<&str> = #pattern_expr;
                match __sftp.list(&__path, #recursive, __pattern).await {
                    Ok(v) => v,
                    Err(e) if e.is_not_found() => {
                        #on_missing_tokens
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
                    ty: quote! { Vec<crate::_rb_sftp::SftpEntry> },
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
            "sftp.list: inline `connection` codegen lands with the next sftp.* PR — \
             use `service: \"<name>\"` for now"
                .to_string(),
        ),
    }
}

/// Lower a [`SftpFieldValue`] to a `String`-typed expression. `$ref`
/// resolution uses the same scope machinery the rest of the language
/// shares, then funnels every concrete type through `to_string()` so the
/// dist code can treat every leaf uniformly.
fn emit_field_string(v: &SftpFieldValue, scope: &ValueScope) -> Result<TokenStream, String> {
    match v {
        SftpFieldValue::Literal(s) => Ok(quote! { #s.to_string() }),
        SftpFieldValue::Env(var) => Ok(quote! { std::env::var(#var)? }),
        SftpFieldValue::Ref(path) => {
            let value = crate::value_ref::ValueRef::parse(&Value::String(format!("${path}")))
                .map_err(|e| format!("sftp.list: {e}"))?;
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
    fn parses_service_form_with_defaults() {
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "path": "/incoming"
        }"#);
        let parsed = Kind.parse(&r).unwrap();
        assert_eq!(parsed.kind_id(), "sftp.list");
        assert_eq!(parsed.name(), Some("files"));
    }

    #[test]
    fn parses_inline_connection() {
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "connection": {
                "host": "$tenant.sftp_host",
                "user": "$tenant.sftp_user",
                "auth": { "password": "$tenant.sftp_password" }
            },
            "path": "/incoming"
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_both_service_and_connection() {
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "connection": {
                "host": "h", "user": "u",
                "auth": { "password": "p" }
            },
            "path": "/incoming"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("not both"), "got: {}", err.message);
    }

    #[test]
    fn rejects_neither_service_nor_connection() {
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "path": "/incoming"
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
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "path": "/incoming",
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
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "path": "incoming"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("absolute"), "got: {}", err.message);
    }

    #[test]
    fn accepts_ref_path_without_absolute_check() {
        // Refs and env reads only resolve at request time — load-time
        // absolute-path enforcement is intentionally skipped for them so the
        // user can compose paths from typed bindings or env vars.
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "path": "$tenant.drop_path"
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_invalid_pattern() {
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "path": "/incoming",
            "pattern": "[invalid"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("invalid `pattern`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn parses_pattern_when_valid() {
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "path": "/incoming",
            "pattern": "**/2026/*.json"
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn parses_on_missing_recursively() {
        // The nested block is parsed against the registry so a typo on the
        // sub-block surfaces at load time (same contract as
        // `db.find_one.on_missing`).
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "path": "/incoming",
            "on_missing": {
                "block": "error",
                "status": 404,
                "code": "remote_dir_not_found",
                "description": "Remote dir not found."
            }
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_on_missing_with_unknown_block() {
        let r = raw(r#"{
            "block": "sftp.list",
            "name": "files",
            "service": "files",
            "path": "/incoming",
            "on_missing": { "block": "nope" }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("unknown block"),
            "got: {}",
            err.message
        );
    }
}
