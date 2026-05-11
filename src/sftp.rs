//! SFTP service declarations and the shared `service` / `connection` contract.
//!
//! Two real use cases drive this module:
//!
//! 1. A static SFTP target known at build time (single backup server, vendor
//!    drop folder). Declared in `main.json` under `services.<name>` with
//!    `kind: "sftp"`. Resolves to an `Arc<SftpService>` field on `AppState`.
//! 2. A dynamic SFTP target whose host/credentials come from a database row at
//!    request time (multi-tenant SaaS storing per-customer SFTP endpoints).
//!    Expressed inline on a `sftp.*` block via the `connection` field, every
//!    leaf accepting `$ref` so values bind from a prior block.
//!
//! The four `sftp.*` operation blocks (`list`, `read`, `write`, `delete`) live
//! in their own follow-up issues. This module ships the foundation: the
//! service-declaration types, the shared `service` / `connection` contract,
//! and the parser the future blocks will plug into.
//!
//! See `docs/blocks/sftp.md` for the user-facing reference.

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

use crate::manifest::{ManifestError, ServiceUrl};

/// Default SFTP port. SSH-2 defines this; we expose it for the codegen so the
/// dist crate matches whatever literal the manifest implies.
pub const DEFAULT_SFTP_PORT: u16 = 22;

/// `services.<name>` body when `kind == "sftp"`. Parsed via `serde` with
/// `deny_unknown_fields` so a typo on a field name surfaces at manifest load
/// time rather than as a silent default.
///
/// Every secret-shaped leaf (`auth.password`, `auth.private_key`,
/// `auth.private_key_pem`, `auth.passphrase`, optionally `host_key_fingerprint`)
/// accepts the same `env:VAR` indirection as other service URLs — the
/// `env:` form is the recommended one for real projects.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SftpService {
    /// Server hostname or address. Literal or `env:VAR`.
    #[schemars(with = "String")]
    pub host: ServiceUrl,
    /// SSH server port. Defaults to [`DEFAULT_SFTP_PORT`].
    #[serde(default = "default_sftp_port")]
    #[schemars(default = "default_sftp_port")]
    pub port: u16,
    /// SSH user name. Literal or `env:VAR`.
    #[schemars(with = "String")]
    pub user: ServiceUrl,
    /// Authentication material. Exactly one of `password`, `private_key`,
    /// `private_key_pem` must be set — enforced by [`SftpAuth::validate`].
    pub auth: SftpAuth,
    /// Expected server host key fingerprint (e.g. `"SHA256:..."`). When set,
    /// the client refuses to connect to a server presenting a different key.
    /// When omitted: dev mode trusts on first use; release startup rejects
    /// the missing value with a browser-visible error.
    #[serde(default)]
    #[schemars(default)]
    #[schemars(with = "Option<String>")]
    pub host_key_fingerprint: Option<ServiceUrl>,
}

fn default_sftp_port() -> u16 {
    DEFAULT_SFTP_PORT
}

/// Auth payload for an SFTP target. Exactly one of `password`,
/// `private_key`, or `private_key_pem` is allowed; `passphrase` is an
/// optional companion to the key forms.
///
/// Naming note: the user-facing parameter is "public key vs. password", but
/// the client authenticates with the *private* key (the server holds the
/// matching public key in `authorized_keys`). The field is named
/// `private_key` to match what the user actually configures.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SftpAuth {
    /// Cleartext password. Literal or `env:VAR`. Use `env:` in real projects.
    #[serde(default)]
    #[schemars(default)]
    #[schemars(with = "Option<String>")]
    pub password: Option<ServiceUrl>,
    /// Filesystem path to a PEM-encoded private key. Literal or `env:VAR`.
    #[serde(default)]
    #[schemars(default)]
    #[schemars(with = "Option<String>")]
    pub private_key: Option<ServiceUrl>,
    /// Inline PEM-encoded private key contents. Literal or `env:VAR`.
    #[serde(default)]
    #[schemars(default)]
    #[schemars(with = "Option<String>")]
    pub private_key_pem: Option<ServiceUrl>,
    /// Optional passphrase. Literal or `env:VAR`. Ignored under `password`.
    #[serde(default)]
    #[schemars(default)]
    #[schemars(with = "Option<String>")]
    pub passphrase: Option<ServiceUrl>,
}

/// Discriminator of which auth method an [`SftpAuth`] resolved to. Used by
/// codegen to emit the matching `russh` client call without re-inspecting
/// the `Option` triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpAuthMethod {
    Password,
    PrivateKey,
    PrivateKeyPem,
}

impl SftpAuth {
    /// Enforce the "exactly one of password/private_key/private_key_pem" rule.
    /// `source` is threaded so the resulting error points at the user-actionable
    /// file (typically `main.json`).
    pub fn validate(&self, source: &Path, ctx: &str) -> Result<SftpAuthMethod, ManifestError> {
        let methods = [
            ("password", self.password.is_some()),
            ("private_key", self.private_key.is_some()),
            ("private_key_pem", self.private_key_pem.is_some()),
        ];
        let set: Vec<&str> = methods
            .iter()
            .filter_map(|(k, v)| v.then_some(*k))
            .collect();
        match set.as_slice() {
            [] => Err(ManifestError::validation(
                source,
                format!(
                    "{ctx}: `auth` must set exactly one of `password`, `private_key`, `private_key_pem`"
                ),
            )),
            [one] => Ok(match *one {
                "password" => SftpAuthMethod::Password,
                "private_key" => SftpAuthMethod::PrivateKey,
                "private_key_pem" => SftpAuthMethod::PrivateKeyPem,
                _ => unreachable!("methods table is exhaustive"),
            }),
            many => Err(ManifestError::validation(
                source,
                format!(
                    "{ctx}: `auth` must set exactly one of `password`, `private_key`, `private_key_pem` (got {})",
                    many.join(", ")
                ),
            )),
        }
    }
}

/// Per-block connection contract, shared by every `sftp.*` block.
///
/// Either `service: "<name>"` (point at a `services.<name>` entry of kind
/// `sftp` in `main.json`) or `connection: {...}` (inline declaration whose
/// leaves accept `$ref` to bind values from a prior block's binding). Exactly
/// one is required; both or neither is a build error.
///
/// The four `sftp.*` operation blocks (`list`, `read`, `write`, `delete`) ship
/// in follow-up issues; this enum is the foundation contract they will plug
/// into. Tests in this module exercise the parser; the `#[allow(dead_code)]`
/// covers the gap between the contract landing and the first consuming block.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum SftpConnectionRef {
    /// Name of a `services.<name>` entry with `kind == "sftp"`.
    Service(String),
    /// Inline declaration — built per call, possibly reading `$ref` bindings.
    /// Boxed so the enum stays compact (the inline form holds five owned
    /// fields, dwarfing the `Service(String)` variant).
    Inline(Box<SftpConnectionInline>),
}

/// Inline form of [`SftpConnectionRef`]. Mirrors [`SftpService`] minus the
/// `kind` discriminator. Every leaf is a [`SftpFieldValue`] so a block can
/// stitch the connection from prior bindings (`$tenant.sftp_host`) or
/// environment indirection (`env:SFTP_PASSWORD`) or a literal.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SftpConnectionInline {
    pub host: SftpFieldValue,
    pub port: Option<SftpFieldValue>,
    pub user: SftpFieldValue,
    pub auth: SftpAuthInline,
    pub host_key_fingerprint: Option<SftpFieldValue>,
}

/// Inline auth payload. Same "exactly one of password/private_key/private_key_pem"
/// rule as the service form, enforced by [`SftpAuthInline::validate`].
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct SftpAuthInline {
    pub password: Option<SftpFieldValue>,
    pub private_key: Option<SftpFieldValue>,
    pub private_key_pem: Option<SftpFieldValue>,
    pub passphrase: Option<SftpFieldValue>,
}

impl SftpAuthInline {
    #[allow(dead_code)]
    pub fn validate(&self, source: &Path, ctx: &str) -> Result<SftpAuthMethod, ManifestError> {
        let methods = [
            ("password", self.password.is_some()),
            ("private_key", self.private_key.is_some()),
            ("private_key_pem", self.private_key_pem.is_some()),
        ];
        let set: Vec<&str> = methods
            .iter()
            .filter_map(|(k, v)| v.then_some(*k))
            .collect();
        match set.as_slice() {
            [] => Err(ManifestError::validation(
                source,
                format!(
                    "{ctx}: `connection.auth` must set exactly one of `password`, `private_key`, `private_key_pem`"
                ),
            )),
            [one] => Ok(match *one {
                "password" => SftpAuthMethod::Password,
                "private_key" => SftpAuthMethod::PrivateKey,
                "private_key_pem" => SftpAuthMethod::PrivateKeyPem,
                _ => unreachable!("methods table is exhaustive"),
            }),
            many => Err(ManifestError::validation(
                source,
                format!(
                    "{ctx}: `connection.auth` must set exactly one of `password`, `private_key`, `private_key_pem` (got {})",
                    many.join(", ")
                ),
            )),
        }
    }
}

/// One leaf of an inline `connection`. Three forms accepted:
///
/// - `Literal("sftp.example.com")` — plain value, embedded verbatim.
/// - `Env("SFTP_HOST")` — `std::env::var("SFTP_HOST")` resolved at request time.
/// - `Ref("tenant.sftp_host")` — pulls from a prior block binding. The leading
///   `$` is stripped on parse; the rest is a dotted path scope-checked by the
///   block consuming it.
///
/// Numeric leaves (e.g. `port`) accept the same three forms; the dist code
/// parses the resolved string to a `u16` at request time.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum SftpFieldValue {
    Literal(String),
    Env(String),
    Ref(String),
}

impl SftpFieldValue {
    /// Best-effort string read. JSON numbers/bools/literals all collapse to a
    /// stringified form so downstream codegen does not have to fork on type.
    #[allow(dead_code)]
    fn from_value(value: &Value, source: &Path, ctx: &str) -> Result<Self, ManifestError> {
        let raw = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => {
                return Err(ManifestError::validation(
                    source,
                    format!("{ctx}: must be a string, number, or boolean"),
                ));
            }
        };
        Ok(parse_field_string(&raw))
    }
}

/// Classify a string field into one of the three [`SftpFieldValue`] forms.
#[allow(dead_code)]
fn parse_field_string(raw: &str) -> SftpFieldValue {
    if let Some(rest) = raw.strip_prefix("env:") {
        SftpFieldValue::Env(rest.to_string())
    } else if let Some(rest) = raw.strip_prefix('$') {
        SftpFieldValue::Ref(rest.to_string())
    } else {
        SftpFieldValue::Literal(raw.to_string())
    }
}

/// Verify a `service: "<name>"` reference points at a known SFTP service.
///
/// Called by future `sftp.*` blocks after parsing the connection ref — keeps
/// the "service must exist and have `kind: sftp`" check in one place. Names
/// matching a typed slot (`db`, `postgres`, `redis`) or absent from the
/// manifest fail with a catalogue of valid SFTP service names.
#[allow(dead_code)]
pub fn validate_service_reference<'a>(
    name: &str,
    available: &'a [String],
    source: &Path,
    label: &str,
) -> Result<&'a str, ManifestError> {
    if let Some(svc) = available.iter().find(|n| *n == name) {
        return Ok(svc.as_str());
    }
    let known = if available.is_empty() {
        "no SFTP services declared in main.json".to_string()
    } else {
        format!("known SFTP services: {}", available.join(", "))
    };
    Err(ManifestError::validation(
        source,
        format!(
            "{label}: `service: \"{name}\"` does not point at a declared SFTP service ({known})"
        ),
    ))
}

/// Parse the `{ service | connection }` payload of one block.
///
/// `fields` is the block's full body (the block's discriminator already
/// stripped). The function reads `service` / `connection`, enforces
/// exactly-one, and surfaces a clean error pointing at the offending block.
///
/// Future `sftp.*` blocks call this from their `BlockKind::parse`; it lives
/// outside the block registry so the foundation issue can ship and test the
/// shared contract before any operation block exists.
#[allow(dead_code)]
pub fn parse_connection_ref(
    fields: &IndexMap<String, Value>,
    source: &Path,
    label: &str,
) -> Result<SftpConnectionRef, ManifestError> {
    let service = fields.get("service");
    let connection = fields.get("connection");
    match (service, connection) {
        (None, None) => Err(ManifestError::validation(
            source,
            format!("{label}: missing `service` or `connection` — exactly one is required"),
        )),
        (Some(_), Some(_)) => Err(ManifestError::validation(
            source,
            format!("{label}: set either `service` or `connection`, not both"),
        )),
        (Some(svc), None) => match svc {
            Value::String(name) => Ok(SftpConnectionRef::Service(name.clone())),
            _ => Err(ManifestError::validation(
                source,
                format!("{label}: `service` must be a string naming a `services.<name>` entry"),
            )),
        },
        (None, Some(conn)) => Ok(SftpConnectionRef::Inline(Box::new(
            parse_inline_connection(conn, source, label)?,
        ))),
    }
}

#[allow(dead_code)]
fn parse_inline_connection(
    value: &Value,
    source: &Path,
    label: &str,
) -> Result<SftpConnectionInline, ManifestError> {
    let Value::Object(map) = value else {
        return Err(ManifestError::validation(
            source,
            format!("{label}: `connection` must be an object"),
        ));
    };

    let allowed = ["host", "port", "user", "auth", "host_key_fingerprint"];
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ManifestError::validation(
                source,
                format!("{label}: `connection` has unknown field `{key}`"),
            ));
        }
    }

    let host = match map.get("host") {
        Some(v) => SftpFieldValue::from_value(v, source, &format!("{label}: `connection.host`"))?,
        None => {
            return Err(ManifestError::validation(
                source,
                format!("{label}: `connection.host` is required"),
            ));
        }
    };
    let user = match map.get("user") {
        Some(v) => SftpFieldValue::from_value(v, source, &format!("{label}: `connection.user`"))?,
        None => {
            return Err(ManifestError::validation(
                source,
                format!("{label}: `connection.user` is required"),
            ));
        }
    };
    let port = match map.get("port") {
        Some(v) => Some(SftpFieldValue::from_value(
            v,
            source,
            &format!("{label}: `connection.port`"),
        )?),
        None => None,
    };
    let host_key_fingerprint = match map.get("host_key_fingerprint") {
        Some(v) => Some(SftpFieldValue::from_value(
            v,
            source,
            &format!("{label}: `connection.host_key_fingerprint`"),
        )?),
        None => None,
    };
    let auth = parse_inline_auth(
        map.get("auth").ok_or_else(|| {
            ManifestError::validation(source, format!("{label}: `connection.auth` is required"))
        })?,
        source,
        label,
    )?;

    let inline = SftpConnectionInline {
        host,
        port,
        user,
        auth,
        host_key_fingerprint,
    };
    inline.auth.validate(source, label)?;
    Ok(inline)
}

#[allow(dead_code)]
fn parse_inline_auth(
    value: &Value,
    source: &Path,
    label: &str,
) -> Result<SftpAuthInline, ManifestError> {
    let Value::Object(map) = value else {
        return Err(ManifestError::validation(
            source,
            format!("{label}: `connection.auth` must be an object"),
        ));
    };
    let allowed = ["password", "private_key", "private_key_pem", "passphrase"];
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ManifestError::validation(
                source,
                format!("{label}: `connection.auth` has unknown field `{key}`"),
            ));
        }
    }
    let read = |key: &str| -> Result<Option<SftpFieldValue>, ManifestError> {
        match map.get(key) {
            Some(v) => Ok(Some(SftpFieldValue::from_value(
                v,
                source,
                &format!("{label}: `connection.auth.{key}`"),
            )?)),
            None => Ok(None),
        }
    };
    Ok(SftpAuthInline {
        password: read("password")?,
        private_key: read("private_key")?,
        private_key_pem: read("private_key_pem")?,
        passphrase: read("passphrase")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_path() -> PathBuf {
        PathBuf::from("/fake/route.json")
    }

    fn fields(json: &str) -> IndexMap<String, Value> {
        let v: Value = serde_json::from_str(json).unwrap();
        let Value::Object(map) = v else {
            panic!("expected object")
        };
        let mut out = IndexMap::new();
        for (k, v) in map {
            out.insert(k, v);
        }
        out
    }

    #[test]
    fn parse_service_form_extracts_name() {
        let f = fields(r#"{ "service": "files" }"#);
        let r = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap();
        match r {
            SftpConnectionRef::Service(name) => assert_eq!(name, "files"),
            _ => panic!("expected service form"),
        }
    }

    #[test]
    fn parse_rejects_both_service_and_connection() {
        let f = fields(
            r#"{ "service": "files", "connection": { "host": "h", "user": "u", "auth": { "password": "p" } } }"#,
        );
        let err = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap_err();
        assert!(err.message.contains("not both"), "got: {}", err.message);
    }

    #[test]
    fn parse_rejects_neither_service_nor_connection() {
        let f = fields(r#"{}"#);
        let err = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap_err();
        assert!(
            err.message.contains("exactly one is required"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn parse_inline_with_literal_values() {
        let f = fields(
            r#"{
                "connection": {
                    "host": "sftp.example.com",
                    "port": 22,
                    "user": "rublocks",
                    "auth": { "password": "env:SFTP_PASSWORD" }
                }
            }"#,
        );
        let r = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap();
        let SftpConnectionRef::Inline(inline) = r else {
            panic!("expected inline form")
        };
        match &inline.host {
            SftpFieldValue::Literal(s) => assert_eq!(s, "sftp.example.com"),
            other => panic!("expected literal, got {other:?}"),
        }
        match inline.auth.password.as_ref().unwrap() {
            SftpFieldValue::Env(v) => assert_eq!(v, "SFTP_PASSWORD"),
            other => panic!("expected env, got {other:?}"),
        }
    }

    #[test]
    fn parse_inline_with_ref_values() {
        // Every leaf accepts `$ref`. The leading `$` is stripped on parse so
        // the rest is a dotted path the block's scope checker can resolve.
        let f = fields(
            r#"{
                "connection": {
                    "host": "$tenant.sftp_host",
                    "port": "$tenant.sftp_port",
                    "user": "$tenant.sftp_user",
                    "auth": { "password": "$tenant.sftp_password" }
                }
            }"#,
        );
        let r = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap();
        let SftpConnectionRef::Inline(inline) = r else {
            panic!("expected inline form")
        };
        match &inline.host {
            SftpFieldValue::Ref(p) => assert_eq!(p, "tenant.sftp_host"),
            other => panic!("expected ref, got {other:?}"),
        }
        match inline.port.as_ref().unwrap() {
            SftpFieldValue::Ref(p) => assert_eq!(p, "tenant.sftp_port"),
            other => panic!("expected ref, got {other:?}"),
        }
    }

    #[test]
    fn parse_inline_rejects_unknown_field() {
        let f = fields(
            r#"{
                "connection": {
                    "host": "h",
                    "user": "u",
                    "auth": { "password": "p" },
                    "extra": "boom"
                }
            }"#,
        );
        let err = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap_err();
        assert!(
            err.message.contains("unknown field `extra`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn parse_inline_rejects_missing_auth() {
        let f = fields(r#"{ "connection": { "host": "h", "user": "u" } }"#);
        let err = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap_err();
        assert!(
            err.message.contains("`connection.auth` is required"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn parse_inline_rejects_multiple_auth_methods() {
        let f = fields(
            r#"{
                "connection": {
                    "host": "h",
                    "user": "u",
                    "auth": { "password": "p", "private_key": "/k" }
                }
            }"#,
        );
        let err = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap_err();
        assert!(
            err.message.contains("password, private_key"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn parse_inline_rejects_no_auth_method() {
        let f = fields(
            r#"{
                "connection": {
                    "host": "h",
                    "user": "u",
                    "auth": { "passphrase": "p" }
                }
            }"#,
        );
        let err = parse_connection_ref(&f, &fake_path(), "process[0]").unwrap_err();
        assert!(
            err.message.contains("exactly one of"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn validate_service_form_accepts_password_only() {
        let auth: SftpAuth = serde_json::from_value(serde_json::json!({
            "password": "env:SFTP_PASSWORD"
        }))
        .unwrap();
        let m = auth.validate(&fake_path(), "services.files").unwrap();
        assert_eq!(m, SftpAuthMethod::Password);
    }

    #[test]
    fn validate_service_form_accepts_private_key_only() {
        let auth: SftpAuth = serde_json::from_value(serde_json::json!({
            "private_key": "/keys/id_ed25519",
            "passphrase": "env:SFTP_KEY_PASSPHRASE"
        }))
        .unwrap();
        let m = auth.validate(&fake_path(), "services.files").unwrap();
        assert_eq!(m, SftpAuthMethod::PrivateKey);
    }

    #[test]
    fn validate_service_form_accepts_private_key_pem_only() {
        let auth: SftpAuth = serde_json::from_value(serde_json::json!({
            "private_key_pem": "env:SFTP_KEY_PEM"
        }))
        .unwrap();
        let m = auth.validate(&fake_path(), "services.files").unwrap();
        assert_eq!(m, SftpAuthMethod::PrivateKeyPem);
    }

    #[test]
    fn validate_service_form_rejects_multiple_methods() {
        let auth: SftpAuth = serde_json::from_value(serde_json::json!({
            "password": "p",
            "private_key_pem": "pem"
        }))
        .unwrap();
        let err = auth.validate(&fake_path(), "services.files").unwrap_err();
        assert!(
            err.message.contains("password, private_key_pem"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn validate_service_form_rejects_no_method() {
        let auth: SftpAuth = serde_json::from_value(serde_json::json!({
            "passphrase": "p"
        }))
        .unwrap();
        let err = auth.validate(&fake_path(), "services.files").unwrap_err();
        assert!(
            err.message.contains("exactly one of"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn sftp_service_rejects_unknown_field() {
        // `deny_unknown_fields` on `SftpService` is part of the acceptance
        // contract — typos must surface at manifest load time.
        let err = serde_json::from_value::<SftpService>(serde_json::json!({
            "host": "h",
            "user": "u",
            "auth": { "password": "p" },
            "junk": 1
        }))
        .unwrap_err();
        assert!(err.to_string().contains("junk"), "got: {err}");
    }

    #[test]
    fn validate_service_reference_accepts_known_name() {
        let available = vec!["files".to_string(), "backups".to_string()];
        let r =
            validate_service_reference("files", &available, &fake_path(), "process[0]").unwrap();
        assert_eq!(r, "files");
    }

    #[test]
    fn validate_service_reference_rejects_unknown_with_catalogue() {
        let available = vec!["files".to_string(), "backups".to_string()];
        let err =
            validate_service_reference("nope", &available, &fake_path(), "process[0]").unwrap_err();
        assert!(
            err.message.contains("`service: \"nope\"`")
                && err.message.contains("known SFTP services"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn validate_service_reference_lists_empty_catalogue_when_no_sftp_services() {
        let err = validate_service_reference("files", &[], &fake_path(), "process[0]").unwrap_err();
        assert!(
            err.message.contains("no SFTP services declared"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn sftp_service_defaults_port_to_22() {
        let svc: SftpService = serde_json::from_value(serde_json::json!({
            "host": "sftp.example.com",
            "user": "u",
            "auth": { "password": "p" }
        }))
        .unwrap();
        assert_eq!(svc.port, DEFAULT_SFTP_PORT);
    }
}
