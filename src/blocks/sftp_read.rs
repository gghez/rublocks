//! `sftp.read` — download a single remote file's contents into memory.
//!
//! Second of the four `sftp.*` operation blocks. Consumes the shared
//! connection contract from `crate::sftp`: every call points at either a
//! `services.<name>` of `kind: "sftp"` or an inline `connection` whose
//! leaves bind from a prior block. The block binds the downloaded bytes
//! to `$<name>` as `bytes::Bytes`, ready to feed the file-conversion
//! blocks (`csv.read`, `xlsx.read`, …) that consume `Bytes`.
//!
//! See `docs/blocks/sftp.read.md`.

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

/// Project-wide hard ceiling on `max_bytes` when the block omits the
/// field. 10 MiB matches the issue spec — large enough to swallow most
/// CSV / XLSX drops, small enough that a misconfigured cap cannot
/// exhaust the server's memory on a single oversized download.
const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "sftp.read")]
    Tag,
}

/// Raw shape of an `sftp.read` block. `connection` is kept as opaque
/// JSON because every leaf accepts the shared `$ref`/`env:`/literal
/// trio — that parse happens in [`crate::sftp::parse_connection_ref`].
// `block` and `connection` are serde discriminators / staging fields
// consumed inside [`Kind::parse`] only.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: sftp.read")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `bytes::Bytes` for downstream
    /// blocks (`csv.read`, `xlsx.read`, …) and for `view` / `output`.
    pub name: String,
    /// `services.<name>` of `kind: "sftp"` — mutually exclusive with
    /// `connection`. Exactly one is required.
    #[serde(default)]
    pub service: Option<String>,
    /// Inline declaration — leaves may bind from a prior block via `$ref`.
    /// Mutually exclusive with `service`.
    #[serde(default)]
    pub connection: Option<Value>,
    /// Absolute remote file path. Literal or `$ref` / `env:` form.
    pub path: Value,
    /// Hard cap on the download size, in bytes. Default
    /// [`DEFAULT_MAX_BYTES`]. Exceeding the cap aborts the transfer and
    /// surfaces a `413` response carrying the actual remote size — the
    /// dev-mode reader fixes the cap from the in-browser error.
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// Sub-block executed when `path` does not exist on the remote. Parsed
    /// recursively against the registry. Without it, ENOENT propagates as
    /// 404 surfaced in dev mode.
    #[serde(default)]
    pub on_missing: Option<Value>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "sftp.read"
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
        if let Some(0) = spec.max_bytes {
            return Err(raw.validation_error("`max_bytes` must be > 0"));
        }
        let connection = parse_connection_ref(&raw.fields, &raw.source, &raw.label)?;
        let path = parse_path(&spec.path, raw)?;
        validate_absolute(&path, raw)?;
        let on_missing = match spec.on_missing.as_ref() {
            Some(v) => {
                let nested =
                    RawBlock::from_value(v, &raw.source, &format!("{}.on_missing", raw.label))?;
                Some(super::parse(&nested)?)
            }
            None => None,
        };
        let max_bytes = spec.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        Ok(Box::new(Instance {
            spec,
            connection,
            path,
            max_bytes,
            on_missing,
        }))
    }
}

/// Path can be a literal absolute path or a `$ref` / `env:` form — same
/// trio as the inline connection leaves. Parsed via [`SftpFieldValue`]
/// so the resolver and the codegen share one shape.
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

#[derive(Debug)]
pub struct Instance {
    spec: Spec,
    connection: SftpConnectionRef,
    path: SftpFieldValue,
    max_bytes: u64,
    on_missing: Option<Box<dyn BlockInstance>>,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "sftp.read"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        Some(quote! { bytes::Bytes })
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
            ("max_bytes", LogValue::Int(self.max_bytes as i64)),
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
        let max_bytes = self.max_bytes;

        // Error dispatch: ENOENT-on-path is the only branch that opts
        // into `on_missing` (or the default 404). `Oversize` and every
        // other error log and return the dev-surfaced response — same
        // shape as `db.find_one` / `sftp.list`.
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
            let #name_ident: bytes::Bytes = {
                let __sftp = #sftp_handle;
                let __path: String = #path_expr;
                let __max_bytes: u64 = #max_bytes;
                match __sftp.read(&__path, __max_bytes).await {
                    Ok(b) => b,
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
                    ty: quote! { bytes::Bytes },
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
            "sftp.read: inline `connection` codegen lands with the next sftp.* PR — \
             use `service: \"<name>\"` for now"
                .to_string(),
        ),
    }
}

/// Lower a [`SftpFieldValue`] to a `String`-typed expression. `$ref`
/// resolution uses the same scope machinery the rest of the language
/// shares, then funnels every concrete type through `to_string()` so
/// the dist code can treat every leaf uniformly.
fn emit_field_string(v: &SftpFieldValue, scope: &ValueScope) -> Result<TokenStream, String> {
    match v {
        SftpFieldValue::Literal(s) => Ok(quote! { #s.to_string() }),
        SftpFieldValue::Env(var) => Ok(quote! { std::env::var(#var)? }),
        SftpFieldValue::Ref(path) => {
            let value = crate::value_ref::ValueRef::parse(&Value::String(format!("${path}")))
                .map_err(|e| format!("sftp.read: {e}"))?;
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
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "path": "/incoming/orders.csv"
        }"#);
        let parsed = Kind.parse(&r).unwrap();
        assert_eq!(parsed.kind_id(), "sftp.read");
        assert_eq!(parsed.name(), Some("raw"));
    }

    #[test]
    fn parses_inline_connection() {
        let r = raw(r#"{
            "block": "sftp.read",
            "name": "raw",
            "connection": {
                "host": "$tenant.sftp_host",
                "user": "$tenant.sftp_user",
                "auth": { "password": "$tenant.sftp_password" }
            },
            "path": "/incoming/orders.csv"
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_both_service_and_connection() {
        let r = raw(r#"{
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "connection": {
                "host": "h", "user": "u",
                "auth": { "password": "p" }
            },
            "path": "/incoming/orders.csv"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("not both"), "got: {}", err.message);
    }

    #[test]
    fn rejects_neither_service_nor_connection() {
        let r = raw(r#"{
            "block": "sftp.read",
            "name": "raw",
            "path": "/incoming/orders.csv"
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
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "path": "/incoming/orders.csv",
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
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "path": "orders.csv"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("absolute"), "got: {}", err.message);
    }

    #[test]
    fn accepts_ref_path_without_absolute_check() {
        // Refs and env reads only resolve at request time — load-time
        // absolute-path enforcement is intentionally skipped for them so
        // the user can compose paths from typed bindings or env vars.
        let r = raw(r#"{
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "path": "$tenant.drop_file"
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_zero_max_bytes() {
        // `0` is never useful and almost certainly a typo — surface
        // it at load time rather than at the first request that always
        // fires `Oversize`.
        let r = raw(r#"{
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "path": "/incoming/orders.csv",
            "max_bytes": 0
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("max_bytes"), "got: {}", err.message);
    }

    #[test]
    fn parses_max_bytes_when_set() {
        let r = raw(r#"{
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "path": "/incoming/orders.csv",
            "max_bytes": 524288
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn parses_on_missing_recursively() {
        // The nested block is parsed against the registry so a typo on
        // the sub-block surfaces at load time (same contract as
        // `db.find_one.on_missing`).
        let r = raw(r#"{
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "path": "/incoming/orders.csv",
            "on_missing": {
                "block": "error",
                "status": 404,
                "code": "remote_file_not_found",
                "description": "Remote file not found."
            }
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_on_missing_with_unknown_block() {
        let r = raw(r#"{
            "block": "sftp.read",
            "name": "raw",
            "service": "files",
            "path": "/incoming/orders.csv",
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
