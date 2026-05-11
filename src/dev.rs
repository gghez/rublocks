//! Dev-mode supervisor: watch source files, rebuild, restart child.
//!
//! Lifecycle:
//! 1. Initial codegen + `cargo build` + spawn the dist binary on port 3000.
//! 2. Watch source files under the project (recursive, excluding `dist/`).
//! 3. On a content-changing event: kill child, regen, build, respawn.
//!
//! Failure handling: when any rebuild step fails (manifest parse, codegen,
//! cargo build, child crash on boot), the supervisor binds port 3000 itself
//! and serves the browser-side error overlay (`dev_error::ErrorServer`).
//! That overlay carries the livereload snippet too, so the browser keeps a
//! warm SSE connection and reloads as soon as the user fixes the issue.
//!
//! "Source files" = `*.json` manifests/models/routes/layouts plus `*.html`
//! templates. Both are inputs to codegen, so either changing must rebuild.

use anyhow::{Context, Result};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, new_debouncer};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;

use crate::dev_error::{AppLabel, DevError, ErrorServer, parse_first_cargo_error};
use crate::manifest::{LoadDotenv, Manifest, ManifestError};
use crate::{agents, codegen, dev_services::DevServices, docker, migrations};

/// Run dev mode for the project at `project_dir`.
///
/// Blocks the calling thread for the lifetime of the dev session. Returns
/// only on watcher channel closure (rare); normal shutdown is via `Ctrl+C`,
/// which is handled by a `ctrlc` handler that kills the child and exits.
pub fn run(project_dir: &Path) -> Result<()> {
    let runtime = Runtime::new().context("failed to start tokio runtime for dev supervisor")?;
    let dist_dir = project_dir.join("dist");
    let dist_canon: PathBuf = std::fs::canonicalize(&dist_dir).unwrap_or_else(|_| dist_dir.clone());

    let supervisor = Arc::new(Supervisor::new(
        runtime,
        project_dir.to_path_buf(),
        dist_dir,
    ));
    let cleanup_sup = supervisor.clone();
    ctrlc::set_handler(move || {
        eprintln!("\nrublocks dev: shutting down");
        cleanup_sup.shutdown_blocking();
        std::process::exit(0);
    })
    .context("failed to install Ctrl+C handler")?;

    println!("rublocks dev: initial build");
    supervisor.rebuild_and_run();

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(300), None, tx)
        .context("failed to create file watcher")?;
    debouncer
        .watch(project_dir, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", project_dir.display()))?;

    println!(
        "rublocks dev: watching {} (Ctrl+C to stop)",
        project_dir.display()
    );

    watch_loop(rx, project_dir, &dist_canon, FALLBACK_POLL, || {
        println!("rublocks dev: change detected, rebuilding");
        supervisor.rebuild_and_run();
    });

    Ok(())
}

/// How long to wait between file events before doing a defensive sweep.
///
/// On Linux/WSL2 inotify has a small race window between `mkdir` and the
/// recursive watcher installing a watch on the new directory: files written
/// inside that window produce no event. We rescan the project tree every
/// `FALLBACK_POLL` to catch those misses. The hash dedup keeps the rebuild
/// path from firing when nothing actually changed. See issue #1.
const FALLBACK_POLL: Duration = Duration::from_secs(1);

/// Drive rebuilds from both inotify events (fast path) and a periodic sweep
/// (fallback for missed events). Extracted from `run` so it can be exercised
/// without spawning a real cargo build — see the tests at the bottom of this
/// file.
fn watch_loop<F: FnMut()>(
    rx: Receiver<DebounceEventResult>,
    project_dir: &Path,
    dist_canon: &Path,
    poll_interval: Duration,
    mut on_change: F,
) {
    let mut last_hash = project_sources_hash(project_dir, dist_canon);
    loop {
        let should_check = match rx.recv_timeout(poll_interval) {
            Ok(Ok(events)) => relevant_change(&events, dist_canon),
            Ok(Err(errs)) => {
                for e in errs {
                    eprintln!("rublocks dev: watch error: {e:?}");
                }
                false
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if !should_check {
            continue;
        }
        let new_hash = project_sources_hash(project_dir, dist_canon);
        if new_hash == last_hash {
            continue;
        }
        last_hash = new_hash;
        on_change();
    }
}

/// Owns the dist child process, the fallback error server, and the lazily
/// provisioned dev services. Only one of {child, error server} is active at
/// any time — the supervisor flips between them as rebuilds succeed or fail,
/// so port 3000 always has *someone* answering.
struct Supervisor {
    runtime: Runtime,
    project_dir: PathBuf,
    dist_dir: PathBuf,
    state: Mutex<SupervisorState>,
}

struct SupervisorState {
    child: Option<Child>,
    error_server: Option<ErrorServer>,
    services: Option<DevServices>,
    /// Last successful manifest's name + version. Threaded into the
    /// `ErrorServer` so the overlay footer can render `{name} v{version}`
    /// even when the current rebuild's manifest failed to parse. See issue #15.
    last_label: Option<AppLabel>,
    /// Project synopsis from the last successful manifest load. Carried over
    /// into the dev-mode error overlay so the page subtitle still names the
    /// project even when the rebuild failed (the only times the manifest is
    /// unavailable are first-load failures, where it would be misleading to
    /// invent a description anyway).
    last_description: Option<String>,
    /// Last manifest-declared language tag. The error overlay uses it for
    /// localized strings + `Content-Language`; before any manifest parses
    /// successfully we fall back to `"en-US"` so the very-first manifest
    /// error still renders.
    last_language: String,
}

impl Supervisor {
    fn new(runtime: Runtime, project_dir: PathBuf, dist_dir: PathBuf) -> Self {
        Self {
            runtime,
            project_dir,
            dist_dir,
            state: Mutex::new(SupervisorState {
                child: None,
                error_server: None,
                services: None,
                last_label: None,
                last_description: None,
                last_language: "en-US".to_string(),
            }),
        }
    }

    fn rebuild_and_run(&self) {
        // Always tear down the current child first; it owns port 3000.
        self.kill_child();

        let outcome = self
            .try_rebuild()
            .and_then(|manifest| {
                // Merge dotenv-sourced vars into the child's env BEFORE the
                // dev-services overlay, so a Postgres URL provisioned by the
                // dev orchestrator (issue #11) still wins over whatever the
                // user's `.env` happened to set. Order: dotenv → services.
                let dotenv_env = collect_dotenv_env(&self.project_dir, &manifest.load_dotenv)?;
                let svc_env = self.ensure_services(&manifest)?;
                let mut env = dotenv_env;
                env.extend(svc_env);
                Ok((manifest, env))
            })
            .and_then(|(manifest, env)| {
                spawn_app(&self.dist_dir, &manifest.name, &env).map_err(|e| DevError::Runtime {
                    message: format!("{e:?}"),
                })
            });

        match outcome {
            Ok(child) => {
                // Free port 3000 before storing the new child, in case its
                // bind races the error server's listener.
                self.shutdown_error_server();
                self.state.lock().unwrap().child = Some(child);
            }
            Err(dev_err) => {
                eprintln!("rublocks dev: {}", short_label(&dev_err));
                self.swap_error_server(dev_err);
            }
        }
    }

    /// Cache the project name + version from a successful manifest load so
    /// any future failure overlay can stamp `{name} v{version}` in its
    /// footer (issue #15).
    fn remember_label(&self, manifest: &Manifest) {
        self.state.lock().unwrap().last_label = Some(AppLabel {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
        });
    }

    fn try_rebuild(&self) -> std::result::Result<Manifest, DevError> {
        let manifest = Manifest::load(&self.project_dir).map_err(manifest_error_to_dev)?;
        // Cache the project label + synopsis + language as soon as the manifest
        // parses, even before codegen runs — so a subsequent codegen/build
        // failure still produces an overlay stamped with the current build
        // (issues #14 + #15 + #16).
        self.remember_label(&manifest);
        {
            let mut state = self.state.lock().unwrap();
            state.last_description = Some(manifest.description.clone());
            state.last_language = manifest.language.clone();
        }
        // Migrations are generated BEFORE codegen so codegen can wire
        // `sqlx::migrate!` against the migration set the dist binary will
        // ship with. Mirroring runs after codegen (which wipes dist/).
        let db_kind = manifest
            .database
            .as_ref()
            .map(|d| d.kind)
            .unwrap_or_default();
        if let Some(emitted) = migrations::generate(&self.project_dir, &manifest.models, db_kind)
            .map_err(|e| DevError::Codegen {
                message: format!("{e:?}"),
            })?
        {
            eprintln!("rublocks dev: wrote migration {}", emitted.path.display());
        }
        codegen::emit(&manifest, &self.project_dir, &self.dist_dir).map_err(|e| {
            DevError::Codegen {
                message: format!("{e:?}"),
            }
        })?;
        migrations::mirror(&self.project_dir, &self.dist_dir).map_err(|e| DevError::Codegen {
            message: format!("{e:?}"),
        })?;
        docker::emit(&manifest, &self.dist_dir).map_err(|e| DevError::Codegen {
            message: format!("{e:?}"),
        })?;
        // Keep per-agent integration files in sync with the binary on every
        // rebuild — authoring through `rublocks dev` should not leave the
        // project's SKILL.md / AGENTS.md / cursor rule stale vs. the build.
        agents::write_all(&self.project_dir).map_err(|e| DevError::Codegen {
            message: format!("{e:?}"),
        })?;
        run_cargo_build(&self.dist_dir)?;
        Ok(manifest)
    }

    /// Provision dev services on the first successful manifest load and
    /// reuse them for the rest of the session. Postgres/redis containers
    /// keep their data across restarts, so re-provisioning would be wasted
    /// work and could even leak Docker resources.
    fn ensure_services(
        &self,
        manifest: &Manifest,
    ) -> std::result::Result<Vec<(String, String)>, DevError> {
        let mut state = self.state.lock().unwrap();
        if let Some(svc) = state.services.as_ref() {
            return Ok(svc.env.clone());
        }
        match DevServices::provision(manifest) {
            Ok(svc) => {
                let env = svc.env.clone();
                state.services = Some(svc);
                Ok(env)
            }
            Err(e) => Err(DevError::Services {
                message: format!("{e:?}"),
            }),
        }
    }

    fn kill_child(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(mut c) = state.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn swap_error_server(&self, error: DevError) {
        self.shutdown_error_server();
        let (label, description, language) = {
            let state = self.state.lock().unwrap();
            (
                state.last_label.clone(),
                state.last_description.clone(),
                state.last_language.clone(),
            )
        };
        let server = self
            .runtime
            .block_on(ErrorServer::spawn(error, label, description, language));
        match server {
            Ok(srv) => {
                self.state.lock().unwrap().error_server = Some(srv);
            }
            Err(e) => eprintln!("rublocks dev: failed to start error overlay server: {e:?}"),
        }
    }

    fn shutdown_error_server(&self) {
        let prev = self.state.lock().unwrap().error_server.take();
        if let Some(srv) = prev {
            self.runtime.block_on(srv.shutdown());
        }
    }

    fn shutdown_blocking(&self) {
        self.kill_child();
        self.shutdown_error_server();
        if let Some(svc) = self.state.lock().unwrap().services.take() {
            // DevServices::shutdown takes &mut self, but the Mutex guard is
            // already dropped — we own `svc` outright here.
            let mut svc = svc;
            svc.shutdown();
        }
    }
}

/// Capture cargo's stdout+stderr so we can render them in the browser overlay,
/// while still streaming progress to the supervisor's terminal in real time.
fn run_cargo_build(dist_dir: &Path) -> std::result::Result<(), DevError> {
    let output = Command::new("cargo")
        .arg("build")
        .arg("--color=never")
        .current_dir(dist_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| DevError::Build {
            stderr: format!("failed to invoke cargo: {e}"),
            first_error: None,
        })?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Mirror stderr to the supervisor's terminal so the user still sees the
    // diagnostic alongside the browser overlay.
    eprint!("{stderr}");
    if output.status.success() {
        Ok(())
    } else {
        let first_error = parse_first_cargo_error(&stderr);
        Err(DevError::Build {
            stderr,
            first_error,
        })
    }
}

/// Parse the manifest-declared `.env` into `(key, value)` pairs ready to be
/// layered on the child's environment.
///
/// Mirrors what the generated binary's `dotenvy::from_path(...).ok()` call
/// would do at startup, with two differences:
///
/// - The supervisor's own `std::env` is NEVER mutated. Polluting the host
///   process would leak across rebuilds and contaminate codegen-time tools
///   that legitimately read env vars (none today, but the invariant is
///   cheap to preserve).
/// - An explicit `LoadDotenv::Path` that does not exist surfaces as a
///   `DevError::Manifest` so the dev overlay points the user at
///   `main.json`. `LoadDotenv::Auto` keeps the dist binary's silent
///   semantics — a missing `.env` next to `main.json` is the implicit-
///   discovery case and means "no dotenv this time".
fn collect_dotenv_env(
    project_dir: &Path,
    policy: &LoadDotenv,
) -> std::result::Result<Vec<(String, String)>, DevError> {
    let path = match policy {
        LoadDotenv::Disabled => return Ok(Vec::new()),
        LoadDotenv::Auto => {
            let default = project_dir.join(".env");
            if !default.is_file() {
                return Ok(Vec::new());
            }
            default
        }
        LoadDotenv::Path(p) => {
            if !p.is_file() {
                return Err(DevError::Manifest {
                    file: Some(project_dir.join("main.json")),
                    message: format!(
                        "`load_dotenv` points at `{}` but no file exists there",
                        p.display()
                    ),
                    line: None,
                    column: None,
                    snippet: None,
                });
            }
            p.clone()
        }
    };
    let mut out = Vec::new();
    let iter = dotenvy::from_path_iter(&path).map_err(|e| DevError::Manifest {
        file: Some(project_dir.join("main.json")),
        message: format!("failed to read `.env` at `{}`: {e}", path.display()),
        line: None,
        column: None,
        snippet: None,
    })?;
    for item in iter {
        let (k, v) = item.map_err(|e| DevError::Manifest {
            file: Some(project_dir.join("main.json")),
            message: format!("invalid `.env` entry in `{}`: {e}", path.display()),
            line: None,
            column: None,
            snippet: None,
        })?;
        out.push((k, v));
    }
    Ok(out)
}

/// Spawn the freshly built dist binary. Inherits stdio so the user still
/// sees `println!`s, panics, etc. in their terminal.
fn spawn_app(dist_dir: &Path, app_name: &str, extra_env: &[(String, String)]) -> Result<Child> {
    let binary = dist_dir.join("target").join("debug").join(app_name);
    let mut cmd = Command::new(&binary);
    cmd.env("RUBLOCKS_DEV", "1");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn()
        .with_context(|| format!("failed to spawn {}", binary.display()))
}

/// Map a typed `ManifestError` from the loaders into a `DevError::Manifest`.
///
/// Every `ManifestError` already carries the offending file path, so the
/// dev overlay never has to guess. Line/column come along when the underlying
/// failure was a JSON syntax error; for shape/validation issues they are
/// `None` and the snippet pane is omitted. See issue #2.
fn manifest_error_to_dev(err: ManifestError) -> DevError {
    let snippet = err.line.and_then(|line| {
        std::fs::read_to_string(&err.file)
            .ok()
            .and_then(|content| extract_snippet(&content, line))
    });
    DevError::Manifest {
        file: Some(err.file),
        message: err.message,
        line: err.line,
        column: err.column,
        snippet,
    }
}

/// Slice up to `±3` lines around `line` for the overlay snippet pane.
fn extract_snippet(content: &str, line: usize) -> Option<String> {
    if line == 0 {
        return None;
    }
    let zero_based = line - 1;
    let start = zero_based.saturating_sub(3);
    let end = zero_based + 3;
    let lines: Vec<&str> = content.lines().collect();
    if start >= lines.len() {
        return None;
    }
    let stop = end.min(lines.len() - 1);
    let width = (stop + 1).to_string().len();
    let snippet = (start..=stop)
        .map(|i| {
            let marker = if i == zero_based { ">" } else { " " };
            format!("{marker} {:>width$} | {}", i + 1, lines[i], width = width)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(snippet)
}

/// Short single-line label for terminal logging (`rublocks dev: <label>`).
fn short_label(err: &DevError) -> String {
    match err {
        DevError::Manifest { file, line, .. } => {
            let where_ = file
                .as_ref()
                .map(|f| f.display().to_string())
                .unwrap_or_else(|| "<unknown file>".to_string());
            match line {
                Some(l) => format!("manifest error at {where_}:{l}"),
                None => format!("manifest error at {where_}"),
            }
        }
        DevError::Codegen { .. } => "codegen error".to_string(),
        DevError::Build { first_error, .. } => match first_error {
            Some(e) => match (&e.file, e.line) {
                (Some(f), Some(l)) => format!("build error at {}:{}", f.display(), l),
                _ => format!("build error: {}", e.message),
            },
            None => "build error".to_string(),
        },
        DevError::Services { .. } => "services error".to_string(),
        DevError::Runtime { .. } => "runtime error".to_string(),
    }
}

/// Cheap pre-filter: does any event touch a watched source file outside `dist/`?
///
/// This lets us skip the project-wide hash recomputation when cargo's churn
/// inside `dist/target/` (which can include `.json` fingerprint files) is the
/// only thing that fired.
fn relevant_change(events: &[DebouncedEvent], dist_canon: &Path) -> bool {
    events.iter().any(|e| {
        e.event
            .paths
            .iter()
            .any(|p| is_source(p) && !p.starts_with(dist_canon))
    })
}

/// Extensions that count as project source: codegen reads them, so a change
/// must trigger a rebuild.
const SOURCE_EXTS: &[&str] = &["json", "html"];

fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|ext| SOURCE_EXTS.contains(&ext))
}

/// Hash every watched source file in the project.
///
/// Content-based dedup, intentionally not mtime-based: WSL2's inotify emits
/// repeated phantom events for a single edit (sometimes across multiple
/// debounce windows), and mtime would change on every write even when the
/// content is identical. See `docs/dev-mode.md#content-hash-dedup`.
fn project_sources_hash(project_dir: &Path, dist_canon: &Path) -> u64 {
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    collect_sources(project_dir, dist_canon, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = DefaultHasher::new();
    for (path, bytes) in &files {
        path.to_string_lossy().hash(&mut hasher);
        bytes.hash(&mut hasher);
    }
    hasher.finish()
}

/// Walk `dir` recursively, collecting every watched source file outside `exclude`.
///
/// Errors are silently skipped — directories that disappear mid-walk (cargo
/// rotating fingerprint dirs) shouldn't crash the dev loop.
fn collect_sources(dir: &Path, exclude: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.starts_with(exclude) {
            continue;
        }
        if path.is_dir() {
            collect_sources(&path, exclude, out);
        } else if is_source(&path)
            && let Ok(bytes) = std::fs::read(&path)
        {
            out.push((path, bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn is_source_matches_json_and_html() {
        assert!(is_source(Path::new("a.json")));
        assert!(is_source(Path::new("a.html")));
        assert!(is_source(Path::new("dir/b.json")));
        assert!(!is_source(Path::new("a.txt")));
        assert!(!is_source(Path::new("a.rs")));
        assert!(!is_source(Path::new("Cargo.toml")));
        assert!(!is_source(Path::new("no_extension")));
    }

    #[test]
    fn extract_snippet_centers_the_marker() {
        let content = "line1\nline2\nline3\nline4\nline5\n";
        let snippet = extract_snippet(content, 3).unwrap();
        // The marker `>` must be on the targeted line.
        let marked_line = snippet
            .lines()
            .find(|l| l.starts_with(">"))
            .expect("marker present");
        assert!(marked_line.contains("line3"));
    }

    #[test]
    fn extract_snippet_handles_line_one() {
        let content = "first\nsecond\nthird";
        let snippet = extract_snippet(content, 1).unwrap();
        assert!(snippet.lines().next().unwrap().contains("first"));
    }

    #[test]
    fn extract_snippet_returns_none_for_zero_or_overflow() {
        assert!(extract_snippet("a\nb", 0).is_none());
        assert!(extract_snippet("a\nb", 9999).is_none());
    }

    #[test]
    fn manifest_error_to_dev_propagates_file_and_position() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("main.json");
        fs::write(&file, "{\n  \"name\": \"x\"\n").unwrap(); // missing closing brace
        let parse_err =
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&file).unwrap())
                .unwrap_err();
        let manifest_err = crate::manifest::ManifestError::parse(&file, parse_err);
        let dev_err = manifest_error_to_dev(manifest_err);
        match dev_err {
            DevError::Manifest {
                file: f,
                line,
                column,
                snippet,
                ..
            } => {
                assert_eq!(f.unwrap(), file);
                assert!(line.is_some());
                assert!(column.is_some());
                assert!(
                    snippet.is_some(),
                    "snippet should be extracted from the file content"
                );
            }
            other => panic!("expected DevError::Manifest, got {other:?}"),
        }
    }

    #[test]
    fn manifest_error_to_dev_handles_validation_without_snippet() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("main.json");
        fs::write(&file, r#"{"name":"x"}"#).unwrap();
        let manifest_err = crate::manifest::ManifestError::validation(&file, "bad name");
        let dev_err = manifest_error_to_dev(manifest_err);
        match dev_err {
            DevError::Manifest {
                file: f,
                line,
                column,
                snippet,
                message,
            } => {
                assert_eq!(f.unwrap(), file);
                assert!(line.is_none());
                assert!(column.is_none());
                assert!(snippet.is_none());
                assert_eq!(message, "bad name");
            }
            other => panic!("expected DevError::Manifest, got {other:?}"),
        }
    }

    /// Regression for issue #1: a file dropped into a subdirectory created
    /// after the watcher armed must still trigger `on_change`, even when
    /// inotify never delivered an event for it.
    ///
    /// We simulate the "inotify missed it" condition by giving the loop a
    /// channel that never receives anything — only the periodic fallback can
    /// detect the change. The loop must call `on_change` within a couple of
    /// poll intervals.
    #[test]
    fn watch_loop_detects_files_in_freshly_created_subdir() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;
        use std::thread;

        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.json"), "{\"name\":\"x\"}").unwrap();

        let dist_canon = dir.path().join("dist");
        let project = dir.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();

        let (tx, rx) = mpsc::channel::<DebounceEventResult>();
        let handle = std::thread::spawn(move || {
            watch_loop(
                rx,
                &project,
                &dist_canon,
                Duration::from_millis(100),
                move || {
                    calls_clone.fetch_add(1, Ordering::SeqCst);
                },
            );
        });

        // Simulate the bug's exact repro: mkdir + write inside, with no
        // event delivered through the channel.
        std::thread::sleep(Duration::from_millis(150));
        let routes = dir.path().join("routes");
        fs::create_dir_all(&routes).unwrap();
        fs::write(
            routes.join("home.json"),
            "{\"path\":\"/\",\"method\":\"GET\",\"kind\":\"page\",\"template\":\"x.html\"}",
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && calls.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(50));
        }

        drop(tx);
        handle.join().unwrap();

        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "expected the fallback sweep to detect the file dropped into the new subdir"
        );
    }

    /// The fallback sweep must not flap when nothing changes — otherwise
    /// every idle second would trigger a wasted rebuild.
    #[test]
    fn watch_loop_does_not_fire_when_idle() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.json"), "{\"name\":\"x\"}").unwrap();

        let dist_canon = dir.path().join("dist");
        let project = dir.path().to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();

        let (tx, rx) = mpsc::channel::<DebounceEventResult>();
        let handle = std::thread::spawn(move || {
            watch_loop(
                rx,
                &project,
                &dist_canon,
                Duration::from_millis(50),
                move || {
                    calls_clone.fetch_add(1, Ordering::SeqCst);
                },
            );
        });

        std::thread::sleep(Duration::from_millis(400));
        drop(tx);
        handle.join().unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no files changed — on_change must not be invoked by the idle sweep"
        );
    }
}
