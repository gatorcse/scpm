//! Client for the JVM compile worker (`scpmd`) over JSON lines on stdio
//! (docs/worker-protocol.md). Phase 1 runs it in the foreground for one command.

use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

pub const PROTOCOL_VERSION: u64 = 1;

/// The oldest JDK the worker runs on (its bytecode targets 17).
pub const MIN_JAVA: u32 = 17;

/// The worker's third-party dependencies, pinned in the lock's boot section.
pub const COMPILE_WORKER_DEPS: &str = include_str!("../../../worker/deps/compile-worker.txt");
pub const TEST_RUNNER_DEPS: &str = include_str!("../../../worker/deps/test-runner.txt");

#[derive(Debug, Clone, thiserror::Error)]
pub enum WorkerError {
    #[error("{0}")]
    Setup(String),
    #[error("cannot start {java}: {message}")]
    Spawn { java: String, message: String },
    /// The process ended before answering; the message names it and includes context.
    #[error("{0}")]
    Exited(String),
    /// No answer in time.
    #[error("{0}")]
    Timeout(String),
    #[error("compile worker error ({code}): {message}")]
    Remote { code: String, message: String },
}

/// Locates a helper jar: an explicit environment variable, or the sbt 2 build output of the
/// `worker/` project in a source checkout containing the running binary.
pub fn helper_jar(env_var: &str, project: &str) -> Result<PathBuf, WorkerError> {
    if let Some(path) = env::var_os(env_var).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(WorkerError::Setup(format!("{env_var} points to {}, which does not exist", path.display())));
        }
        return Ok(path);
    }
    let exe = env::current_exe().map_err(|e| WorkerError::Setup(e.to_string()))?;
    for dir in exe.ancestors() {
        let out = dir.join("worker/target/out/jvm");
        if let Some(jar) = find_jar(&out, project) {
            return Ok(jar);
        }
    }
    Err(WorkerError::Setup(format!(
        "cannot find the {project} jar; build it with `sbt package` in worker/ or set {env_var}"
    )))
}

fn find_jar(out: &Path, project: &str) -> Option<PathBuf> {
    let mut found = Vec::new();
    for scala_dir in std::fs::read_dir(out).ok()?.flatten() {
        let dir = scala_dir.path().join(project);
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(project) && name.ends_with(".jar") && !name.ends_with("-tests.jar") {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found.pop()
}

/// The JDK used for helper JVMs and forked runs.
#[derive(Debug, Clone)]
pub struct Java {
    pub path: PathBuf,
    /// Where `path` came from, for error messages (e.g. `JAVA_HOME`).
    pub source: String,
    /// Full version, e.g. `25.0.2`.
    pub version: String,
    pub major: u32,
}

/// The first executable file named `name` in the directories of `path` (a `PATH` value),
/// skipping non-executable entries the way process spawning does.
pub fn find_in_path(path: &std::ffi::OsStr, name: &Path) -> Option<PathBuf> {
    // Absolute (symlinks kept): a relative `PATH` entry must name the same file after a child
    // is started in another directory (test JVMs run in their module directory).
    env::split_paths(path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
        .map(|p| std::path::absolute(&p).unwrap_or(p))
}

/// [`find_in_path`] on `$PATH`.
pub fn find_on_path(name: &Path) -> Option<PathBuf> {
    find_in_path(&env::var_os("PATH")?, name)
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// A bare executable name resolved on `PATH` (so the probe, the launch and fingerprints all use
/// the same file); paths are kept as they are.
fn on_path(exe: PathBuf) -> PathBuf {
    if exe.components().count() == 1 { find_on_path(&exe).unwrap_or(exe) } else { exe }
}

/// Opens a log for appending, refusing a symlink planted at its path.
pub fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// Finds `java`: `SCPM_JAVA_HOME`, `JAVA_HOME`, then `PATH`; checks its version once per process.
pub fn java() -> Result<Java, WorkerError> {
    static JAVA: OnceLock<Result<Java, WorkerError>> = OnceLock::new();
    JAVA.get_or_init(detect_java).clone()
}

fn detect_java() -> Result<Java, WorkerError> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    // `SCPM_JAVA` names the executable itself (the daemon gets the exact one its key was
    // computed for, which needn't be `<home>/bin/java`); otherwise `<home>/bin/java`, then PATH.
    let explicit =
        env::var_os("SCPM_JAVA").filter(|j| !j.is_empty()).map(|j| (PathBuf::from(j), "$SCPM_JAVA".to_string()));
    let (path, source) = explicit
        .or_else(|| {
            ["SCPM_JAVA_HOME", "JAVA_HOME"].iter().find_map(|var| {
                let home = env::var_os(var).filter(|h| !h.is_empty())?;
                Some((Path::new(&home).join("bin").join(exe), format!("${var}")))
            })
        })
        .unwrap_or_else(|| (PathBuf::from(exe), "PATH".to_string()));
    let path = on_path(path);
    let version = java_version(&path, &source)?;
    let major = major_version(&version);
    if major < MIN_JAVA {
        return Err(WorkerError::Setup(format!(
            "{} (from {source}) is Java {}; scpm needs JDK {MIN_JAVA} or newer (set SCPM_JAVA_HOME)",
            path.display(),
            if version.is_empty() { "of unknown version".to_string() } else { version }
        )));
    }
    Ok(Java { path, source, version, major })
}

/// The version `java -version` reports for `java`, read from its JDK's `release` file when it
/// has a usable one (no JVM start); otherwise by running `java -version`.
fn java_version(java: &Path, source: &str) -> Result<String, WorkerError> {
    if let Some(version) = release_version(java) {
        return Ok(version);
    }
    let out = Command::new(java).arg("-version").output().map_err(|e| WorkerError::Spawn {
        java: format!("{} (from {source})", java.display()),
        message: format!("{e}; install a JDK {MIN_JAVA}+ or set SCPM_JAVA_HOME / JAVA_HOME"),
    })?;
    // `openjdk version "25.0.2" 2026-01-20` or `java version "1.8.0_402"`.
    // The ` version "…"` line (other output, e.g. from JAVA_TOOL_OPTIONS, may contain quotes).
    let text = String::from_utf8_lossy(&out.stderr);
    Ok(parse_java_version(&text).unwrap_or_default())
}

/// `25` from `25.0.2` or `25-ea`, `8` from `1.8.0_402`; 0 if there is no number.
fn major_version(version: &str) -> u32 {
    match version.split(['.', '-', '+']).collect::<Vec<_>>().as_slice() {
        ["1", minor, ..] => minor.parse().unwrap_or(0),
        [major, ..] => major.parse().unwrap_or(0),
        _ => 0,
    }
}

/// The version in the `release` file of the JDK `java` belongs to: `<home>/release`, where
/// `<home>/bin/java` is `java` with a bare name looked up on `PATH` (as spawning does) and
/// symlinks resolved. `None` when `java` is not an executable in a `bin` directory or the file
/// is missing or has no usable `JAVA_VERSION`; a wrapper script outside a JDK has none.
fn release_version(java: &Path) -> Option<String> {
    let java = if java.components().count() == 1 { find_on_path(java)? } else { java.to_path_buf() };
    let java = java.canonicalize().ok().filter(|p| is_executable(p))?;
    let bin = java.parent().filter(|b| b.file_name().is_some_and(|n| n == "bin"))?;
    parse_release(&std::fs::read_to_string(bin.parent()?.join("release")).ok()?)
}

/// What `java -version` would report, from a JDK `release` file: `JAVA_VERSION` (`25.0.2`,
/// `21`, `1.8.0_402`; quoted or not), plus the pre-release tag that `java -version` shows and
/// `JAVA_VERSION` omits (`25-ea`), taken from `JAVA_RUNTIME_VERSION` (`25-ea+3-2345`).
/// `None` unless `JAVA_VERSION` starts with a version number.
fn parse_release(text: &str) -> Option<String> {
    let value = |key: &str| {
        text.lines().find_map(|l| {
            let v = l.trim().strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
            let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
            Some(v.trim().to_string())
        })
    };
    let version = value("JAVA_VERSION")?;
    if major_version(&version) == 0 {
        return None;
    }
    // JEP 223: `$VNUM(-$PRE)?(+$BUILD)?(-$OPT)?`; `java -version` prints `$VNUM(-$PRE)?`.
    // Pre-9 strings (`1.8.0_402-b06`) have no `$PRE`.
    let pre = value("JAVA_RUNTIME_VERSION").filter(|_| !version.starts_with("1.")).and_then(|full| {
        let rest = full.split('+').next()?.strip_prefix(version.as_str())?.strip_prefix('-')?;
        let pre = rest.split('-').next()?;
        (!pre.is_empty() && pre.chars().all(|c| c.is_ascii_alphanumeric())).then(|| pre.to_string())
    });
    Some(match pre {
        Some(pre) => format!("{version}-{pre}"),
        None => version,
    })
}

/// `25.0.2` from `openjdk version "25.0.2" 2026-01-20` (and `java -version` noise around it).
fn parse_java_version(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|l| l.split_once(" version \""))
        .and_then(|(_, rest)| rest.split('"').next())
        .map(str::to_string)
}

/// The executable `SCPM_JAVA` names, as `detect_java` runs it: a bare name is looked up on
/// `path`, and symlinks are resolved. `None` when `SCPM_JAVA` is unset or empty.
fn explicit_java(explicit: Option<OsString>, path: Option<&OsString>) -> Option<PathBuf> {
    let exe = PathBuf::from(explicit.filter(|j| !j.is_empty())?);
    let exe = if exe.components().count() == 1 {
        // Executable-aware, like process spawning: a non-executable `java` earlier on PATH is skipped.
        path.and_then(|p| find_in_path(p, &exe)).unwrap_or(exe)
    } else {
        exe
    };
    Some(exe.canonicalize().unwrap_or(exe))
}

/// The home directory of the JDK scpm runs, found without starting a JVM, in `detect_java`'s
/// order: the `SCPM_JAVA` executable's home, `SCPM_JAVA_HOME`, `JAVA_HOME`, then the `java` on
/// `PATH` with symlinks resolved. `None` if there is none.
pub fn java_home() -> Option<PathBuf> {
    java_home_from(
        env::var_os("SCPM_JAVA"),
        &[env::var_os("SCPM_JAVA_HOME"), env::var_os("JAVA_HOME")],
        env::var_os("PATH"),
    )
}

fn java_home_from(explicit: Option<OsString>, homes: &[Option<OsString>], path: Option<OsString>) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    // `<home>/bin/java`; a version manager's shim resolves to the manager, not a JDK.
    let home_of = |java: &Path| {
        java.parent()?.parent().map(Path::to_path_buf).filter(|home| home.join("bin").join(exe).is_file())
    };
    let from_explicit = explicit_java(explicit, path.as_ref()).and_then(|java| home_of(&java));
    let from_env = || {
        homes
            .iter()
            .flatten()
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .find(|home| home.join("bin").join(exe).is_file())
    };
    let from_path = || {
        env::split_paths(path.as_ref()?)
            .map(|d| d.join(exe))
            .find(|p| p.is_file())
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| home_of(&p))
            .filter(|home| home.join("release").is_file())
    };
    from_explicit.or_else(from_env).or_else(from_path).map(|home| home.canonicalize().unwrap_or(home))
}

/// An identity of the JDK for build fingerprints, found without starting a JVM when possible:
/// the `release` file of the JDK that `detect_java` uses (`SCPM_JAVA`, `SCPM_JAVA_HOME`,
/// `JAVA_HOME`, or the resolved `java` on `PATH`). Falls back to running `java -version`, never
/// to another JDK's `release` file.
pub fn java_identity() -> Result<String, WorkerError> {
    // Same precedence as `detect_java`: an explicit `SCPM_JAVA` executable comes first, so
    // switching it changes the fingerprint even while `JAVA_HOME` is set.
    // A bare name runs whatever PATH finds, so identify that, not the literal name.
    if let Some(real) = explicit_java(env::var_os("SCPM_JAVA"), env::var_os("PATH").as_ref()) {
        let release = real.parent().and_then(Path::parent).and_then(|home| {
            let text = std::fs::read_to_string(home.join("release")).ok()?;
            text.lines().find(|l| l.starts_with("JAVA_VERSION=")).map(str::to_string)
        });
        return Ok(match release {
            Some(line) => format!("{} {line}", real.display()),
            None => format!("{} {}", real.display(), java()?.version),
        });
    }
    if let Some(id) = selected_java_home(|v| env::var_os(v)).as_deref().and_then(release_identity) {
        return Ok(id);
    }
    let java = java()?;
    Ok(format!("{} {}", java.path.display(), java.version))
}

/// The JDK home `detect_java` picks when `SCPM_JAVA` is unset, with the same precedence: the
/// first non-empty of `SCPM_JAVA_HOME` and `JAVA_HOME`, else the home of the `java` found on
/// `PATH`.
fn selected_java_home(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    if let Some(home) = ["SCPM_JAVA_HOME", "JAVA_HOME"].iter().find_map(|v| var(v).filter(|h| !h.is_empty())) {
        return Some(PathBuf::from(home));
    }
    find_in_path(&var("PATH")?, Path::new(exe))
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| p.parent()?.parent().map(Path::to_path_buf))
}

/// `<home> JAVA_VERSION="…"` from a JDK home's `release` file, if it has one.
fn release_identity(home: &Path) -> Option<String> {
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    let line = release.lines().find(|l| l.starts_with("JAVA_VERSION="))?;
    Some(format!("{} {line}", home.display()))
}

/// JVM flags for helper processes: keep HotSpot's own output off stdout (the protocol
/// channel), exit on OOM, and silence JDK 24+ warnings that would otherwise fill stderr.
fn helper_jvm_flags(java: &Java) -> Vec<&'static str> {
    let mut flags = vec![
        "-XX:+ExitOnOutOfMemoryError",
        "-XX:+DisplayVMOutputToStderr",
        "-Xlog:disable",
        "-Xlog:all=warning:stderr",
        "--enable-native-access=ALL-UNNAMED",
    ];
    if java.major >= 23 {
        flags.push("--sun-misc-unsafe-memory-access=allow");
    }
    flags
}

type Reply = Result<Value, WorkerError>;

/// How to start a helper JVM.
pub struct Spawn<'a> {
    /// For messages: "the compile worker", "the test JVM".
    pub name: &'a str,
    pub classpath: &'a [PathBuf],
    pub main: &'a str,
    pub jvm_options: &'a [String],
    pub env: Vec<(String, String)>,
    pub cwd: Option<&'a Path>,
    pub log_file: Option<&'a Path>,
    /// Send `initialize` and check the protocol version (the compile worker does; the
    /// one-shot test runner doesn't).
    pub initialize: bool,
    /// Start in a new process group so `kill` also stops processes the JVM started.
    pub own_process_group: bool,
}

/// Requests waiting for a reply; `None` once the worker's stdout has closed.
type Pending = Arc<Mutex<Option<HashMap<u64, Sender<Reply>>>>>;

/// Receives the events of one routed request. It runs on the thread reading the worker's
/// output, so it must not block.
pub type EventSink = Arc<dyn Fn(Value) + Send + Sync>;

/// Worker stderr logs are truncated when they grow past this size.
const LOG_CAP: u64 = 10 * 1024 * 1024;

/// Per-request event sinks, keyed by request id (see [`Worker::request_routed`]).
type Sinks = Arc<Mutex<HashMap<u64, EventSink>>>;

/// A running worker: a child JVM on stdio, or a connection to the shared daemon. Requests may
/// be sent from several threads at once.
pub struct Worker {
    name: String,
    own_group: bool,
    /// `None` when connected to a daemon.
    child: Mutex<Option<Child>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    pending: Pending,
    sinks: Sinks,
    next_id: AtomicU64,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// The `initialize` result, once initialized.
    init: OnceLock<Value>,
    /// The connection, when connected to a daemon (to close it both ways).
    #[cfg(unix)]
    socket: Option<std::os::unix::net::UnixStream>,
}

fn push_tail(tail: &Mutex<VecDeque<String>>, line: String) {
    let mut t = tail.lock().expect("tail");
    if t.len() == 40 {
        t.pop_front();
    }
    t.push_back(line);
}

impl Worker {
    /// Starts `java <flags> <options> -cp <classpath> <main>`. Events go to `on_event`.
    pub fn spawn(spec: Spawn, on_event: impl Fn(Value) + Send + 'static) -> Result<Worker, WorkerError> {
        let java = java()?;
        let sep = if cfg!(windows) { ";" } else { ":" };
        // Relative entries (e.g. a relative `SCPM_TEST_RUNNER_JAR`) must name files relative to
        // where scpm runs, not to the child's `cwd`.
        let cp = spec.classpath.iter().map(|p| absolute(p).display().to_string()).collect::<Vec<_>>().join(sep);
        let mut cmd = Command::new(&java.path);
        cmd.args(helper_jvm_flags(&java)).args(spec.jvm_options).arg("-cp").arg(cp).arg(spec.main).envs(spec.env);
        if let Some(dir) = spec.cwd {
            cmd.current_dir(dir);
        }
        #[cfg(unix)]
        if spec.own_process_group {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child =
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| {
                WorkerError::Spawn {
                    java: format!("{} for {} (from {})", java.path.display(), spec.name, java.source),
                    message: e.to_string(),
                }
            })?;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let stdin = child.stdin.take().expect("piped");
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        {
            let tail = stderr_tail.clone();
            let mut log = spec.log_file.and_then(|p| {
                std::fs::create_dir_all(p.parent()?).ok()?;
                open_log(p).ok()
            });
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                let mut lines = 0u64;
                while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                    let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                    if let Some(log) = log.as_mut() {
                        lines += 1;
                        // Keep a chatty worker from filling the disk (the file is in append mode).
                        if lines.is_multiple_of(256) && log.metadata().is_ok_and(|m| m.len() > LOG_CAP) {
                            let _ = log.set_len(0);
                            let _ = writeln!(log, "[log truncated at {} MB]", LOG_CAP >> 20);
                        }
                        let _ = writeln!(log, "{line}");
                    }
                    push_tail(&tail, line);
                    buf.clear();
                }
            });
        }
        let mut worker = Worker::attach(spec.name, stdout, Box::new(stdin), Some(child), stderr_tail, on_event);
        worker.own_group = spec.own_process_group;
        if spec.initialize {
            worker.initialize(None)?;
        }
        Ok(worker)
    }

    /// Wraps a connection to a worker served on a Unix socket (the shared daemon) and, if
    /// `initialize`, initializes it, giving up after `timeout` without an answer.
    #[cfg(unix)]
    pub fn over_socket(
        name: &str,
        stream: std::os::unix::net::UnixStream,
        timeout: Duration,
        initialize: bool,
        on_event: impl Fn(Value) + Send + 'static,
    ) -> Result<Worker, WorkerError> {
        let setup = |e: std::io::Error| WorkerError::Setup(format!("{name}: {e}"));
        // A stalled peer must not block a writer forever.
        stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(setup)?;
        let reader = stream.try_clone().map_err(setup)?;
        let writer = stream.try_clone().map_err(setup)?;
        let mut worker =
            Worker::attach(name, reader, Box::new(writer), None, Arc::new(Mutex::new(VecDeque::new())), on_event);
        worker.socket = Some(stream);
        if initialize {
            worker.initialize(Some(timeout))?;
        }
        Ok(worker)
    }

    /// The worker's `initialize` result (absent if it wasn't initialized).
    pub fn init_result(&self) -> Option<&Value> {
        self.init.get()
    }

    /// Wires a worker around a transport: a thread reads responses and events from `reader`.
    fn attach(
        name: &str,
        reader: impl std::io::Read + Send + 'static,
        writer: Box<dyn Write + Send>,
        child: Option<Child>,
        stderr_tail: Arc<Mutex<VecDeque<String>>>,
        on_event: impl Fn(Value) + Send + 'static,
    ) -> Worker {
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let sinks: Sinks = Arc::new(Mutex::new(HashMap::new()));
        {
            let pending = pending.clone();
            let sinks = sinks.clone();
            let tail = stderr_tail.clone();
            let name = name.to_string();
            // One thread only reads, so the peer is never held up by slow event handling (a
            // terminal paused by `| less`); another handles messages in order, so a response
            // still comes after its request's events.
            let (tx, rx) = channel::<Value>();
            {
                let tail = tail.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(reader);
                    let mut buf = Vec::new();
                    while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                        match serde_json::from_slice::<Value>(&buf) {
                            Ok(msg) => {
                                if tx.send(msg).is_err() {
                                    break;
                                }
                            }
                            // Not JSON (shouldn't happen with the launch flags): keep it as context.
                            Err(_) => {
                                push_tail(&tail, format!("(stdout) {}", String::from_utf8_lossy(&buf).trim_end()))
                            }
                        }
                        buf.clear();
                    }
                });
            }
            std::thread::spawn(move || {
                for msg in rx {
                    match msg {
                        msg if msg.get("method").and_then(Value::as_str) == Some("event") => {
                            let params = msg["params"].clone();
                            // Called outside the lock: one sink must never hold up the others.
                            let sink = params["requestId"]
                                .as_u64()
                                .and_then(|id| sinks.lock().expect("sinks").get(&id).cloned());
                            match sink {
                                Some(sink) => sink(params),
                                None => on_event(params),
                            }
                        }
                        msg => {
                            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                                let reply = match msg.get("error") {
                                    Some(err) => Err(WorkerError::Remote {
                                        code: err["code"].as_str().unwrap_or("unknown").to_string(),
                                        message: err["message"].as_str().unwrap_or_default().to_string(),
                                    }),
                                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                                };
                                let sender = pending.lock().expect("pending").as_mut().and_then(|p| p.remove(&id));
                                if let Some(tx) = sender {
                                    let _ = tx.send(reply);
                                }
                            }
                        }
                    }
                }
                // EOF: fail everything still waiting, and refuse new requests.
                let context = format!("{name} exited unexpectedly{}", tail_context(&tail));
                let waiting = pending.lock().expect("pending").take().unwrap_or_default();
                for (_, tx) in waiting {
                    let _ = tx.send(Err(WorkerError::Exited(context.clone())));
                }
            });
        }
        Worker {
            name: name.to_string(),
            own_group: false,
            child: Mutex::new(child),
            writer: Mutex::new(Some(writer)),
            pending,
            sinks,
            next_id: AtomicU64::new(1),
            stderr_tail,
            init: OnceLock::new(),
            #[cfg(unix)]
            socket: None,
        }
    }

    /// The child process id (not for a daemon connection).
    pub fn pid(&self) -> Option<u32> {
        self.child.lock().expect("child").as_ref().map(Child::id)
    }

    /// Sends `initialize` and checks the protocol version (for a worker spawned without it).
    pub fn initialize(&self, timeout: Option<Duration>) -> Result<Value, WorkerError> {
        let init = self.request_timeout("initialize", json!({"protocolVersion": PROTOCOL_VERSION}), timeout)?;
        if init["protocolVersion"].as_u64() != Some(PROTOCOL_VERSION) {
            return Err(WorkerError::Setup(format!(
                "worker speaks protocol {}, expected {PROTOCOL_VERSION}",
                init["protocolVersion"]
            )));
        }
        let _ = self.init.set(init.clone());
        Ok(init)
    }

    /// Sends a request and blocks until its response.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, WorkerError> {
        self.send_request(method, params, None, |_| {}, None)
    }

    /// Like [`Worker::request`], but fails if there is no answer within `timeout`.
    pub fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, WorkerError> {
        self.send_request(method, params, None, |_| {}, timeout)
    }

    /// Sends a request whose events go to `sink` (instead of the worker-wide handler), and
    /// tells `on_id` the id it was sent with once it has been written (so a `cancel` sent
    /// from `on_id` on reaches the worker after the request). Blocks until the response.
    pub fn request_routed(
        &self,
        method: &str,
        params: Value,
        sink: Option<EventSink>,
        on_id: impl FnOnce(u64),
    ) -> Result<Value, WorkerError> {
        self.send_request(method, params, sink, on_id, None)
    }

    fn send_request(
        &self,
        method: &str,
        params: Value,
        sink: Option<EventSink>,
        on_id: impl FnOnce(u64),
        timeout: Option<Duration>,
    ) -> Result<Value, WorkerError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        // Release `pending` before `exited()`, which locks `child` (`is_alive` takes `child`
        // first): holding both in opposite orders deadlocks.
        let registered = match self.pending.lock().expect("pending").as_mut() {
            Some(p) => {
                p.insert(id, tx);
                true
            }
            None => false,
        };
        if !registered {
            return Err(self.exited());
        }
        if let Some(sink) = sink {
            self.sinks.lock().expect("sinks").insert(id, sink);
        }
        let reply = if self.write_line(&json!({"id": id, "method": method, "params": params})) {
            on_id(id);
            let received = match timeout {
                Some(t) => rx.recv_timeout(t),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                // Closed stdout: add the exit status and stderr context. Other failures (a
                // kill with its own reason) pass through.
                Ok(Err(WorkerError::Exited(m))) if m.contains("exited unexpectedly") => Err(self.exited()),
                Ok(reply) => reply,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(p) = self.pending.lock().expect("pending").as_mut() {
                        p.remove(&id);
                    }
                    Err(WorkerError::Timeout(format!(
                        "{} did not answer `{method}` within {}s",
                        self.name,
                        timeout.unwrap_or_default().as_secs()
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => Err(self.exited()),
            }
        } else {
            if let Some(p) = self.pending.lock().expect("pending").as_mut() {
                p.remove(&id);
            }
            Err(self.exited())
        };
        self.sinks.lock().expect("sinks").remove(&id);
        reply
    }

    /// Writes one message. A failed (possibly partial) write leaves the stream unusable: it
    /// is closed and everything waiting on it fails.
    fn write_line(&self, msg: &Value) -> bool {
        let failed = {
            let mut writer = self.writer.lock().expect("writer");
            let Some(w) = writer.as_mut() else { return false };
            match writeln!(w, "{msg}").and_then(|()| w.flush()) {
                Ok(()) => return true,
                Err(e) => {
                    writer.take();
                    e
                }
            }
        };
        self.close_socket();
        let waiting = self.pending.lock().expect("pending").take().unwrap_or_default();
        for (_, tx) in waiting {
            let _ = tx.send(Err(WorkerError::Exited(format!("{} cannot be sent requests: {failed}", self.name))));
        }
        false
    }

    /// Asks the worker to cancel request `id`, without waiting for the answer.
    pub fn send_cancel(&self, id: u64) {
        let own = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.write_line(&json!({"id": own, "method": "cancel", "params": {"requestId": id}}));
    }

    /// Closes a daemon connection in both directions, so the reading thread ends and the
    /// daemon sees the client go away (and cancels what it still runs for it). For a child
    /// process, closes its stdin.
    fn close_socket(&self) {
        #[cfg(unix)]
        if let Some(s) = &self.socket {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        self.writer.lock().map(|mut w| w.take()).ok();
    }

    /// An `Exited` error with the exit status (if known) and the end of stderr.
    fn exited(&self) -> WorkerError {
        #[cfg(unix)]
        if self.socket.is_some() {
            return WorkerError::Exited(format!("{} closed the connection (see `scpm daemon logs`)", self.name));
        }
        let status = self.child.lock().ok().and_then(|mut c| {
            let c = c.as_mut()?;
            // Give the process a moment to finish exiting so its status is available.
            for _ in 0..20 {
                if let Ok(Some(s)) = c.try_wait() {
                    return Some(s);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None
        });
        let status = status.map(|s| format!(" ({s})")).unwrap_or_default();
        WorkerError::Exited(format!("{} exited unexpectedly{status}{}", self.name, tail_context(&self.stderr_tail)))
    }

    /// Whether the process is still running (or the daemon connection still open).
    pub fn is_alive(&self) -> bool {
        // One lock at a time: `child` and `pending` are never held together.
        let running = self.child.lock().expect("child").as_mut().is_none_or(|c| matches!(c.try_wait(), Ok(None)));
        running && self.pending.lock().expect("pending").is_some()
    }

    /// Kills the process (and its process group, if it has its own) and fails pending
    /// requests with `reason` right away, without waiting for stdout to close (a grandchild
    /// may keep it open). For a daemon connection, the requests in flight are cancelled and
    /// the connection is closed (the daemon stays up).
    pub fn kill(&self, reason: &str) {
        if let Some(child) = self.child.lock().expect("child").as_mut() {
            #[cfg(unix)]
            if self.own_group {
                // SAFETY: plain syscall on a pid we own; a negative pid targets its group.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill();
        } else {
            let in_flight: Vec<u64> =
                self.pending.lock().expect("pending").as_ref().map(|p| p.keys().copied().collect()).unwrap_or_default();
            for id in in_flight {
                self.send_cancel(id);
            }
            self.close_socket();
        }
        let waiting = self.pending.lock().expect("pending").take().unwrap_or_default();
        for (_, tx) in waiting {
            let _ = tx.send(Err(WorkerError::Exited(format!("{} {reason}", self.name))));
        }
    }

    /// Closes stdin and waits up to `limit` for the process to exit, killing it (and its group)
    /// after that. Returns the exit status if it exited by itself.
    pub fn wait_timeout(self, limit: Option<std::time::Duration>) -> Option<std::process::ExitStatus> {
        self.close(limit)
    }

    /// [`Worker::wait_timeout`] for a worker that may be shared.
    pub fn close(&self, limit: Option<std::time::Duration>) -> Option<std::process::ExitStatus> {
        self.writer.lock().expect("writer").take();
        let started = std::time::Instant::now();
        loop {
            if let Ok(Some(status)) = self.child.lock().expect("child").as_mut()?.try_wait() {
                return Some(status);
            }
            if limit.is_some_and(|l| started.elapsed() >= l) {
                self.kill("did not exit in time");
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Asks a child worker to exit and waits for it; a daemon connection is just closed (the
    /// daemon is shared and stays up).
    pub fn shutdown(self) {
        if self.child.lock().expect("child").is_some() {
            let _ = self.request("shutdown", json!({}));
        }
        self.close_socket();
        if let Some(c) = self.child.lock().expect("child").as_mut() {
            let _ = c.wait();
        }
    }
}

/// `p` resolved against the current directory (unchanged if absolute or if that fails).
fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn tail_context(tail: &Mutex<VecDeque<String>>) -> String {
    let t = tail.lock().expect("tail");
    if t.is_empty() { String::new() } else { format!(":\n{}", t.iter().cloned().collect::<Vec<_>>().join("\n")) }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing stdin is a clean shutdown for the worker; make sure it doesn't outlive us.
        // A daemon connection is closed both ways, so the daemon drops what it runs for us.
        self.close_socket();
        if let Ok(mut child) = self.child.lock()
            && let Some(child) = child.as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod version_tests {
    use super::{absolute, parse_java_version};

    #[test]
    #[cfg(unix)]
    fn path_lookup_skips_non_executable_files() {
        use std::os::unix::fs::PermissionsExt;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(a.path().join("java"), "not a program").unwrap();
        std::fs::write(b.path().join("java"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(b.path().join("java"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        assert_eq!(super::find_in_path(&path, std::path::Path::new("java")), Some(b.path().join("java")));
    }

    #[test]
    #[cfg(unix)]
    fn path_lookup_returns_absolute_paths_for_relative_entries() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        std::fs::write(dir.path().join("java"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir.path().join("java"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let relative = dir.path().strip_prefix(&cwd).unwrap();
        let found = super::find_in_path(relative.as_os_str(), std::path::Path::new("java")).unwrap();
        assert!(found.is_absolute(), "{}", found.display());
        assert_eq!(found, cwd.join(relative).join("java"));
    }

    #[test]
    fn java_versions_ignore_unrelated_quotes() {
        let out = "Picked up JAVA_TOOL_OPTIONS: -Dfoo=\"bar baz\"\nopenjdk version \"25.0.2\" 2026-01-20 LTS\n";
        assert_eq!(parse_java_version(out).as_deref(), Some("25.0.2"));
        assert_eq!(parse_java_version("java version \"1.8.0_402\"").as_deref(), Some("1.8.0_402"));
    }

    #[test]
    fn release_files_give_the_version_java_reports() {
        use super::parse_release;
        let v = parse_release;
        assert_eq!(v("IMPLEMENTOR=\"Eclipse Adoptium\"\nJAVA_VERSION=\"25.0.5\"\n").as_deref(), Some("25.0.5"));
        assert_eq!(v("JAVA_VERSION=\"17\"").as_deref(), Some("17"));
        assert_eq!(v("JAVA_VERSION=\"21.0.2\"\r\n").as_deref(), Some("21.0.2"));
        assert_eq!(v("JAVA_VERSION=25\n").as_deref(), Some("25"));
        assert_eq!(
            v("JAVA_VERSION=\"1.8.0_402\"\nJAVA_RUNTIME_VERSION=\"1.8.0_402-b06\"").as_deref(),
            Some("1.8.0_402")
        );
        // `JAVA_VERSION_DATE` is not `JAVA_VERSION`.
        assert_eq!(v("JAVA_VERSION_DATE=\"2026-01-20\"\nJAVA_VERSION=\"25.0.2\"").as_deref(), Some("25.0.2"));
        // GA builds: `java -version` prints `JAVA_VERSION`; pre-release builds add the tag.
        let ga = "JAVA_RUNTIME_VERSION=\"25.0.5.1+2-LTS\"\nJAVA_VERSION=\"25.0.5.1\"";
        assert_eq!(v(ga).as_deref(), Some("25.0.5.1"));
        assert_eq!(v("JAVA_VERSION=\"26\"\nJAVA_RUNTIME_VERSION=\"26-ea+3-2345\"").as_deref(), Some("26-ea"));
        assert_eq!(
            v("JAVA_VERSION=\"25\"\nJAVA_RUNTIME_VERSION=\"25-internal-adhoc.me.jdk\"").as_deref(),
            Some("25-internal")
        );
        // Nothing usable: the caller runs `java -version`.
        assert_eq!(v(""), None);
        assert_eq!(v("JAVA_VERSION=\"\""), None);
        assert_eq!(v("JAVA_VERSION=\"unknown\""), None);
        assert_eq!(v("IMPLEMENTOR=\"x\""), None);
    }

    #[cfg(unix)]
    #[test]
    fn java_version_reads_release_and_falls_back_to_the_probe() {
        use super::java_version;
        use std::os::unix::fs::PermissionsExt;
        // A fake JDK whose `java` reports 21.0.3 when run.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("bin")).unwrap();
        let java = home.path().join("bin/java");
        std::fs::write(&java, "#!/bin/sh\necho 'openjdk version \"21.0.3\" 2024-04-16' >&2\n").unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        // No `release` file: the probe runs.
        assert_eq!(java_version(&java, "$JAVA_HOME").unwrap(), "21.0.3");
        // A `release` file without a usable `JAVA_VERSION`: still the probe.
        std::fs::write(home.path().join("release"), "IMPLEMENTOR=\"x\"\n").unwrap();
        assert_eq!(java_version(&java, "$JAVA_HOME").unwrap(), "21.0.3");
        // With one, `java` is not started; also through a symlink from another `bin`.
        std::fs::write(home.path().join("release"), "JAVA_VERSION=\"25.0.2\"\n").unwrap();
        assert_eq!(java_version(&java, "$JAVA_HOME").unwrap(), "25.0.2");
        let link = tempfile::tempdir().unwrap();
        std::fs::create_dir(link.path().join("bin")).unwrap();
        std::os::unix::fs::symlink(&java, link.path().join("bin/java")).unwrap();
        assert_eq!(java_version(&link.path().join("bin/java"), "$JAVA_HOME").unwrap(), "25.0.2");
        // A non-executable `java` is not trusted: spawning reports it as before.
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = java_version(&java, "$JAVA_HOME").unwrap_err().to_string();
        assert!(err.contains("(from $JAVA_HOME)"), "{err}");
    }

    #[test]
    fn java_identity_reads_only_the_selected_home() {
        use super::{release_identity, selected_java_home};
        use std::ffi::OsString;
        let selected = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("release"), "JAVA_VERSION=\"21.0.1\"\n").unwrap();
        let env = |v: &str| match v {
            "SCPM_JAVA_HOME" => Some(OsString::from(selected.path())),
            "JAVA_HOME" => Some(OsString::from(other.path())),
            _ => None,
        };
        // SCPM_JAVA_HOME wins even though only JAVA_HOME has a release file.
        let home = selected_java_home(env).unwrap();
        assert_eq!(home, selected.path());
        assert_eq!(release_identity(&home), None);

        let env = |v: &str| (v == "JAVA_HOME").then(|| OsString::from(other.path()));
        let home = selected_java_home(env).unwrap();
        assert!(release_identity(&home).unwrap().ends_with("JAVA_VERSION=\"21.0.1\""));
    }

    #[test]
    fn relative_classpath_entries_resolve_against_the_current_directory() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolute(std::path::Path::new("tools/runner.jar")), cwd.join("tools/runner.jar"));
        let abs = cwd.join("x.jar");
        assert_eq!(absolute(&abs), abs);
    }
}

#[cfg(test)]
mod java_home_tests {
    use super::java_home_from;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A fake JDK at `dir/name`: `bin/java` and a `release` file.
    fn jdk(dir: &Path, name: &str) -> PathBuf {
        let home = dir.join(name);
        fs::create_dir_all(home.join("bin")).unwrap();
        let java = home.join("bin").join(if cfg!(windows) { "java.exe" } else { "java" });
        fs::write(&java, "").unwrap();
        // PATH lookup skips non-executable files, as process spawning does.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&java, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(home.join("release"), "JAVA_VERSION=\"21\"\n").unwrap();
        home.canonicalize().unwrap()
    }

    #[test]
    fn scpm_java_comes_before_the_home_variables() {
        let dir = tempfile::tempdir().unwrap();
        let (j21, j17) = (jdk(dir.path(), "jdk21"), jdk(dir.path(), "jdk17"));
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        let homes = [None, Some(OsString::from(&j17))];
        let explicit = Some(j21.join("bin").join(exe).into_os_string());
        assert_eq!(java_home_from(explicit, &homes, None), Some(j21.clone()));
        // A bare name resolves through PATH, as `detect_java` runs it.
        let path = Some(j21.join("bin").into_os_string());
        assert_eq!(java_home_from(Some(OsString::from(exe)), &homes, path.clone()), Some(j21.clone()));
        // Unset or unresolvable: the home variables, then PATH.
        assert_eq!(java_home_from(None, &homes, path.clone()), Some(j17));
        assert_eq!(java_home_from(Some("".into()), &[None, None], path.clone()), Some(j21));
        assert_eq!(java_home_from(Some("/nowhere/bin/java".into()), &[None, None], None), None);
    }
}
