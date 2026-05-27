use crate::server::{AppState, router};
use crate::store::Store;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: u64,
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().expect("could not locate home directory")
}

pub fn data_dir() -> PathBuf {
    home_dir().join(".blueprint")
}

pub fn lock_path() -> PathBuf {
    data_dir().join("daemon.lock")
}

/// flock(2)'d during the discover-or-spawn critical section in `ensure_running`
/// so two simultaneous CLI invocations don't both spawn a daemon and have the
/// second die on bind. Separate from `daemon.lock` (which is data — pid/port).
pub fn spawn_lock_path() -> PathBuf {
    data_dir().join("daemon.spawn.lock")
}

pub fn db_path() -> PathBuf {
    data_dir().join("blueprints.db")
}

pub fn read_lock() -> Option<LockInfo> {
    let p = lock_path();
    if !p.exists() {
        return None;
    }
    let bytes = std::fs::read(&p).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_lock(info: &LockInfo) -> Result<()> {
    let p = lock_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(info)?;
    std::fs::write(&p, bytes)?;
    Ok(())
}

/// Remove the lock file ONLY if it still names `pid`. Avoids the race where
/// a daemon's graceful shutdown clears a lock that a newer daemon has already
/// written to (which happens when shutdown blocks on an in-flight long-poll
/// while `publish` spawns the replacement and writes its own lock).
pub fn clear_lock_for_pid(pid: u32) {
    if let Some(existing) = read_lock()
        && existing.pid == pid
    {
        let _ = std::fs::remove_file(lock_path());
    }
}

pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // signal 0 is "check existence" on unix
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) }
}

#[cfg(not(unix))]
unsafe fn libc_kill(_pid: i32, _sig: i32) -> i32 {
    -1
}

/// Look up a running daemon. If the lock file points at a live process, return its info.
pub fn discover_running() -> Option<LockInfo> {
    let info = read_lock()?;
    if process_alive(info.pid) {
        Some(info)
    } else {
        clear_lock_for_pid(info.pid);
        None
    }
}

/// Probe `/api/health` on the recorded port. ~500ms timeout; returns true only
/// if the daemon answered OK, so a stale lock file or stuck process can't pass.
async fn is_healthy(info: &LockInfo) -> bool {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}/api/health", info.port))
        .timeout(Duration::from_millis(500))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Drop-guard wrapping an advisory `flock(2)` on `spawn_lock_path()`.
/// While alive, no other `ensure_running` caller on this machine can be
/// mid-spawn. The lock is released when the guard drops (kernel releases on
/// close(2) of the underlying file).
///
/// Best-effort: if locking fails (NFS, libc weirdness), the guard contains
/// `None` and the caller proceeds without serialization — same behavior as
/// before this change, which is already self-healing via `wait_for_daemon`.
pub struct SpawnLock {
    #[cfg(unix)]
    _flock: Option<nix::fcntl::Flock<std::fs::File>>,
}

fn acquire_spawn_lock() -> SpawnLock {
    #[cfg(unix)]
    {
        use nix::fcntl::{Flock, FlockArg};
        let _ = std::fs::create_dir_all(data_dir());
        let flock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(spawn_lock_path())
            .ok()
            .and_then(|f| Flock::lock(f, FlockArg::LockExclusive).ok());
        SpawnLock { _flock: flock }
    }
    #[cfg(not(unix))]
    {
        SpawnLock {}
    }
}

/// Ensure a daemon is running. Reuses any healthy daemon — even on a different
/// port from what `resolve_port` would pick — so a second CLI session in a
/// different repo can't murder the first session's daemon just because its
/// shell has `BLUEPRINT_PORT` set differently. Only an unhealthy/stale lock
/// triggers respawn; the spawn itself is serialized via an flock on
/// `spawn_lock_path()` so concurrent callers don't both spawn.
pub async fn ensure_running(exe: &Path) -> Result<LockInfo> {
    // Fast path: a healthy daemon (any port) — no lock needed.
    if let Some(info) = discover_running()
        && is_healthy(&info).await
    {
        return Ok(info);
    }

    // Slow path: take the spawn lock and re-check under it. If another caller
    // raced ahead and started the daemon, we'll see it on the re-check.
    let _spawn_guard = acquire_spawn_lock();

    if let Some(info) = discover_running() {
        if is_healthy(&info).await {
            return Ok(info);
        }
        // Lock points at a dead or stuck process. SIGTERM if it's still
        // around, then clear the lock and spawn fresh.
        if process_alive(info.pid) {
            unsafe { libc_kill(info.pid as i32, 15) };
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        clear_lock_for_pid(info.pid);
    }

    spawn_detached(exe)?;
    wait_for_daemon(Duration::from_secs(5))
        .await
        .context("daemon did not come up within 5s")
}

fn spawn_detached(exe: &Path) -> Result<()> {
    let log_path = data_dir().join("daemon.log");
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(exe);
    cmd.arg("serve");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log));
    cmd.stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs in the child between fork() and exec(). setsid()
        // detaches from the controlling terminal so the daemon survives the parent's exit.
        unsafe {
            cmd.pre_exec(|| {
                unsafe extern "C" {
                    fn setsid() -> i32;
                }
                setsid();
                Ok(())
            });
        }
    }
    cmd.spawn().context("failed to spawn daemon")?;
    Ok(())
}

async fn wait_for_daemon(timeout: Duration) -> Result<LockInfo> {
    let started = Instant::now();
    loop {
        if let Some(info) = discover_running()
            && reqwest::Client::new()
                .get(format!("http://127.0.0.1:{}/api/health", info.port))
                .timeout(Duration::from_millis(500))
                .send()
                .await
                .is_ok()
        {
            return Ok(info);
        }
        if started.elapsed() > timeout {
            anyhow::bail!("timed out waiting for daemon");
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

/// Run the daemon in this process — blocks forever.
pub async fn run_foreground(preferred_port: Option<u16>) -> Result<()> {
    let store = Arc::new(Store::open(&db_path()).context("opening sqlite store")?);
    let auth_cfg = crate::auth::AuthConfig::from_env();

    // D3: surface the two role-config misconfigurations loudly. Either branch
    // means owner-role assignment is silently broken; an early stderr line
    // saves a confused debug session when comments never trip a plan edit.
    if auth_cfg.enabled && auth_cfg.owner_login.is_none() {
        eprintln!(
            "warn: GitHub OAuth is enabled but BLUEPRINT_OWNER_GITHUB_LOGIN is unset — \
             no comment will trigger a plan edit. Set it in ~/.blueprint/env to identify yourself."
        );
    }
    if !auth_cfg.enabled && auth_cfg.owner_login.is_some() {
        eprintln!(
            "warn: BLUEPRINT_OWNER_GITHUB_LOGIN is set but GitHub OAuth credentials are missing — \
             owner role will never be assigned. Add GITHUB_CLIENT_ID and GITHUB_CLIENT_SECRET to ~/.blueprint/env."
        );
    }

    let auth = if auth_cfg.enabled {
        Some(Arc::new(auth_cfg))
    } else {
        None
    };
    let state = AppState::with_auth(store, auth);
    let shutdown_notify = state.shutdown.clone();
    let app = router(state);

    let env_port: Option<u16> = std::env::var("BLUEPRINT_PORT")
        .ok()
        .and_then(|s| s.parse().ok());
    let port = resolve_port(preferred_port, env_port);
    let addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).await.with_context(|| {
        format!(
            "binding 127.0.0.1:{port} (port in use? pass `--port <n>` or set \
             BLUEPRINT_PORT to override — but note GitHub OAuth requires 7321)"
        )
    })?;
    let local = listener.local_addr()?;

    let info = LockInfo {
        pid: std::process::id(),
        port: local.port(),
        started_at: now_secs(),
    };
    write_lock(&info)?;
    tracing::info!(?info, "blueprint daemon listening");
    eprintln!(
        "blueprint daemon listening on http://127.0.0.1:{}",
        local.port()
    );

    // Cleanup the lock on shutdown — but only if it still belongs to us.
    // If a replacement daemon already wrote a new lock during our graceful
    // shutdown, we must not nuke it.
    let server =
        axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(shutdown_notify));
    let result = server.await.context("axum serve");
    clear_lock_for_pid(info.pid);
    result
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn shutdown_signal(notify: Arc<Notify>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    let notified = notify.notified();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
        _ = notified => {}
    }
}

/// Pick a bind port. Resolution order:
///   --port flag → BLUEPRINT_PORT env → 7321.
///
/// 7321 is the port hard-coded into the registered GitHub OAuth callback URL
/// (`http://127.0.0.1:7321/auth/github/callback`, see `src/auth.rs`). Defaulting
/// to it unconditionally keeps the daemon at a stable, predictable port so the
/// OAuth round-trip works regardless of when env credentials are configured.
pub fn resolve_port(cli_flag: Option<u16>, env_var: Option<u16>) -> u16 {
    cli_flag.or(env_var).unwrap_or(7321)
}

#[cfg(test)]
mod tests {
    use super::resolve_port;

    #[test]
    fn cli_flag_wins() {
        assert_eq!(resolve_port(Some(9000), Some(8000)), 9000);
    }

    #[test]
    fn env_var_wins_over_default() {
        assert_eq!(resolve_port(None, Some(8000)), 8000);
    }

    #[test]
    fn default_is_7321() {
        assert_eq!(resolve_port(None, None), 7321);
    }
}
