use crate::server::{AppState, router};
use crate::store::Store;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: u64,
}

/// A missing `$HOME` is a diagnosable environment problem, not a bug — and
/// `auth.rs`/`cli.rs` already degrade gracefully rather than panic. Returning a
/// `Result` keeps the whole file honest instead of unwinding with a backtrace.
pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context(
        "could not locate a home directory — set $HOME so blueprint knows where ~/.blueprint lives",
    )
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".blueprint"))
}

pub fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("daemon.lock"))
}

/// flock(2)'d during the discover-or-spawn critical section in `ensure_running`
/// so two simultaneous CLI invocations don't both spawn a daemon and have the
/// second die on bind. Separate from `daemon.lock` (which is data — pid/port).
pub fn spawn_lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("daemon.spawn.lock"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("blueprints.db"))
}

pub fn read_lock() -> Option<LockInfo> {
    // No `exists()` pre-check: it's a TOCTOU with no upside, since the read
    // that follows already reports a missing file as an error.
    let bytes = std::fs::read(lock_path().ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the lock via temp-file + `rename`, because `fs::write` truncates before
/// it writes: a reader landing in that window sees an empty file, parses `None`,
/// concludes no daemon is running, and spawns a duplicate. `rename` within the
/// same directory is atomic, so a reader sees either the old lock or the new one.
pub fn write_lock(info: &LockInfo) -> Result<()> {
    let p = lock_path()?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(info)?;
    // PID in the temp name so two daemons racing to publish a lock don't
    // clobber each other's half-written scratch file.
    let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, &p) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// Remove the lock file ONLY if it still names `pid`. Avoids the race where
/// a daemon's graceful shutdown clears a lock that a newer daemon has already
/// written to (which happens when shutdown blocks on an in-flight long-poll
/// while `publish` spawns the replacement and writes its own lock).
pub fn clear_lock_for_pid(pid: u32) {
    if let Some(existing) = read_lock()
        && existing.pid == pid
        && let Ok(p) = lock_path()
    {
        let _ = std::fs::remove_file(p);
    }
}

/// signal 0 is "check existence" on unix.
///
/// `EPERM` means the process exists but belongs to another user — reporting that
/// as dead would let us clear a perfectly good daemon's lock and spawn a second
/// one that then fails to bind. Only `ESRCH` (no such process) is really dead.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Some(pid) = to_pid(pid) else {
            return false;
        };
        match nix::sys::signal::kill(pid, None) {
            Ok(()) => true,
            Err(nix::errno::Errno::EPERM) => true,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// `LockInfo.pid` is a `u32` on the wire; `kill(2)` wants a `pid_t`. Reject 0
/// (and anything that doesn't fit) rather than signalling a process group.
#[cfg(unix)]
fn to_pid(pid: u32) -> Option<nix::unistd::Pid> {
    if pid == 0 {
        return None;
    }
    i32::try_from(pid).ok().map(nix::unistd::Pid::from_raw)
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

/// Health probes deserve one client, not one per probe: `reqwest::Client` is
/// `Arc`-backed, so cloning shares the connection pool instead of rebuilding the
/// whole TLS/pool stack for a 200-byte GET against loopback.
fn probe_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(HEALTH_PROBE_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// Generous on purpose. The cost of a false negative is wildly asymmetric: an
/// unhealthy verdict leads straight into kill-and-respawn, so a daemon that was
/// merely busy (a handful of 30s reviewer long-polls plus a SQLite write) gets
/// murdered and every in-flight long-poll is dropped. Waiting 3s to be sure is
/// cheap; killing a working daemon is not.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Probe `/api/health` on the recorded port. Returns true only if the daemon
/// answered 2xx, so a stale lock file or a process wedged mid-boot can't pass.
async fn is_healthy(info: &LockInfo) -> bool {
    probe_client()
        .get(format!("http://127.0.0.1:{}/api/health", info.port))
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
/// That degradation is logged rather than silent, because "two daemons raced"
/// is otherwise indistinguishable from a bind failure at the far end.
pub struct SpawnLock {
    #[cfg(unix)]
    _flock: Option<nix::fcntl::Flock<std::fs::File>>,
}

fn acquire_spawn_lock() -> SpawnLock {
    #[cfg(unix)]
    {
        use nix::fcntl::{Flock, FlockArg};
        let Ok(dir) = data_dir() else {
            tracing::warn!("no home directory; spawning without the spawn lock");
            return SpawnLock { _flock: None };
        };
        let _ = std::fs::create_dir_all(&dir);
        let Ok(path) = spawn_lock_path() else {
            return SpawnLock { _flock: None };
        };
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
        else {
            tracing::warn!("could not open the spawn lock file; spawning unserialized");
            return SpawnLock { _flock: None };
        };
        // Non-blocking first so the common "peer is mid-spawn" case is visible
        // in a log line, and so a wedged peer holding the lock forever can't
        // hang us silently — `LockExclusive` alone would block with no
        // diagnostic at all.
        let flock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(f) => Some(f),
            Err((file, nix::errno::Errno::EWOULDBLOCK)) => {
                tracing::debug!(
                    "another blueprint process is mid-spawn; waiting on the spawn lock"
                );
                match Flock::lock(file, FlockArg::LockExclusive) {
                    Ok(f) => Some(f),
                    Err((_, e)) => {
                        tracing::warn!(%e, "blocking flock failed; spawning unserialized");
                        None
                    }
                }
            }
            Err((_, e)) => {
                tracing::warn!(%e, "could not flock the spawn lock; spawning unserialized");
                None
            }
        };
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
        // Lock points at a dead or stuck process. Make sure it's actually gone
        // before spawning: the replacement binds the same port, so a predecessor
        // that outlives us fails the new daemon's bind instead.
        terminate_and_reap(info.pid).await;
        clear_lock_for_pid(info.pid);
    }

    spawn_detached(exe)?;
    wait_for_daemon(STARTUP_BUDGET).await.with_context(|| {
        format!(
            "daemon did not come up within {}s",
            STARTUP_BUDGET.as_secs()
        )
    })
}

/// How long a doomed daemon gets to honor SIGTERM before we escalate. It may be
/// draining reviewer long-polls, so give it room — but not forever.
const TERM_GRACE: Duration = Duration::from_secs(3);

/// How long after SIGKILL we wait for the PID to vanish. SIGKILL is not
/// negotiable, so this only covers the kernel's teardown and the reap.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// Startup budget for a freshly spawned daemon. Sized to sit above the
/// worst-case kill path (TERM_GRACE + KILL_GRACE) plus a real boot — otherwise a
/// slow-but-successful shutdown eats the whole budget and `ensure_running` fails
/// on a daemon that was seconds from ready.
const STARTUP_BUDGET: Duration = Duration::from_secs(10);

/// SIGTERM, then poll for the PID to actually disappear, escalating to SIGKILL
/// if it outlives `TERM_GRACE`.
///
/// The reap matters as much as the signal. When the daemon is our own previously
/// spawned child, `Command::spawn` never waited on it, so SIGTERM turns it into
/// a zombie — and a zombie still answers `kill(pid, 0)`, so `process_alive`
/// would report it alive forever and we'd loop here on every invocation.
/// `waitpid(WNOHANG)` on each pass collects it if it is ours, and is a harmless
/// `ECHILD` if it isn't.
async fn terminate_and_reap(pid: u32) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{self, Signal};
        let Some(target) = to_pid(pid) else { return };
        if !process_alive(pid) {
            reap(target);
            return;
        }
        let _ = signal::kill(target, Signal::SIGTERM);
        if wait_for_exit(pid, target, TERM_GRACE).await {
            return;
        }
        tracing::warn!(
            pid,
            "daemon ignored SIGTERM for {}s; escalating to SIGKILL so the port is released",
            TERM_GRACE.as_secs()
        );
        let _ = signal::kill(target, Signal::SIGKILL);
        if !wait_for_exit(pid, target, KILL_GRACE).await {
            // Nothing left to try; the bind error from the fresh child will be
            // the user-visible symptom, so say why here.
            tracing::warn!(
                pid,
                "daemon survived SIGKILL; the replacement may fail to bind"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Poll until `pid` is gone or `budget` elapses, reaping on every pass.
#[cfg(unix)]
async fn wait_for_exit(pid: u32, target: nix::unistd::Pid, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        reap(target);
        if !process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Non-blocking reap. `ECHILD` (not our child) and `Ok(StillAlive)` are both
/// expected and uninteresting.
#[cfg(unix)]
fn reap(target: nix::unistd::Pid) {
    use nix::sys::wait::{WaitPidFlag, waitpid};
    let _ = waitpid(target, Some(WaitPidFlag::WNOHANG));
}

fn spawn_detached(exe: &Path) -> Result<()> {
    let log_path = data_dir()?.join("daemon.log");
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
        // detaches from the controlling terminal so the daemon survives the
        // parent's exit. It is async-signal-safe and allocates nothing, which is
        // what makes it legal in this window.
        unsafe {
            cmd.pre_exec(|| {
                // EPERM here means we're already a session leader — which is
                // exactly the state we wanted, so it isn't a spawn failure.
                match nix::unistd::setsid() {
                    Ok(_) | Err(nix::errno::Errno::EPERM) => Ok(()),
                    Err(e) => Err(std::io::Error::from(e)),
                }
            });
        }
    }
    cmd.spawn().context("failed to spawn daemon")?;
    Ok(())
}

/// Wait for a spawned daemon to become *usable*, not merely reachable.
/// `is_healthy` checks for 2xx; the earlier `.is_ok()` here accepted any reply,
/// so a daemon that bound the port and then failed to open SQLite counted as a
/// successful start and the caller got a lock pointing at a 500 machine.
async fn wait_for_daemon(timeout: Duration) -> Result<LockInfo> {
    let started = Instant::now();
    loop {
        if let Some(info) = discover_running()
            && is_healthy(&info).await
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
pub async fn run_foreground(preferred_port: Option<u16>, env: crate::auth::EnvFile) -> Result<()> {
    let store = Arc::new(Store::open(db_path()?).context("opening sqlite store")?);
    let auth_cfg = crate::auth::AuthConfig::from_env_file(&env);

    // Surface the two role-config misconfigurations loudly. Either branch
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
    spawn_session_sweeper(state.store.clone());
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

/// How often to drop expired session rows. Expired rows are already invisible
/// to `load_session`, so this is housekeeping, not correctness — but a daemon
/// that stays up for weeks would otherwise accumulate a row per abandoned
/// login until the next restart.
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);

/// Sweep expired sessions now, then once a day for as long as the daemon lives.
/// A detached task rather than work inside `router()`: building a router
/// shouldn't write to the database, and every integration test builds one.
fn spawn_session_sweeper(store: Arc<Store>) {
    tokio::spawn(async move {
        loop {
            match store.delete_expired_sessions() {
                Ok(0) => {}
                Ok(n) => tracing::debug!(n, "swept expired sessions"),
                Err(e) => tracing::warn!(%e, "could not sweep expired sessions"),
            }
            tokio::time::sleep(SESSION_SWEEP_INTERVAL).await;
        }
    });
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
