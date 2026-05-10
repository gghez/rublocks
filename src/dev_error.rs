//! Browser-side error overlay for dev mode.
//!
//! When codegen, manifest parsing, or `cargo build` fails, the supervisor
//! takes over port 3000 with this tiny axum server and renders a clear,
//! actionable HTML page. The same page embeds the livereload snippet, so
//! when the underlying issue is fixed and the child boots, the browser
//! reconnects and reloads itself. See `docs/dev-mode.md`.

use anyhow::Result;
use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive},
        Html, IntoResponse, Sse,
    },
    routing::{any, get},
    Router,
};
use futures_util::stream;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// One classified dev-mode failure, with enough structure to render a useful
/// page. Each variant captures whatever the upstream tool could give us;
/// missing fields just render as "unknown" rather than blocking the overlay.
#[derive(Debug, Clone)]
pub enum DevError {
    Manifest {
        file: Option<PathBuf>,
        message: String,
        line: Option<usize>,
        column: Option<usize>,
        snippet: Option<String>,
    },
    Codegen {
        message: String,
    },
    Build {
        stderr: String,
        first_error: Option<CargoError>,
    },
    Services {
        message: String,
    },
    Runtime {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct CargoError {
    pub file: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
    pub code: Option<String>,
}

/// Running fallback server. Hold onto this for the lifetime of the error
/// state; drop or call `shutdown` to release port 3000 before respawning
/// the dist child.
pub struct ErrorServer {
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl ErrorServer {
    /// Bind 0.0.0.0:3000 and serve `error` at every path. The future
    /// returned by `axum::serve` runs until `shutdown` is invoked.
    pub async fn spawn(error: DevError) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
        let state = Arc::new(error);
        let app = Router::new()
            .route("/__rublocks/livereload.js", get(livereload_js))
            .route("/__rublocks/events", get(sse_events))
            .fallback(any(error_overlay))
            .with_state(state);
        let (tx, rx) = oneshot::channel();
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(ErrorServer {
            shutdown: Some(tx),
            join,
        })
    }

    /// Send the shutdown signal and wait for the listener to release the port.
    /// Required before respawning the dist child, which itself binds 3000.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.join).await;
    }
}

async fn error_overlay(State(state): State<Arc<DevError>>) -> impl IntoResponse {
    Html(render_error_html(&state))
}

async fn livereload_js() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        LIVERELOAD_JS,
    )
}

async fn sse_events() -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    // The error server never emits a payload — the SSE connection itself is
    // the heartbeat. When the supervisor shuts this server down (because the
    // build succeeded), the stream ends, the browser reconnects to the now-
    // healthy child, sees `onopen` with `everConnected=true`, and reloads.
    Sse::new(stream::pending::<Result<Event, Infallible>>()).keep_alive(KeepAlive::default())
}

/// Plain-text payload designed to be pasted into an agent chat. Carries the
/// same information as the overlay body but in a single Markdown block so
/// the user doesn't have to retype anything when delegating the fix.
fn render_payload(error: &DevError) -> String {
    let mut out = String::new();
    match error {
        DevError::Manifest {
            file,
            message,
            line,
            column,
            snippet,
        } => {
            out.push_str("rublocks dev — Manifest error\n\n");
            if let Some(f) = file {
                out.push_str(&format!("file: {}\n", f.display()));
            }
            if let Some(l) = line {
                let pos = match column {
                    Some(c) => format!("{l}:{c}"),
                    None => l.to_string(),
                };
                out.push_str(&format!("at: {pos}\n"));
            }
            out.push('\n');
            if let Some(s) = snippet {
                out.push_str(s);
                out.push_str("\n\n");
            }
            out.push_str(message);
            out.push('\n');
        }
        DevError::Codegen { message } => {
            out.push_str("rublocks dev — Codegen error\n\n");
            out.push_str(message);
            out.push('\n');
        }
        DevError::Services { message } => {
            out.push_str("rublocks dev — Services error\n\n");
            out.push_str(message);
            out.push('\n');
        }
        DevError::Build {
            stderr,
            first_error,
        } => {
            out.push_str("rublocks dev — Build error\n\n");
            if let Some(err) = first_error {
                if let Some(code) = &err.code {
                    out.push_str(&format!("code: {code}\n"));
                }
                if let Some(file) = &err.file {
                    out.push_str(&format!("file: {}\n", file.display()));
                }
                if let Some(line) = err.line {
                    let pos = match err.column {
                        Some(c) => format!("{line}:{c}"),
                        None => line.to_string(),
                    };
                    out.push_str(&format!("at: {pos}\n"));
                }
                out.push_str(&format!("\nmessage: {}\n\n", err.message));
            }
            out.push_str("cargo output:\n");
            out.push_str(stderr);
            if !stderr.ends_with('\n') {
                out.push('\n');
            }
        }
        DevError::Runtime { message } => {
            out.push_str("rublocks dev — Runtime error\n\n");
            out.push_str(message);
            out.push('\n');
        }
    }
    out
}

fn render_error_html(error: &DevError) -> String {
    let payload = render_payload(error);
    let (title, category, body) = match error {
        DevError::Manifest {
            file,
            message,
            line,
            column,
            snippet,
        } => (
            "Manifest error",
            "MANIFEST",
            render_manifest_body(file.as_ref(), message, *line, *column, snippet.as_deref()),
        ),
        DevError::Codegen { message } => (
            "Codegen error",
            "CODEGEN",
            format!(
                "<p class=\"hint\">rublocks could not produce a valid Rust project from the manifest. \
                 The message below comes straight from the compiler.</p>\
                 <pre class=\"trace\">{}</pre>",
                escape_html(message)
            ),
        ),
        DevError::Services { message } => (
            "Services error",
            "SERVICES",
            format!(
                "<p class=\"hint\">A declared service could not be provisioned. \
                 Check that Docker is running, or export the env var the manifest references.</p>\
                 <pre class=\"trace\">{}</pre>",
                escape_html(message)
            ),
        ),
        DevError::Build {
            stderr,
            first_error,
        } => (
            "Build error",
            "CARGO BUILD",
            render_build_body(stderr, first_error.as_ref()),
        ),
        DevError::Runtime { message } => (
            "Runtime error",
            "RUNTIME",
            format!(
                "<p class=\"hint\">The dist binary exited before serving requests. The captured output is below.</p>\
                 <pre class=\"trace\">{}</pre>",
                escape_html(message)
            ),
        ),
    };

    format!(
        "<!doctype html>\n<html lang=\"en\"><head>\
         <meta charset=\"utf-8\">\
         <title>rublocks — {title}</title>\
         <script src=\"/__rublocks/livereload.js\"></script>\
         <style>{css}</style>\
         </head><body>\
         <header>\
           <div class=\"row\">\
             <span class=\"badge\">{category}</span>\
             <button id=\"copy-btn\" type=\"button\" data-payload=\"{payload}\">\
               Copy for agent\
             </button>\
           </div>\
           <h1>{title}</h1>\
         </header>\
         <main>{body}</main>\
         <footer>rublocks dev mode · this page auto-reloads when the issue is fixed</footer>\
         <script>{copy_js}</script>\
         </body></html>",
        title = escape_html(title),
        category = escape_html(category),
        body = body,
        css = OVERLAY_CSS,
        payload = escape_html(&payload),
        copy_js = COPY_JS,
    )
}

fn render_manifest_body(
    file: Option<&PathBuf>,
    message: &str,
    line: Option<usize>,
    column: Option<usize>,
    snippet: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "<p class=\"hint\">A declarative file failed to parse. \
         Fix the syntax / shape and save — the page will reload automatically.</p>"
    ));
    out.push_str("<dl>");
    if let Some(f) = file {
        out.push_str(&format!(
            "<dt>file</dt><dd><code>{}</code></dd>",
            escape_html(&f.display().to_string())
        ));
    }
    if let Some(l) = line {
        let pos = match column {
            Some(c) => format!("{l}:{c}"),
            None => l.to_string(),
        };
        out.push_str(&format!("<dt>at</dt><dd>{}</dd>", escape_html(&pos)));
    }
    out.push_str("</dl>");
    if let Some(snip) = snippet {
        out.push_str(&format!(
            "<pre class=\"snippet\">{}</pre>",
            escape_html(snip)
        ));
    }
    out.push_str(&format!(
        "<pre class=\"trace\">{}</pre>",
        escape_html(message)
    ));
    out
}

fn render_build_body(stderr: &str, first: Option<&CargoError>) -> String {
    let mut out = String::new();
    out.push_str(
        "<p class=\"hint\">The generated Rust project failed to compile. \
         The first cargo diagnostic is summarized below; the full output follows.</p>",
    );
    if let Some(err) = first {
        out.push_str("<dl>");
        if let Some(code) = &err.code {
            out.push_str(&format!(
                "<dt>code</dt><dd><code>{}</code></dd>",
                escape_html(code)
            ));
        }
        if let Some(file) = &err.file {
            out.push_str(&format!(
                "<dt>file</dt><dd><code>{}</code></dd>",
                escape_html(&file.display().to_string())
            ));
        }
        if let Some(line) = err.line {
            let pos = match err.column {
                Some(c) => format!("{line}:{c}"),
                None => line.to_string(),
            };
            out.push_str(&format!("<dt>at</dt><dd>{}</dd>", escape_html(&pos)));
        }
        out.push_str(&format!(
            "<dt>message</dt><dd>{}</dd></dl>",
            escape_html(&err.message)
        ));
    }
    out.push_str(&format!(
        "<details open><summary>cargo output</summary><pre class=\"trace\">{}</pre></details>",
        escape_html(stderr)
    ));
    out
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Parse cargo build output and pull out the first `error[E0xxx]:` block.
/// Best-effort — when the format is unfamiliar we just return `None` and
/// the overlay falls back to showing the raw cargo dump.
pub fn parse_first_cargo_error(stderr: &str) -> Option<CargoError> {
    let mut lines = stderr.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("error[")
            .or_else(|| trimmed.strip_prefix("error: "))
        else {
            continue;
        };
        let (code, message) = if let Some(idx) = rest.find("]: ") {
            (Some(rest[..idx].to_string()), rest[idx + 3..].to_string())
        } else {
            (None, rest.to_string())
        };
        // Next ` --> ` line carries file:line:column.
        let mut file = None;
        let mut line_no = None;
        let mut col_no = None;
        for follow in lines.by_ref() {
            let t = follow.trim_start();
            if let Some(loc) = t.strip_prefix("--> ") {
                let mut parts = loc.splitn(3, ':');
                file = parts.next().map(PathBuf::from);
                line_no = parts.next().and_then(|s| s.parse().ok());
                col_no = parts.next().and_then(|s| s.trim().parse().ok());
                break;
            }
            if t.starts_with("error") || t.starts_with("warning") {
                break;
            }
        }
        return Some(CargoError {
            file,
            line: line_no,
            column: col_no,
            message,
            code,
        });
    }
    None
}

const OVERLAY_CSS: &str = r#"
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body {
  margin: 0;
  font: 14px/1.5 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
  background: #1a1a1d;
  color: #e6e6e6;
}
header {
  padding: 1.5rem 2rem 0.5rem;
  border-bottom: 1px solid #2c2c30;
}
header .row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
}
header h1 {
  margin: 0.5rem 0 1rem;
  font-size: 1.5rem;
  font-weight: 600;
}
#copy-btn {
  background: #2a2a30;
  color: #e6e6e6;
  border: 1px solid #3c3c44;
  padding: 0.4rem 0.9rem;
  border-radius: 0.3rem;
  font: inherit;
  font-size: 0.875rem;
  cursor: pointer;
  transition: background 0.1s, color 0.1s;
}
#copy-btn:hover { background: #34343c; }
#copy-btn.ok { background: #2d6e3e; border-color: #3a8550; color: white; }
#copy-btn.err { background: #6e2d2d; border-color: #853a3a; color: white; }
.badge {
  display: inline-block;
  padding: 0.15rem 0.5rem;
  background: #b03434;
  color: white;
  font-size: 0.75rem;
  font-weight: 700;
  letter-spacing: 0.05em;
  border-radius: 0.2rem;
}
main { padding: 1.5rem 2rem; max-width: 70rem; }
.hint { margin: 0 0 1.5rem; color: #b8b8b8; }
dl {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 0.25rem 1rem;
  margin: 0 0 1rem;
}
dt { color: #8c8c92; font-weight: 600; }
dd { margin: 0; }
code, pre {
  font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
}
pre {
  background: #111114;
  color: #e6e6e6;
  padding: 1rem;
  border-radius: 0.4rem;
  overflow: auto;
  margin: 0.5rem 0;
}
pre.snippet { border-left: 3px solid #b03434; }
pre.trace { font-size: 12.5px; }
details { margin-top: 1.5rem; }
summary { cursor: pointer; color: #8c8c92; }
footer {
  padding: 1.5rem 2rem;
  color: #6a6a70;
  border-top: 1px solid #2c2c30;
  font-size: 12px;
}
"#;

/// Tiny script attached to the "Copy for agent" button. Reads the payload
/// from the button's `data-payload` attribute and writes it to the
/// clipboard, with visual feedback on success or failure.
const COPY_JS: &str = r#"(function () {
  const btn = document.getElementById('copy-btn');
  if (!btn) return;
  const original = btn.textContent;
  btn.addEventListener('click', async function () {
    const raw = btn.getAttribute('data-payload') || '';
    // Inject the URL the browser actually requested — server-side we don't
    // know it (the overlay catches every path with the same fallback).
    const url = window.location.href;
    const payload = raw.replace(/^(rublocks dev — [^\n]+\n)\n/, `$1\nurl: ${url}\n`);
    try {
      await navigator.clipboard.writeText(payload);
      btn.textContent = 'Copied';
      btn.classList.add('ok');
    } catch (e) {
      btn.textContent = 'Copy failed — select the trace manually';
      btn.classList.add('err');
    }
    setTimeout(function () {
      btn.textContent = original;
      btn.classList.remove('ok');
      btn.classList.remove('err');
    }, 2000);
  });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_handles_specials() {
        assert_eq!(escape_html("a<b>c"), "a&lt;b&gt;c");
        assert_eq!(escape_html("a & b"), "a &amp; b");
        assert_eq!(escape_html("a\"b'c"), "a&quot;b&#39;c");
        assert_eq!(escape_html("plain"), "plain");
    }

    #[test]
    fn parse_first_cargo_error_extracts_code_and_location() {
        let stderr = "\
   Compiling foo v0.1.0
error[E0277]: the trait bound is not satisfied
   --> src/main.rs:12:5
    |
12  |     bad();
    |     ^^^^^
";
        let parsed = parse_first_cargo_error(stderr).expect("should parse");
        assert_eq!(parsed.code.as_deref(), Some("E0277"));
        assert_eq!(parsed.file.as_deref(), Some(std::path::Path::new("src/main.rs")));
        assert_eq!(parsed.line, Some(12));
        assert_eq!(parsed.column, Some(5));
        assert!(parsed.message.contains("trait bound"));
    }

    #[test]
    fn parse_first_cargo_error_handles_codeless_errors() {
        let stderr = "error: linking failed\n";
        let parsed = parse_first_cargo_error(stderr).expect("should parse");
        assert!(parsed.code.is_none());
        assert!(parsed.file.is_none());
        assert!(parsed.message.contains("linking failed"));
    }

    #[test]
    fn parse_first_cargo_error_returns_none_when_clean() {
        assert!(parse_first_cargo_error("    Finished\n").is_none());
        assert!(parse_first_cargo_error("").is_none());
    }

    #[test]
    fn render_payload_manifest_includes_file_and_position() {
        let err = DevError::Manifest {
            file: Some(std::path::PathBuf::from("/p/main.json")),
            message: "boom".to_string(),
            line: Some(3),
            column: Some(5),
            snippet: Some("> 3 | { broken }".to_string()),
        };
        let payload = render_payload(&err);
        assert!(payload.starts_with("rublocks dev — Manifest error\n\n"));
        assert!(payload.contains("file: /p/main.json"));
        assert!(payload.contains("at: 3:5"));
        assert!(payload.contains("> 3 | { broken }"));
        assert!(payload.contains("boom"));
    }

    #[test]
    fn render_payload_build_includes_first_error_metadata() {
        let err = DevError::Build {
            stderr: "raw cargo dump".to_string(),
            first_error: Some(CargoError {
                file: Some(std::path::PathBuf::from("src/x.rs")),
                line: Some(7),
                column: Some(1),
                message: "kaboom".to_string(),
                code: Some("E0001".to_string()),
            }),
        };
        let payload = render_payload(&err);
        assert!(payload.contains("code: E0001"));
        assert!(payload.contains("file: src/x.rs"));
        assert!(payload.contains("at: 7:1"));
        assert!(payload.contains("message: kaboom"));
        assert!(payload.contains("cargo output:\nraw cargo dump"));
    }

    /// Acceptance criterion for issue #2: the `Copy for agent` payload must
    /// carry the same file path the overlay surfaces.
    #[test]
    fn render_payload_and_html_agree_on_manifest_file() {
        let err = DevError::Manifest {
            file: Some(std::path::PathBuf::from("/p/routes/home.json")),
            message: "bad shape".to_string(),
            line: None,
            column: None,
            snippet: None,
        };
        let payload = render_payload(&err);
        let html = render_error_html(&err);
        assert!(payload.contains("file: /p/routes/home.json"));
        assert!(html.contains("/p/routes/home.json"));
    }

    #[test]
    fn render_payload_omits_unknown_manifest_file() {
        let err = DevError::Manifest {
            file: None,
            message: "shape error".to_string(),
            line: None,
            column: None,
            snippet: None,
        };
        let payload = render_payload(&err);
        assert!(!payload.contains("file:"));
        assert!(payload.contains("shape error"));
    }
}

/// Browser-side livereload snippet served alongside the error overlay.
///
/// Matches the protocol the dist binary uses when no overlay is up: open an
/// `EventSource`, record `everConnected=true` on first connect, reload on
/// reconnect. Both servers serve the SAME script so transitions between
/// "child healthy" and "error overlay" never confuse the client.
pub const LIVERELOAD_JS: &str = r#"(function () {
  let everConnected = false;
  function connect() {
    const es = new EventSource('/__rublocks/events');
    es.onopen = function () {
      if (everConnected) {
        location.reload();
      }
      everConnected = true;
    };
    es.onerror = function () {
      es.close();
      setTimeout(connect, 500);
    };
  }
  connect();
})();
"#;
