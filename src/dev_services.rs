//! Dev-mode Docker fallback for declared services.
//!
//! When `rublocks dev` starts and a service's `env:` variable is unset, we
//! spin up a minimal Docker container instead of crashing. Containers and
//! their data volumes are named deterministically per project+service so a
//! second `dev` invocation reuses the same database.
//!
//! See `docs/dev-mode.md#service-fallback` for the full lifecycle.

use crate::manifest::{Manifest, ServiceUrl};
use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POSTGRES_IMAGE: &str = "postgres:16-alpine";
const REDIS_IMAGE: &str = "redis:7-alpine";
const READINESS_TIMEOUT: Duration = Duration::from_secs(30);
const READINESS_POLL: Duration = Duration::from_millis(500);

/// Containers provisioned for the current dev session.
///
/// `tracked` lists every container we touched — both newly started and ones
/// we found already running. Ctrl+C stops them all: the `rublocks-dev-*`
/// naming convention makes the container ours by definition, so leaving any
/// of them up after the dev session ended would be surprising. `env` holds
/// the var/url pairs to inject into the dist child process.
pub struct DevServices {
    tracked: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl DevServices {
    /// Provision Docker fallbacks for every service whose `env:` variable is
    /// unset. Services with literal URLs, or env vars already set in the
    /// caller's environment, are left untouched.
    pub fn provision(manifest: &Manifest) -> Result<Self> {
        let mut services = DevServices {
            tracked: Vec::new(),
            env: Vec::new(),
        };

        let pg_var = manifest
            .services
            .postgres
            .as_ref()
            .and_then(|s| needs_fallback(&s.url));
        let redis_var = manifest
            .services
            .redis
            .as_ref()
            .and_then(|s| needs_fallback(&s.url));

        if pg_var.is_some() || redis_var.is_some() {
            ensure_docker()?;
        }

        if let Some(var) = pg_var {
            let url = services.ensure_postgres(&manifest.name)?;
            eprintln!("rublocks dev: postgres ready at {url} (env {var})");
            services.env.push((var, url));
        }
        if let Some(var) = redis_var {
            let url = services.ensure_redis(&manifest.name)?;
            eprintln!("rublocks dev: redis ready at {url} (env {var})");
            services.env.push((var, url));
        }

        Ok(services)
    }

    /// Stop every container this session provisioned. Volumes and the
    /// container definitions are kept so the next `dev` run picks up the same
    /// data.
    pub fn shutdown(&mut self) {
        for name in self.tracked.drain(..) {
            let _ = Command::new("docker")
                .args(["stop", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    fn ensure_postgres(&mut self, app_name: &str) -> Result<String> {
        let container = format!("rublocks-dev-{app_name}-postgres");
        let volume = format!("rublocks-dev-{app_name}-postgres-data");
        let db_name = app_name.replace('-', "_");
        let volume_mount = format!("{volume}:/var/lib/postgresql/data");
        let db_env = format!("POSTGRES_DB={db_name}");

        match container_status(&container)? {
            ContainerStatus::Missing => {
                let status = Command::new("docker")
                    .args([
                        "run",
                        "-d",
                        "--name",
                        &container,
                        "--label",
                        "rublocks-dev=1",
                        "-v",
                        &volume_mount,
                        "-e",
                        "POSTGRES_USER=rublocks",
                        "-e",
                        "POSTGRES_PASSWORD=rublocks",
                        "-e",
                        &db_env,
                        "-p",
                        "5432",
                        POSTGRES_IMAGE,
                    ])
                    .stdout(Stdio::null())
                    .status()
                    .context("failed to invoke docker run")?;
                anyhow::ensure!(status.success(), "docker run for postgres failed");
            }
            ContainerStatus::Stopped => {
                let status = Command::new("docker")
                    .args(["start", &container])
                    .stdout(Stdio::null())
                    .status()
                    .context("failed to invoke docker start")?;
                anyhow::ensure!(status.success(), "docker start for postgres failed");
            }
            ContainerStatus::Running => {}
        }
        self.tracked.push(container.clone());

        let port = read_host_port(&container, "5432/tcp")?;
        wait_until_ready(
            &container,
            "postgres",
            &["exec", &container, "pg_isready", "-U", "rublocks"],
            None,
        )?;
        Ok(format!(
            "postgres://rublocks:rublocks@127.0.0.1:{port}/{db_name}"
        ))
    }

    fn ensure_redis(&mut self, app_name: &str) -> Result<String> {
        let container = format!("rublocks-dev-{app_name}-redis");
        let volume = format!("rublocks-dev-{app_name}-redis-data");
        let volume_mount = format!("{volume}:/data");

        match container_status(&container)? {
            ContainerStatus::Missing => {
                let status = Command::new("docker")
                    .args([
                        "run",
                        "-d",
                        "--name",
                        &container,
                        "--label",
                        "rublocks-dev=1",
                        "-v",
                        &volume_mount,
                        "-p",
                        "6379",
                        REDIS_IMAGE,
                        "redis-server",
                        "--appendonly",
                        "yes",
                    ])
                    .stdout(Stdio::null())
                    .status()
                    .context("failed to invoke docker run")?;
                anyhow::ensure!(status.success(), "docker run for redis failed");
            }
            ContainerStatus::Stopped => {
                let status = Command::new("docker")
                    .args(["start", &container])
                    .stdout(Stdio::null())
                    .status()
                    .context("failed to invoke docker start")?;
                anyhow::ensure!(status.success(), "docker start for redis failed");
            }
            ContainerStatus::Running => {}
        }
        self.tracked.push(container.clone());

        let port = read_host_port(&container, "6379/tcp")?;
        wait_until_ready(
            &container,
            "redis",
            &["exec", &container, "redis-cli", "ping"],
            Some("PONG"),
        )?;
        Ok(format!("redis://127.0.0.1:{port}/"))
    }
}

/// Returns the env-var name when the URL is `env:VAR` and `VAR` is unset.
fn needs_fallback(url: &ServiceUrl) -> Option<String> {
    match url {
        ServiceUrl::Env(var) if std::env::var(var).is_err() => Some(var.clone()),
        _ => None,
    }
}

enum ContainerStatus {
    Missing,
    Running,
    Stopped,
}

/// Probe a container by name; "no such object" maps to `Missing`.
fn container_status(name: &str) -> Result<ContainerStatus> {
    let output = Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", name])
        .output()
        .context("failed to invoke docker inspect")?;
    if !output.status.success() {
        return Ok(ContainerStatus::Missing);
    }
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(match state.as_str() {
        "running" => ContainerStatus::Running,
        _ => ContainerStatus::Stopped,
    })
}

/// Read the host-side port for `<internal>` (e.g. `"5432/tcp"`).
///
/// Docker prints one line per address family (`0.0.0.0:PORT`, `[::]:PORT`);
/// we take the first line and parse the trailing port number.
fn read_host_port(container: &str, internal: &str) -> Result<u16> {
    let output = Command::new("docker")
        .args(["port", container, internal])
        .output()
        .context("failed to invoke docker port")?;
    anyhow::ensure!(
        output.status.success(),
        "docker port {container} {internal} failed"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("docker port returned empty output for {container}"))?;
    let port_str = line
        .rsplit(':')
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed docker port line: {line}"))?
        .trim();
    port_str
        .parse::<u16>()
        .with_context(|| format!("could not parse port from `{line}`"))
}

/// Verify `docker version` succeeds; produce a friendly error otherwise so the
/// user knows to either start Docker or export the missing env vars by hand.
fn ensure_docker() -> Result<()> {
    let status = Command::new("docker")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => bail!(
            "Docker is installed but the daemon is not reachable. \
             Start Docker, or export the missing env vars manually before running `rublocks dev`."
        ),
        Err(_) => bail!(
            "Docker not found in PATH. Install Docker, \
             or export the missing env vars manually before running `rublocks dev`."
        ),
    }
}

/// Poll a readiness probe inside the container until it passes or we time out.
///
/// `expected_stdout`, when set, must appear in the probe's stdout for it to
/// count as ready (used for `redis-cli ping` → `PONG`).
fn wait_until_ready(
    container: &str,
    label: &str,
    args: &[&str],
    expected_stdout: Option<&str>,
) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < READINESS_TIMEOUT {
        if let Ok(out) = Command::new("docker").args(args).output() {
            if out.status.success() {
                let ready = match expected_stdout {
                    Some(needle) => String::from_utf8_lossy(&out.stdout).contains(needle),
                    None => true,
                };
                if ready {
                    return Ok(());
                }
            }
        }
        thread::sleep(READINESS_POLL);
    }
    bail!(
        "{label} container `{container}` did not become ready within {}s",
        READINESS_TIMEOUT.as_secs()
    )
}
