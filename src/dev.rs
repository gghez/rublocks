//! Dev-mode supervisor: watch source files, rebuild, restart child.
//!
//! Lifecycle:
//! 1. Initial codegen + `cargo build` + spawn the dist binary.
//! 2. Watch source files under the project (recursive, excluding `dist/`).
//! 3. On a content-changing event: kill child, regen, build, respawn.
//!
//! "Source files" = `*.json` manifests/models/routes/layouts plus `*.html`
//! templates. Both are inputs to codegen, so either changing must rebuild.
//!
//! Browser livereload is handled by the dist binary itself (dev-only routes
//! mounted when `RUBLOCKS_DEV=1`). See `docs/dev-mode.md` for the full
//! protocol description.

use anyhow::{Context, Result};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebouncedEvent};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{codegen, dev_services::DevServices, manifest::Manifest};

/// Run dev mode for the project at `project_dir`.
///
/// Blocks the calling thread for the lifetime of the dev session. Returns
/// only on watcher channel closure (rare); normal shutdown is via `Ctrl+C`,
/// which is handled by a `ctrlc` handler that kills the child and exits.
pub fn run(project_dir: &Path) -> Result<()> {
    let dist_dir = project_dir.join("dist");

    println!("rublocks dev: initial build");
    let manifest = Manifest::load(project_dir)?;

    let services = Arc::new(Mutex::new(DevServices::provision(&manifest)?));

    codegen::emit(&manifest, &dist_dir)?;
    cargo_build(&dist_dir)?;

    let dist_canon: PathBuf = std::fs::canonicalize(&dist_dir).unwrap_or_else(|_| dist_dir.clone());

    let child = spawn_app(&dist_dir, &manifest.name, &services.lock().unwrap().env)?;
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));

    let cleanup_child = child_slot.clone();
    let cleanup_services = services.clone();
    ctrlc::set_handler(move || {
        eprintln!("\nrublocks dev: shutting down");
        if let Some(mut c) = cleanup_child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        cleanup_services.lock().unwrap().shutdown();
        std::process::exit(0);
    })
    .context("failed to install Ctrl+C handler")?;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(300), None, tx)
        .context("failed to create file watcher")?;
    debouncer
        .watch(project_dir, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", project_dir.display()))?;

    let mut last_hash = project_sources_hash(project_dir, &dist_canon);

    println!(
        "rublocks dev: watching {} (Ctrl+C to stop)",
        project_dir.display()
    );

    for result in rx {
        let events = match result {
            Ok(events) => events,
            Err(errs) => {
                for e in errs {
                    eprintln!("rublocks dev: watch error: {e:?}");
                }
                continue;
            }
        };

        if !relevant_change(&events, &dist_canon) {
            continue;
        }

        let new_hash = project_sources_hash(project_dir, &dist_canon);
        if new_hash == last_hash {
            continue;
        }
        last_hash = new_hash;

        println!("rublocks dev: change detected, rebuilding");
        if let Some(mut c) = child_slot.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }

        let manifest = match Manifest::load(project_dir) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("rublocks dev: manifest error: {e:?}");
                continue;
            }
        };
        if let Err(e) = codegen::emit(&manifest, &dist_dir) {
            eprintln!("rublocks dev: codegen error: {e:?}");
            continue;
        }
        if let Err(e) = cargo_build(&dist_dir) {
            eprintln!("rublocks dev: cargo build error: {e:?}");
            continue;
        }
        match spawn_app(&dist_dir, &manifest.name, &services.lock().unwrap().env) {
            Ok(c) => *child_slot.lock().unwrap() = Some(c),
            Err(e) => eprintln!("rublocks dev: spawn error: {e:?}"),
        }
    }

    Ok(())
}

/// Extensions that count as project source: codegen reads them, so a change
/// must trigger a rebuild.
const SOURCE_EXTS: &[&str] = &["json", "html"];

fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|ext| SOURCE_EXTS.contains(&ext))
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
        } else if is_source(&path) {
            if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
}

/// Run `cargo build` in `dist_dir`.
///
/// Inherits the parent's stdio so cargo's output streams to the dev console
/// in real time — the user sees compilation progress without buffering.
fn cargo_build(dist_dir: &Path) -> Result<()> {
    let status = Command::new("cargo")
        .arg("build")
        .current_dir(dist_dir)
        .status()
        .context("failed to invoke cargo")?;
    anyhow::ensure!(status.success(), "cargo build failed (status {status})");
    Ok(())
}

/// Spawn the freshly built dist binary with `RUBLOCKS_DEV=1`.
///
/// `extra_env` carries values provisioned by the Docker fallback for any
/// service whose `env:` variable was unset (see `dev_services`). It is empty
/// when the user already exported every service URL.
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
