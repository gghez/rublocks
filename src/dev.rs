use anyhow::{Context, Result};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebouncedEvent};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{codegen, manifest::Manifest};

pub fn run(project_dir: &Path) -> Result<()> {
    let dist_dir = project_dir.join("dist");

    println!("rublocks dev: initial build");
    let manifest = Manifest::load(project_dir)?;
    codegen::emit(&manifest, &dist_dir)?;
    cargo_build(&dist_dir)?;

    let dist_canon: PathBuf = std::fs::canonicalize(&dist_dir).unwrap_or_else(|_| dist_dir.clone());

    let child = spawn_app(&dist_dir, &manifest.name)?;
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));

    let cleanup = child_slot.clone();
    ctrlc::set_handler(move || {
        eprintln!("\nrublocks dev: shutting down");
        if let Some(mut c) = cleanup.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        std::process::exit(0);
    })
    .context("failed to install Ctrl+C handler")?;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(300), None, tx)
        .context("failed to create file watcher")?;
    debouncer
        .watch(project_dir, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", project_dir.display()))?;

    let mut last_hash = project_json_hash(project_dir, &dist_canon);

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

        let new_hash = project_json_hash(project_dir, &dist_canon);
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
        match spawn_app(&dist_dir, &manifest.name) {
            Ok(c) => *child_slot.lock().unwrap() = Some(c),
            Err(e) => eprintln!("rublocks dev: spawn error: {e:?}"),
        }
    }

    Ok(())
}

fn relevant_change(events: &[DebouncedEvent], dist_canon: &Path) -> bool {
    events.iter().any(|e| {
        e.event.paths.iter().any(|p| {
            let is_json = p.extension().and_then(|x| x.to_str()) == Some("json");
            let in_dist = p.starts_with(dist_canon);
            is_json && !in_dist
        })
    })
}

fn project_json_hash(project_dir: &Path, dist_canon: &Path) -> u64 {
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    collect_json(project_dir, dist_canon, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = DefaultHasher::new();
    for (path, bytes) in &files {
        path.to_string_lossy().hash(&mut hasher);
        bytes.hash(&mut hasher);
    }
    hasher.finish()
}

fn collect_json(dir: &Path, exclude: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.starts_with(exclude) {
            continue;
        }
        if path.is_dir() {
            collect_json(&path, exclude, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("json") {
            if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
}

fn cargo_build(dist_dir: &Path) -> Result<()> {
    let status = Command::new("cargo")
        .arg("build")
        .current_dir(dist_dir)
        .status()
        .context("failed to invoke cargo")?;
    anyhow::ensure!(status.success(), "cargo build failed (status {status})");
    Ok(())
}

fn spawn_app(dist_dir: &Path, app_name: &str) -> Result<Child> {
    let binary = dist_dir.join("target").join("debug").join(app_name);
    Command::new(&binary)
        .env("RUBLOCKS_DEV", "1")
        .spawn()
        .with_context(|| format!("failed to spawn {}", binary.display()))
}
