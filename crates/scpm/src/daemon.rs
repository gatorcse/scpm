//! The shared compile daemon (design §6.3): one warm worker JVM per (scpm build, JDK, worker
//! classpath, JVM options) per user, serving every workspace and worktree over a Unix socket.
//!
//! `scpm daemon serve` is a small Rust process that owns the worker JVM (on stdio) and
//! multiplexes clients onto it: each client request is forwarded with a fresh id and its
//! events are routed back to that client with the client's id, so the wire format is the
//! worker protocol (docs/worker-protocol.md). It exits after an idle period, when its worker
//! dies, or on `scpm daemon stop`.
//!
//! Files in the daemon directory, per key: `<key>.lock` (held by the daemon for its
//! lifetime), `<key>.sock`, `<key>.json` (informational), `<key>.log` (+ `.log.1`), and
//! `<key>.failed` (a recent start failure, so new commands don't retry it every time).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::fetch;
use crate::worker::{self, Worker, WorkerError};

/// Default idle time before a daemon exits; `SCPM_DAEMON_IDLE_SECS` overrides it.
const DEFAULT_IDLE: Duration = Duration::from_secs(3600);
/// Default worker heap; `SCPM_DAEMON_HEAP` overrides it.
const DEFAULT_HEAP: &str = "4g";
/// Longest socket path we bind (the OS limit is 104 bytes on macOS, 108 on Linux).
const MAX_SOCKET_PATH: usize = 100;
/// Length of a daemon key (hex).
const KEY_LEN: usize = 16;
/// How long `initialize`, `daemon/status` and `daemon/stop` may take.
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a new daemon may take to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a failed start keeps new commands from trying again.
const FAILURE_TTL: Duration = Duration::from_secs(300);
/// Messages queued for one client before it counts as stalled.
const CLIENT_QUEUE: usize = 4096;
/// How long one write to a client may block before it counts as stalled.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest request line a client may send.
const MAX_LINE: u64 = 64 << 20;
/// How long `daemon/stop` waits for active requests.
const STOP_WAIT: Duration = Duration::from_secs(30);
/// Workspaces listed in the status.
const MAX_WORKSPACES: usize = 50;
const NAME: &str = "the scpm daemon";

static DISABLED: AtomicBool = AtomicBool::new(false);

/// Turns the daemon off for this process (`--no-daemon`).
pub fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
}

/// Whether this platform can run the shared daemon (Unix with peer credential checks).
pub const SUPPORTED: bool = PEER_CHECKED && cfg!(unix);

/// Whether commands should use the shared daemon: on Unix, outside CI, unless disabled with
/// `--no-daemon` or `SCPM_NO_DAEMON`.
pub fn enabled() -> bool {
    // Only where the peer's uid can be checked (`peer_is_us`); elsewhere a client could talk to
    // another user's socket.
    PEER_CHECKED
        && cfg!(unix)
        && !DISABLED.load(Ordering::Relaxed)
        && std::env::var_os("SCPM_NO_DAEMON").is_none_or(|v| v.is_empty() || v == "0")
        && !crate::install::in_ci()
}

fn setup(path: &Path, e: impl std::fmt::Display) -> WorkerError {
    WorkerError::Setup(format!("{}: {e}", path.display()))
}

/// `SCPM_DAEMON_DIR`, or `<cache>/scpm/daemons`.
fn configured_dir() -> Result<PathBuf, WorkerError> {
    if let Some(d) = std::env::var_os("SCPM_DAEMON_DIR").filter(|d| !d.is_empty()) {
        // Absolute: the daemon runs in this directory and resolves it again.
        return std::path::absolute(&d).map_err(|e| setup(Path::new(&d), e));
    }
    dirs::cache_dir()
        .map(|c| c.join("scpm/daemons"))
        .ok_or_else(|| WorkerError::Setup("cannot find the user cache directory; set SCPM_DAEMON_DIR".into()))
}

/// The daemon directory (created private to this user): `SCPM_DAEMON_DIR` or
/// `<cache>/scpm/daemons`, or `/tmp/scpm-<uid>` when socket paths there would be too long
/// (`SCPM_DAEMON_FALLBACK_DIR` overrides that, for tests).
pub fn dir() -> Result<PathBuf, WorkerError> {
    let configured = configured_dir()?;
    let probe = configured.join(format!("{}.sock", "0".repeat(KEY_LEN)));
    let fallback = probe.as_os_str().len() > MAX_SOCKET_PATH;
    // Absolute: the daemon runs with its directory as cwd and the client connects from its own,
    // so a relative override would name two different places.
    let dir = if fallback {
        let dir = fallback_dir();
        std::path::absolute(&dir).map_err(|e| setup(&dir, e))?
    } else {
        configured
    };
    // A shared fallback like `/tmp/scpm-<uid>` could be planted as a symlink by another user;
    // a configured or cache directory may be a deliberate one.
    ensure_private_dir(&dir, !fallback)?;
    Ok(dir)
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(unix)]
fn fallback_dir() -> PathBuf {
    std::env::var_os("SCPM_DAEMON_FALLBACK_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(|| PathBuf::from(format!("/tmp/scpm-{}", uid())), PathBuf::from)
}

#[cfg(not(unix))]
fn fallback_dir() -> PathBuf {
    std::env::temp_dir().join("scpm-daemons")
}

/// Creates `dir` with mode 0700 (tightening an existing one) and refuses one owned by
/// another user, or a symlink unless `follow_symlinks` (e.g. a planted `/tmp/scpm-<uid>`).
#[cfg(unix)]
fn ensure_private_dir(dir: &Path, follow_symlinks: bool) -> Result<(), WorkerError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| setup(dir, e))?;
    let meta =
        if follow_symlinks { fs::metadata(dir) } else { fs::symlink_metadata(dir) }.map_err(|e| setup(dir, e))?;
    if !meta.is_dir() || meta.uid() != uid() {
        return Err(setup(dir, "not a directory owned by the current user"));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| setup(dir, e))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path, _follow_symlinks: bool) -> Result<(), WorkerError> {
    fs::create_dir_all(dir).map_err(|e| setup(dir, e))
}

/// The files of one daemon.
struct Paths {
    sock: PathBuf,
    info: PathBuf,
    log: PathBuf,
    lock: PathBuf,
    failed: PathBuf,
}

impl Paths {
    fn new(dir: &Path, key: &str) -> Paths {
        let f = |ext: &str| dir.join(format!("{key}.{ext}"));
        Paths { sock: f("sock"), info: f("json"), log: f("log"), lock: f("lock"), failed: f("failed") }
    }
}

/// What a daemon was started with; written next to its socket for `daemon ls` and `stop`
/// (informational: the daemon gets its classpath on the command line).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub key: String,
    pub scpm_version: String,
    pub classpath: Vec<PathBuf>,
    #[serde(default)]
    pub jvm_options: Vec<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub worker_pid: Option<u32>,
    /// The worker is initialized and the daemon accepts clients.
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub started: Option<u64>,
}

fn read_info(path: &Path) -> DaemonInfo {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_info(path: &Path, info: &DaemonInfo) {
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    if write_new(&tmp, &serde_json::to_vec_pretty(info).expect("info serializes")).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Writes a file that must not exist yet (so never through a planted symlink).
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    fs::OpenOptions::new().write(true).create_new(true).open(path)?.write_all(bytes)
}

/// JVM options of the daemon's worker: a heap cap, and ZGC with a soft limit so an idle
/// daemon returns memory.
fn jvm_options() -> Result<Vec<String>, WorkerError> {
    let heap = std::env::var("SCPM_DAEMON_HEAP").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| DEFAULT_HEAP.into());
    let bytes = parse_size(&heap)
        .ok_or_else(|| WorkerError::Setup(format!("SCPM_DAEMON_HEAP={heap} is not a heap size such as 4g or 2048m")))?;
    Ok(vec![format!("-Xmx{heap}"), "-XX:+UseZGC".into(), format!("-XX:SoftMaxHeapSize={}k", bytes / 2 / 1024)])
}

/// `4g` → bytes. At least 16 MB.
fn parse_size(s: &str) -> Option<u64> {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n: u64 = s[..digits].parse().ok()?;
    let unit = match s[digits..].to_ascii_lowercase().as_str() {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return None,
    };
    n.checked_mul(unit).filter(|b| *b >= 16 << 20)
}

/// `path len mtime`, so a rebuilt file changes the key.
fn stamp(path: &Path) -> String {
    let meta = fs::metadata(path).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("{} {} {mtime}", path.display(), meta.map_or(0, |m| m.len()))
}

/// The real path of an executable named on `PATH` (executable files only, like spawning)
/// or given as a path.
fn resolve_executable(path: &Path) -> PathBuf {
    let found = if path.components().count() > 1 { Some(path.to_path_buf()) } else { worker::find_on_path(path) };
    found.and_then(|p| p.canonicalize().ok()).unwrap_or_else(|| path.to_path_buf())
}

/// The daemon key: one daemon per (scpm version and binary, JDK, worker classpath, JVM
/// options). Binaries and jars count with their size and mtime, so rebuilding one gets a new
/// daemon.
fn key(classpath: &[PathBuf], jvm_options: &[String]) -> Result<String, WorkerError> {
    let java = worker::java()?;
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(format!("v2\n{}\nexe {}\n", env!("CARGO_PKG_VERSION"), stamp(&exe)));
    h.update(format!("java {} {}\n", resolve_executable(&java.path).display(), java.version));
    for o in jvm_options {
        h.update(format!("opt {o}\n"));
    }
    for p in classpath {
        h.update(format!("cp {}\n", stamp(p)));
    }
    Ok(fetch::hex(&h.finalize())[..KEY_LEN].to_string())
}

/// Takes `path`'s exclusive lock without waiting; `None` if another process holds it.
#[cfg(unix)]
fn try_lock(path: &Path, create: bool) -> std::io::Result<Option<File>> {
    use std::os::fd::AsRawFd;
    let file = fs::OpenOptions::new().read(true).write(true).create(create).truncate(false).open(path)?;
    // SAFETY: valid fd for the lifetime of `file`.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == std::io::ErrorKind::WouldBlock { Ok(None) } else { Err(e) }
}

/// Whether a daemon (running or starting) holds the lock at `path`.
#[cfg(unix)]
fn lock_held(path: &Path) -> bool {
    matches!(try_lock(path, false), Ok(None))
}

/// The last lines of a log, for error messages.
fn log_tail(log: &Path, lines: usize) -> String {
    let text = fs::read(log).unwrap_or_default();
    let text = String::from_utf8_lossy(&text[text.len().saturating_sub(64 * 1024)..]).into_owned();
    let tail: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
    tail[tail.len().saturating_sub(lines)..]
        .iter()
        .map(|l| l.strip_prefix("error: ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// A start failure recorded in the last few minutes, if any.
fn recent_failure(marker: &Path) -> Option<String> {
    let age = fs::metadata(marker).ok()?.modified().ok()?.elapsed().unwrap_or_default();
    if age >= FAILURE_TTL {
        return None;
    }
    let reason = fs::read_to_string(marker).unwrap_or_default();
    Some(format!("{}; not retrying for {}s", reason.trim_end(), (FAILURE_TTL - age).as_secs()))
}

/// Environment for the daemon: what the worker and scpm need, without per-shell JVM
/// options (`JAVA_TOOL_OPTIONS`) or other noise from the terminal that started it.
fn daemon_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(k, _)| {
            let Some(k) = k.to_str() else { return false };
            matches!(k, "PATH" | "HOME" | "JAVA_HOME" | "LANG" | "TMPDIR" | "USER" | "LOGNAME")
                || ["SCPM_", "COURSIER_", "LC_"].iter().any(|p| k.starts_with(p))
                || k.to_ascii_lowercase().ends_with("_proxy")
        })
        .collect()
}

enum Connect {
    /// Nothing listening (or it went away while we connected): start one.
    NotRunning,
    /// Something is listening but doesn't answer.
    Failed(WorkerError),
}

#[cfg(unix)]
fn try_connect(sock: &Path, timeout: Duration, on_event: impl Fn(Value) + Send + 'static) -> Result<Worker, Connect> {
    let stream = std::os::unix::net::UnixStream::connect(sock).map_err(|_| Connect::NotRunning)?;
    if !peer_is_us(&stream) {
        return Err(Connect::Failed(setup(sock, "served by another user")));
    }
    Worker::over_socket(NAME, stream, timeout, true, on_event).map_err(|e| match e {
        WorkerError::Exited(_) => Connect::NotRunning,
        e => Connect::Failed(e),
    })
}

/// Connects to the daemon for `classpath`, starting it if needed. Events of requests sent on
/// the returned connection go to `on_event`.
#[cfg(unix)]
pub fn connect_or_start(
    classpath: &[PathBuf],
    on_event: impl Fn(Value) + Send + Clone + 'static,
) -> Result<Worker, WorkerError> {
    use std::os::unix::process::CommandExt;

    // The daemon runs in its own directory.
    let classpath: Vec<PathBuf> =
        classpath.iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone())).collect();
    let jvm_options = jvm_options()?;
    let key = key(&classpath, &jvm_options)?;
    let java = resolve_executable(&worker::java()?.path);
    let java_home = java.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default();
    let dir = dir()?;
    let p = Paths::new(&dir, &key);
    // A daemon whose worker is still starting answers `initialize` once it is up.
    let info = read_info(&p.info);
    let timeout = if info.pid.is_some() && !info.ready { START_TIMEOUT } else { RPC_TIMEOUT };
    let first = Instant::now();
    let connected = match try_connect(&p.sock, timeout, on_event.clone()) {
        // It may have started after we read its info: if it is still starting, wait for it.
        Err(Connect::Failed(WorkerError::Timeout(_)))
            if timeout < START_TIMEOUT && {
                let info = read_info(&p.info);
                info.pid.is_some() && !info.ready
            } =>
        {
            try_connect(&p.sock, START_TIMEOUT.saturating_sub(first.elapsed()), on_event.clone())
        }
        other => other,
    };
    match connected {
        Ok(w) => return Ok(w),
        Err(Connect::Failed(e)) => return Err(e),
        Err(Connect::NotRunning) => {}
    }
    if let Some(reason) = recent_failure(&p.failed) {
        return Err(WorkerError::Setup(reason));
    }
    let spawn = || -> Result<std::process::Child, WorkerError> {
        let log = worker::open_log(&p.log).map_err(|e| setup(&p.log, e))?;
        let exe = std::env::current_exe().map_err(|e| WorkerError::Setup(e.to_string()))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["daemon", "serve", "--key", &key]);
        for entry in &classpath {
            cmd.arg("--cp").arg(entry);
        }
        for o in &jvm_options {
            cmd.arg(format!("--jvm-opt={o}"));
        }
        cmd.current_dir(&dir)
            .env_clear()
            .envs(daemon_env())
            .env("SCPM_DAEMON_DIR", &dir)
            // The exact executable the key was computed for: a relative home or a `bin/java`
            // symlinked to a differently named launcher resolve the same way in the daemon.
            .env("SCPM_JAVA", &java)
            .env("SCPM_JAVA_HOME", &java_home)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(|e| setup(&p.log, e))?)
            .stderr(log);
        // SAFETY: setsid is async-signal-safe. Its own session: no controlling terminal, so a
        // Ctrl-C or a closed terminal doesn't stop it.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().map_err(|e| WorkerError::Setup(format!("cannot start the daemon: {e}")))
    };
    // Reap it when it exits (the MCP server outlives daemons). A slow start is left to
    // finish for the next command.
    let detach = |child: Option<std::process::Child>| {
        if let Some(mut child) = child {
            std::thread::spawn(move || child.wait());
        }
    };
    let mut child = Some(spawn()?);
    let started = Instant::now();
    let mut respawned = false;
    loop {
        let remaining = START_TIMEOUT.saturating_sub(started.elapsed()).max(Duration::from_secs(1));
        match try_connect(&p.sock, remaining, on_event.clone()) {
            Ok(w) => {
                detach(child);
                return Ok(w);
            }
            Err(Connect::Failed(e)) => {
                detach(child);
                return Err(WorkerError::Setup(format!("the daemon started but did not answer: {e}")));
            }
            Err(Connect::NotRunning) => {}
        }
        if let Some(c) = child.as_mut()
            && let Ok(Some(status)) = c.try_wait()
        {
            child = None;
            if !status.success() {
                // A real failure: don't retry it for a while.
                let code = status.code().map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
                let reason =
                    format!("the daemon exited with {code}: {}\n  log: {}", log_tail(&p.log, 5), p.log.display());
                let _ = fs::remove_file(&p.failed);
                let _ = write_new(&p.failed, reason.as_bytes());
                return Err(WorkerError::Setup(reason));
            }
            // Exit 0: it lost this key's lock to another daemon, starting or stopping. If
            // that one is gone already, start once more.
            if !respawned && !lock_held(&p.lock) && !p.sock.exists() {
                child = Some(spawn()?);
                respawned = true;
            }
        }
        if child.is_none()
            && let Some(reason) = recent_failure(&p.failed)
        {
            return Err(WorkerError::Setup(reason));
        }
        if started.elapsed() >= START_TIMEOUT {
            detach(child);
            return Err(WorkerError::Setup(format!(
                "the daemon did not start within {}s; see `scpm daemon logs`",
                START_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(unix))]
pub fn connect_or_start(_: &[PathBuf], _: impl Fn(Value) + Send + Clone + 'static) -> Result<Worker, WorkerError> {
    Err(WorkerError::Setup("the shared daemon needs Unix sockets".into()))
}

#[cfg(unix)]
fn request(sock: &Path, method: &str, params: Value) -> Result<Value, WorkerError> {
    // Without `initialize`: a status request doesn't count as activity.
    let stream = std::os::unix::net::UnixStream::connect(sock).map_err(|e| setup(sock, e))?;
    if !peer_is_us(&stream) {
        return Err(setup(sock, "served by another user"));
    }
    let w = Worker::over_socket(NAME, stream, RPC_TIMEOUT, false, |_| {})?;
    let r = w.request_timeout(method, params, Some(RPC_TIMEOUT));
    w.shutdown();
    r
}

/// Output of `ps -o <field>= -p <pid>`.
fn ps(pid: u32, field: &str) -> Option<String> {
    let out =
        std::process::Command::new("ps").args(["-o", &format!("{field}="), "-p", &pid.to_string()]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// Status of every known daemon. Leftovers of daemons that are gone are removed; a daemon
/// that holds its lock but doesn't answer (starting, or hung) is listed with its `state`.
#[cfg(unix)]
pub fn list() -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(dir) = dir() else { return out };
    let keys: BTreeSet<String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension()?.to_str()?;
            matches!(ext, "json" | "sock" | "lock").then(|| path.file_stem()?.to_str().map(str::to_string)).flatten()
        })
        .collect();
    for key in keys {
        let p = Paths::new(&dir, &key);
        match request(&p.sock, "daemon/status", json!({})) {
            Ok(mut status) => {
                if let Some(rss) = status["workerPid"].as_u64().and_then(|pid| ps(pid as u32, "rss")) {
                    status["workerRssKb"] = rss.parse::<u64>().map_or(Value::Null, Value::from);
                }
                out.push(status);
            }
            Err(e) => {
                let Some(_lock) = hold_unless_running(&p, |info| {
                    let state = match (p.sock.exists(), info.ready) {
                        (true, _) => "unresponsive",
                        (false, true) => "stopping",
                        (false, false) => "starting",
                    };
                    out.push(json!({"key": key, "state": state, "pid": info.pid, "workerPid": info.worker_pid, "error": e.to_string()}));
                }) else {
                    continue;
                };
                // No daemon: remove what it left behind (holding its lock, so none starts meanwhile).
                let _ = fs::remove_file(&p.sock);
                let _ = fs::remove_file(&p.info);
                // Logs of daemons gone for a week (lock files stay: a starter may have one open).
                let old = |f: &Path| {
                    fs::metadata(f)
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > Duration::from_secs(7 * 24 * 3600)))
                };
                if old(&p.lock) && (old(&p.log) || !p.log.exists()) {
                    for f in [&p.log, &p.log.with_extension("log.1"), &p.failed] {
                        let _ = fs::remove_file(f);
                    }
                }
            }
        }
    }
    out
}

#[cfg(not(unix))]
pub fn list() -> Vec<Value> {
    Vec::new()
}

/// Stops every daemon; returns how many were stopped and the keys of those that did not
/// stop. A daemon with active requests gets up to 30 s to finish them unless `force`; one
/// that doesn't answer is terminated.
#[cfg(unix)]
pub fn stop_all(force: bool, notify: impl Fn(&str)) -> (usize, Vec<String>) {
    let Ok(dir) = dir() else { return (0, Vec::new()) };
    let (mut stopped, mut failed) = (0, Vec::new());
    for entry in list() {
        let Some(key) = entry["key"].as_str() else { continue };
        let p = Paths::new(&dir, key);
        // The instance we are stopping: a replacement that takes the key meanwhile is left alone.
        let pid = entry["pid"].as_u64().map(|p| p as u32);
        let done = if entry["state"] == "running" {
            match request(&p.sock, "daemon/stop", json!({"force": force})) {
                Ok(r) => {
                    let active = r["activeRequests"].as_u64().unwrap_or(0);
                    if active > 0 && !force {
                        notify(&format!(
                            "daemon {key}: waiting for {active} active request{} (--force cancels them)",
                            if active == 1 { "" } else { "s" }
                        ));
                    }
                    wait_gone(&p, key, pid, STOP_WAIT + Duration::from_secs(5)) || terminate(&p, key, pid)
                }
                Err(_) => terminate(&p, key, pid),
            }
        } else if entry["state"] == "stopping" {
            // Already draining: `--force` escalates; otherwise wait for it as above.
            if force {
                terminate(&p, key, pid)
            } else {
                notify(&format!("daemon {key}: waiting for it to finish stopping (--force kills it)"));
                wait_gone(&p, key, pid, STOP_WAIT + Duration::from_secs(5)) || terminate(&p, key, pid)
            }
        } else {
            terminate(&p, key, pid)
        };
        if done {
            stopped += 1;
        } else {
            notify(&format!("daemon {key} did not stop"));
            failed.push(key.to_string());
        }
    }
    (stopped, failed)
}

#[cfg(not(unix))]
pub fn stop_all(_: bool, _: impl Fn(&str)) -> (usize, Vec<String>) {
    (0, Vec::new())
}

/// Whether `pid` is this key's daemon process.
#[cfg(unix)]
fn is_daemon(pid: u32, key: &str) -> bool {
    ps(pid, "command").is_some_and(|c| c.contains("daemon serve") && c.contains(&format!("--key {key}")))
}

/// Whether the daemon instance `pid` (or, unknown, any daemon) no longer holds the key: its
/// lock is free, or another instance has taken over.
#[cfg(unix)]
fn instance_gone(p: &Paths, key: &str, pid: Option<u32>) -> bool {
    !lock_held(&p.lock) || pid.is_some_and(|pid| replaced(p, pid) || !is_daemon(pid, key))
}

/// Whether another instance than `pid` has written the key's info (an exiting one removes
/// its info before it lets go of the lock, so missing info doesn't count).
#[cfg(unix)]
fn replaced(p: &Paths, pid: u32) -> bool {
    read_info(&p.info).pid.is_some_and(|other| other != pid)
}

/// Waits up to `limit` for [`instance_gone`].
#[cfg(unix)]
fn wait_gone(p: &Paths, key: &str, pid: Option<u32>, limit: Duration) -> bool {
    let started = Instant::now();
    while !instance_gone(p, key, pid) {
        if started.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// Signals a daemon that doesn't answer (SIGTERM, then SIGKILL) and kills its worker JVM.
/// Only processes that are this key's daemon instance `expected` (the one listed) and its
/// worker are signalled; a replacement that took over the key counts as stopped.
#[cfg(unix)]
fn terminate(p: &Paths, key: &str, expected: Option<u32>) -> bool {
    let info = read_info(&p.info);
    if expected.is_some_and(|e| info.pid.is_some_and(|pid| pid != e)) {
        return true;
    }
    let is_daemon = |pid: u32| is_daemon(pid, key);
    let signal = |pid: u32, sig: i32| {
        // SAFETY: plain syscall; the pid was checked to be ours just before.
        unsafe { libc::kill(pid as i32, sig) };
    };
    if let Some(pid) = expected.or(info.pid).filter(|pid| is_daemon(*pid)) {
        let worker = info.worker_pid.filter(|w| {
            ps(*w, "ppid").is_some_and(|pp| pp == pid.to_string())
                && ps(*w, "command").is_some_and(|c| c.contains("scpm.worker.Main"))
        });
        signal(pid, libc::SIGTERM);
        // A stopped process acts on SIGTERM once continued.
        signal(pid, libc::SIGCONT);
        if !wait_gone(p, key, Some(pid), Duration::from_secs(2)) && is_daemon(pid) {
            signal(pid, libc::SIGKILL);
            wait_gone(p, key, Some(pid), Duration::from_secs(2));
        }
        if let Some(w) = worker
            && ps(w, "command").is_some_and(|c| c.contains("scpm.worker.Main"))
        {
            signal(w, libc::SIGKILL);
        }
    }
    let Some(_lock) = hold_unless_running(p, |_| {}) else {
        // Held by a replacement instance: ours is gone, but its files are the replacement's.
        return expected.is_some_and(|e| !is_daemon(e) || replaced(p, e));
    };
    let _ = fs::remove_file(&p.sock);
    let _ = fs::remove_file(&p.info);
    true
}

/// Takes the daemon's lock if no daemon holds it (`Some(None)` when there is no lock file);
/// while it is held, no daemon can start. Calls `running` with the daemon's info otherwise.
#[cfg(unix)]
fn hold_unless_running(p: &Paths, running: impl FnOnce(DaemonInfo)) -> Option<Option<File>> {
    match try_lock(&p.lock, false) {
        Ok(None) => {
            running(read_info(&p.info));
            None
        }
        Ok(Some(file)) => Some(Some(file)),
        Err(_) => Some(None),
    }
}

/// Daemon logs, most recently written first.
pub fn logs() -> Vec<PathBuf> {
    let Ok(dir) = dir() else { return Vec::new() };
    let mut logs: Vec<(SystemTime, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    logs.sort_by_key(|a| std::cmp::Reverse(a.0));
    logs.into_iter().map(|(_, p)| p).collect()
}

/// Arguments of `scpm daemon serve`.
pub struct ServeArgs {
    pub key: String,
    pub classpath: Vec<PathBuf>,
    pub jvm_options: Vec<String>,
}

/// Requests of one client: client id → worker id (once sent), and cancels that arrived
/// before the worker id was known.
#[derive(Default)]
struct Requests {
    ids: HashMap<u64, Option<u64>>,
    early_cancels: HashSet<u64>,
}

/// One connected client. Messages to it go through a bounded queue drained by its own
/// writer thread, so a slow client never holds up the worker's output.
#[cfg(unix)]
struct Client {
    stream: std::os::unix::net::UnixStream,
    queue: std::sync::mpsc::SyncSender<String>,
    dead: AtomicBool,
    requests: Mutex<Requests>,
}

#[cfg(unix)]
impl Client {
    fn send(&self, msg: Value) {
        if self.dead.load(Ordering::SeqCst) {
            return;
        }
        if self.queue.try_send(msg.to_string()).is_err() {
            self.disconnect("it is not reading its messages");
        }
    }

    /// Closes the connection; the read loop then ends and cancels the client's requests.
    fn disconnect(&self, why: &str) {
        if !self.dead.swap(true, Ordering::SeqCst) && !why.is_empty() {
            eprintln!("disconnecting a client: {why}");
        }
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Shared state of a running daemon.
struct State {
    key: String,
    worker: Worker,
    init: Value,
    jvm_options: Vec<String>,
    sock: PathBuf,
    log: PathBuf,
    started: Instant,
    last_activity: Mutex<Instant>,
    active: AtomicU64,
    requests: AtomicU64,
    clients: AtomicU64,
    clients_total: AtomicU64,
    /// Most recent first.
    workspaces: Mutex<VecDeque<String>>,
    stopping: AtomicBool,
    cleanup: Box<dyn Fn(bool) + Send + Sync>,
    /// (dev, inode) of the socket we bound.
    bound: (u64, u64),
}

impl State {
    fn touch(&self) {
        *self.last_activity.lock().expect("activity") = Instant::now();
    }

    fn status(&self) -> Value {
        let heap = self.jvm_options.iter().find_map(|o| o.strip_prefix("-Xmx")).unwrap_or_default();
        json!({
            "key": self.key,
            "state": "running",
            "pid": std::process::id(),
            "scpmVersion": env!("CARGO_PKG_VERSION"),
            "uptimeSeconds": self.started.elapsed().as_secs(),
            "idleSeconds": if self.active.load(Ordering::SeqCst) > 0 {
                0
            } else {
                self.last_activity.lock().expect("activity").elapsed().as_secs()
            },
            "activeRequests": self.active.load(Ordering::SeqCst),
            "requests": self.requests.load(Ordering::SeqCst),
            // Connected clients, not counting the one asking.
            "clients": self.clients.load(Ordering::SeqCst).saturating_sub(1),
            "clientsTotal": self.clients_total.load(Ordering::SeqCst),
            "workspaces": *self.workspaces.lock().expect("workspaces"),
            "heap": heap,
            "jvmOptions": self.jvm_options,
            "workerPid": self.init["pid"],
            "socket": self.sock,
            "log": self.log,
            "worker": self.init,
        })
    }

    /// Waits up to `limit` for the active requests to finish.
    fn wait_idle(&self, limit: Duration) -> bool {
        let started = Instant::now();
        while self.active.load(Ordering::SeqCst) > 0 {
            if started.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    /// Removes the socket (new clients start a new daemon), stops the worker and exits.
    fn exit(&self, code: i32, why: &str) -> ! {
        eprintln!("{why}; daemon stopping");
        (self.cleanup)(true);
        // Let client writers flush the last responses.
        std::thread::sleep(Duration::from_millis(200));
        self.worker.close(Some(Duration::from_secs(2)));
        std::process::exit(code)
    }

    /// `daemon/stop`: lets active requests finish (up to 30 s) unless `force`.
    fn stop(&self, force: bool) -> ! {
        (self.cleanup)(false);
        if force {
            self.worker.kill("was stopped (scpm daemon stop --force)");
        }
        if !self.wait_idle(STOP_WAIT) {
            eprintln!("requests still active after {}s", STOP_WAIT.as_secs());
            self.worker.kill("was stopped (scpm daemon stop)");
            self.wait_idle(Duration::from_secs(1));
        }
        self.exit(0, "stop requested")
    }
}

/// Spawns a thread; false if the system can't create one (it is then not run).
fn spawn(f: impl FnOnce() + Send + 'static) -> bool {
    std::thread::Builder::new().spawn(f).is_ok()
}

/// Whether `peer_is_us` can really check the peer on this target.
const PEER_CHECKED: bool = cfg!(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "linux",
    target_os = "android"
));

/// Whether the peer of `conn` runs as the current user.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
fn peer_is_us(conn: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: valid fd; the out-pointers live for the call.
    unsafe { libc::getpeereid(conn.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == libc::getuid() }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_is_us(conn: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: ucred is plain data; getsockopt writes at most `len` bytes into it.
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        ) == 0
            && cred.uid == libc::getuid()
    }
}

/// Elsewhere the directory's permissions (0700) keep other users out.
#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux",
        target_os = "android"
    ))
))]
fn peer_is_us(_: &std::os::unix::net::UnixStream) -> bool {
    // Can't check: fail closed (management connections such as `daemon ls|stop` too).
    false
}

/// Rotates the log (keeping one `.log.1`) and, unless run from a terminal, sends this
/// process's output to it.
#[cfg(unix)]
fn redirect_output(log: &Path) {
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;
    if fs::metadata(log).is_ok_and(|m| m.len() > 0) {
        let _ = fs::rename(log, log.with_extension("log.1"));
    }
    if std::io::stderr().is_terminal() {
        return;
    }
    if let Ok(f) = worker::open_log(log) {
        // SAFETY: both fds are valid; dup2 replaces stdout/stderr atomically.
        unsafe {
            libc::dup2(f.as_raw_fd(), 1);
            libc::dup2(f.as_raw_fd(), 2);
        }
    }
}

/// Runs a daemon (the hidden `scpm daemon serve`).
#[cfg(unix)]
pub fn serve(args: ServeArgs) -> Result<(), WorkerError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = dir()?;
    let p = Paths::new(&dir, &args.key);
    // The lock is held for the daemon's lifetime: whoever holds it is the only daemon for
    // this key. Wait briefly for an exiting one to let go.
    let started = Instant::now();
    let _lock = loop {
        match try_lock(&p.lock, true).map_err(|e| setup(&p.lock, e))? {
            Some(lock) => break lock,
            // A reachable daemon owns this key: don't linger to replace it when it exits
            // later (a stopping daemon removes its socket first, so that case still waits).
            None if UnixStream::connect(&p.sock).is_ok() => return Ok(()),
            // A stopping daemon may take STOP_WAIT to finish its compiles.
            None if started.elapsed() < STOP_WAIT + Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            // Another process is (or is becoming) this key's daemon.
            None => return Ok(()),
        }
    };
    redirect_output(&p.log);
    if UnixStream::connect(&p.sock).is_ok() {
        eprintln!("a daemon is already serving {}", p.sock.display());
        return Ok(());
    }
    let _ = fs::remove_file(&p.sock);
    let mut info = DaemonInfo {
        key: args.key.clone(),
        scpm_version: env!("CARGO_PKG_VERSION").into(),
        classpath: args.classpath.clone(),
        jvm_options: args.jvm_options.clone(),
        pid: Some(std::process::id()),
        worker_pid: None,
        ready: false,
        started: SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()),
    };
    // Before binding: a client that connects must see a starting daemon (and its longer
    // timeout), never a socket without info.
    write_info(&p.info, &info);
    let ours = |path: &Path| read_info(path).pid == Some(std::process::id());
    let listener = match UnixListener::bind(&p.sock) {
        Ok(l) => l,
        Err(e) => {
            if ours(&p.info) {
                let _ = fs::remove_file(&p.info);
            }
            return Err(setup(&p.sock, e));
        }
    };
    fs::set_permissions(&p.sock, fs::Permissions::from_mode(0o600)).map_err(|e| setup(&p.sock, e))?;
    let bound = fs::metadata(&p.sock).map(|m| (m.dev(), m.ino())).map_err(|e| setup(&p.sock, e))?;
    let _ = fs::remove_file(&p.failed);
    // Removes the socket if it is still the one we bound (new clients then start a new
    // daemon) and, when `exiting`, the info file if it is still ours. A draining daemon keeps
    // its info so `ls` and `stop` see it as stopping.
    let cleanup = {
        let (sock, info) = (p.sock.clone(), p.info.clone());
        move |exiting: bool| {
            if fs::metadata(&sock).is_ok_and(|m| (m.dev(), m.ino()) == bound) {
                let _ = fs::remove_file(&sock);
            }
            if exiting && ours(&info) {
                let _ = fs::remove_file(&info);
            }
        }
    };

    let spec = worker::Spawn {
        name: "the compile worker",
        classpath: &args.classpath,
        main: "scpm.worker.Main",
        jvm_options: &args.jvm_options,
        env: Vec::new(),
        cwd: None,
        log_file: Some(&p.log),
        // Initialized below, once its pid is on record (so `stop` can find it meanwhile).
        initialize: false,
        own_process_group: false,
        clear_env: false,
    };
    let worker = Worker::spawn(spec, |_| {}).and_then(|w| {
        info.worker_pid = w.pid();
        write_info(&p.info, &info);
        let init = w.initialize(None)?;
        Ok((w, init))
    });
    let (worker, init) = match worker {
        Ok(w) => w,
        Err(e) => {
            cleanup(true);
            return Err(e);
        }
    };
    info.ready = true;
    write_info(&p.info, &info);
    let state = Arc::new(State {
        key: args.key.clone(),
        worker,
        init,
        jvm_options: args.jvm_options,
        sock: p.sock.clone(),
        log: p.log.clone(),
        started: Instant::now(),
        last_activity: Mutex::new(Instant::now()),
        active: AtomicU64::new(0),
        requests: AtomicU64::new(0),
        clients: AtomicU64::new(0),
        clients_total: AtomicU64::new(0),
        workspaces: Mutex::new(VecDeque::new()),
        stopping: AtomicBool::new(false),
        cleanup: Box::new(cleanup),
        bound,
    });
    eprintln!("scpm daemon {} serving on {} (pid {})", args.key, p.sock.display(), std::process::id());

    // Exit when idle long enough, or when the worker JVM dies.
    let idle = std::env::var("SCPM_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(DEFAULT_IDLE, Duration::from_secs);
    {
        let state = state.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                if state.stopping.load(Ordering::SeqCst) {
                    continue;
                }
                // Someone removed or replaced our socket: nobody can reach us any more.
                // Each exit path claims the stop first; a graceful stop that got there first
                // (and removed the socket itself) is left to finish.
                if !fs::metadata(&state.sock).is_ok_and(|m| (m.dev(), m.ino()) == state.bound) {
                    if state.stopping.swap(true, Ordering::SeqCst) {
                        continue;
                    }
                    state.exit(0, "the socket was removed");
                }
                if !state.worker.is_alive() {
                    if state.stopping.swap(true, Ordering::SeqCst) {
                        continue;
                    }
                    // Fail what is still waiting, and give the answers a moment to go out.
                    state.worker.kill("exited unexpectedly");
                    state.wait_idle(Duration::from_secs(1));
                    state.exit(1, "the compile worker exited");
                }
                let last = state.last_activity.lock().expect("activity");
                if state.active.load(Ordering::SeqCst) == 0
                    && last.elapsed() >= idle
                    && !state.stopping.swap(true, Ordering::SeqCst)
                {
                    let secs = last.elapsed().as_secs();
                    drop(last);
                    state.exit(0, &format!("idle for {secs}s"));
                }
            }
        });
    }

    let mut backoff = Duration::from_millis(10);
    loop {
        let conn = match listener.accept() {
            Ok((conn, _)) => {
                backoff = Duration::from_millis(10);
                conn
            }
            Err(e) => {
                // E.g. out of file descriptors: back off instead of spinning.
                eprintln!("accept failed: {e}");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(1));
                continue;
            }
        };
        if !peer_is_us(&conn) {
            eprintln!("refused a connection from another user");
            continue;
        }
        if state.stopping.load(Ordering::SeqCst) {
            // Closing it (before `initialize`) tells the client to start a new daemon.
            continue;
        }
        state.clients.fetch_add(1, Ordering::SeqCst);
        state.clients_total.fetch_add(1, Ordering::SeqCst);
        let st = state.clone();
        if !spawn(move || serve_client(st, conn)) {
            state.clients.fetch_sub(1, Ordering::SeqCst);
            std::thread::sleep(backoff);
        }
    }
}

#[cfg(not(unix))]
pub fn serve(_: ServeArgs) -> Result<(), WorkerError> {
    Err(WorkerError::Setup("the shared daemon needs Unix sockets".into()))
}

/// Writes queued messages to a client until it is gone.
#[cfg(unix)]
fn write_loop(client: Arc<Client>, mut stream: std::os::unix::net::UnixStream, rx: std::sync::mpsc::Receiver<String>) {
    use std::io::Write;
    use std::sync::mpsc::RecvTimeoutError;
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(mut line) => {
                line.push('\n');
                if let Err(e) = stream.write_all(line.as_bytes()) {
                    client.disconnect(&format!("cannot write to it: {e}"));
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) if !client.dead.load(Ordering::SeqCst) => {}
            Err(_) => return,
        }
    }
}

/// Serves one client connection until it closes; then cancels what still runs for it.
#[cfg(unix)]
fn serve_client(state: Arc<State>, conn: std::os::unix::net::UnixStream) {
    use std::io::{BufRead, BufReader, Read};

    let (Ok(out), Ok(handle)) = (conn.try_clone(), conn.try_clone()) else {
        state.clients.fetch_sub(1, Ordering::SeqCst);
        return;
    };
    let _ = out.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
    let (tx, rx) = std::sync::mpsc::sync_channel(CLIENT_QUEUE);
    let client =
        Arc::new(Client { stream: handle, queue: tx, dead: AtomicBool::new(false), requests: Mutex::default() });
    {
        let writer = client.clone();
        if !spawn(move || write_loop(writer, out, rx)) {
            client.disconnect("no thread for it");
        }
    }
    let mut reader = BufReader::new(conn);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) if n as u64 >= MAX_LINE && !buf.ends_with(b"\n") => {
                client.disconnect("its request is too long");
                break;
            }
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
        let Some(cid) = msg["id"].as_u64() else { continue };
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        // Only clients that compile keep the daemon alive (not `daemon ls`).
        match method.as_str() {
            "initialize" => {
                // The same check as the worker's own: a client speaking another protocol gets
                // `unsupported_protocol`, not the cached result.
                if params["protocolVersion"].as_u64() != Some(worker::PROTOCOL_VERSION) {
                    client.send(json!({"id": cid, "error": {
                        "code": "unsupported_protocol",
                        "message": format!("the scpm daemon speaks protocol {}", worker::PROTOCOL_VERSION),
                    }}));
                    continue;
                }
                state.touch();
                client.send(json!({"id": cid, "result": state.init}));
            }
            "shutdown" => client.send(json!({"id": cid, "result": {}})),
            "daemon/status" => client.send(json!({"id": cid, "result": state.status()})),
            "daemon/stop" => {
                let active = state.active.load(Ordering::SeqCst);
                client.send(json!({"id": cid, "result": {"activeRequests": active}}));
                if !state.stopping.swap(true, Ordering::SeqCst) {
                    let state = state.clone();
                    let force = params["force"].as_bool().unwrap_or(false);
                    std::thread::spawn(move || state.stop(force));
                }
            }
            "cancel" => {
                let wid = params["requestId"].as_u64().and_then(|c| {
                    let mut r = client.requests.lock().expect("requests");
                    match r.ids.get(&c).copied() {
                        Some(Some(wid)) => Some(wid),
                        // Not sent to the worker yet: cancel it as soon as it is.
                        Some(None) => {
                            r.early_cancels.insert(c);
                            None
                        }
                        None => None,
                    }
                });
                if let Some(wid) = wid {
                    state.worker.send_cancel(wid);
                }
                client.send(json!({"id": cid, "result": {}}));
            }
            "compile" => {
                // Counted before the check: either this compile sees `stopping`, or `stop`
                // sees it active and waits for it.
                {
                    // Under the lock the idle monitor holds while it decides to exit.
                    let mut last = state.last_activity.lock().expect("activity");
                    *last = Instant::now();
                    state.active.fetch_add(1, Ordering::SeqCst);
                }
                if state.stopping.load(Ordering::SeqCst) {
                    state.active.fetch_sub(1, Ordering::SeqCst);
                    client.send(
                        json!({"id": cid, "error": {"code": "unavailable", "message": "the daemon is stopping"}}),
                    );
                    continue;
                }
                if let Some(ws) = params.pointer("/rootPaths/WS").and_then(Value::as_str) {
                    let mut w = state.workspaces.lock().expect("workspaces");
                    w.retain(|x| x != ws);
                    w.push_front(ws.to_string());
                    w.truncate(MAX_WORKSPACES);
                }
                client.requests.lock().expect("requests").ids.insert(cid, None);
                state.requests.fetch_add(1, Ordering::SeqCst);
                let (st, cl) = (state.clone(), client.clone());
                if !spawn(move || compile(st, cl, cid, params)) {
                    client.requests.lock().expect("requests").ids.remove(&cid);
                    state.active.fetch_sub(1, Ordering::SeqCst);
                    client.send(
                        json!({"id": cid, "error": {"code": "internal", "message": "the daemon is out of threads"}}),
                    );
                }
            }
            other => client.send(
                json!({"id": cid, "error": {"code": "unknown_method", "message": format!("unknown method `{other}`")}}),
            ),
        }
    }
    // The client is gone: cancel its requests (one that isn't sent yet is cancelled when it is).
    client.disconnect("");
    state.clients.fetch_sub(1, Ordering::SeqCst);
    let in_flight: Vec<u64> = client.requests.lock().expect("requests").ids.values().flatten().copied().collect();
    if !in_flight.is_empty() {
        eprintln!("a client went away; cancelling its {} request(s)", in_flight.len());
    }
    for wid in in_flight {
        state.worker.send_cancel(wid);
    }
}

/// Runs one client's compile on the worker, routing its events back to the client.
#[cfg(unix)]
fn compile(state: Arc<State>, client: Arc<Client>, cid: u64, params: Value) {
    let sink: worker::EventSink = {
        let client = client.clone();
        Arc::new(move |mut event: Value| {
            event["requestId"] = cid.into();
            client.send(json!({"method": "event", "params": event}));
        })
    };
    let on_id = |wid: u64| {
        let cancel = {
            let mut r = client.requests.lock().expect("requests");
            r.ids.insert(cid, Some(wid));
            r.early_cancels.remove(&cid) || client.dead.load(Ordering::SeqCst)
        };
        if cancel {
            state.worker.send_cancel(wid);
        }
    };
    let reply = state.worker.request_routed("compile", params, Some(sink), on_id);
    {
        let mut r = client.requests.lock().expect("requests");
        r.ids.remove(&cid);
        r.early_cancels.remove(&cid);
    }
    client.send(match reply {
        Ok(result) => json!({"id": cid, "result": result}),
        Err(WorkerError::Remote { code, message }) => json!({"id": cid, "error": {"code": code, "message": message}}),
        Err(e) => json!({"id": cid, "error": {"code": "internal", "message": e.to_string()}}),
    });
    // Touched first: the idle timer must not see the end of a long compile as idle time.
    state.touch();
    state.active.fetch_sub(1, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_sizes() {
        assert_eq!(parse_size("4g"), Some(4 << 30));
        assert_eq!(parse_size("2048M"), Some(2048 << 20));
        assert_eq!(parse_size("1k"), None, "too small");
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("4gb"), None);
    }
}
