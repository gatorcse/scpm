//! Client for the JVM compile worker (`scpmd`) over JSON lines on stdio
//! (docs/worker-protocol.md). Phase 1 runs it in the foreground for one command.

use std::collections::{HashMap, VecDeque};
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

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

/// Finds `java`: `SCPM_JAVA_HOME`, `JAVA_HOME`, then `PATH`; checks its version once per process.
pub fn java() -> Result<Java, WorkerError> {
    static JAVA: OnceLock<Result<Java, WorkerError>> = OnceLock::new();
    JAVA.get_or_init(detect_java).clone()
}

fn detect_java() -> Result<Java, WorkerError> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    let (path, source) = ["SCPM_JAVA_HOME", "JAVA_HOME"]
        .iter()
        .find_map(|var| {
            let home = env::var_os(var).filter(|h| !h.is_empty())?;
            Some((Path::new(&home).join("bin").join(exe), format!("${var}")))
        })
        .unwrap_or_else(|| (PathBuf::from(exe), "PATH".to_string()));
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
    let java = if java.components().count() == 1 {
        env::split_paths(&env::var_os("PATH")?).map(|d| d.join(java)).find(|p| runnable(p))?
    } else {
        java.to_path_buf()
    };
    let java = java.canonicalize().ok().filter(|p| runnable(p))?;
    let bin = java.parent().filter(|b| b.file_name().is_some_and(|n| n == "bin"))?;
    parse_release(&std::fs::read_to_string(bin.parent()?.join("release")).ok()?)
}

/// Whether spawning `p` can run it: an executable file.
fn runnable(p: &Path) -> bool {
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

/// An identity of the JDK for build fingerprints, found without starting a JVM when possible:
/// the `release` file of the JDK home that `detect_java` uses (`SCPM_JAVA_HOME`, `JAVA_HOME`,
/// or the resolved `java` on `PATH`). Falls back to running `java -version`, never to another
/// JDK's `release` file.
pub fn java_identity() -> Result<String, WorkerError> {
    if let Some(id) = selected_java_home(|v| env::var_os(v)).as_deref().and_then(release_identity) {
        return Ok(id);
    }
    let java = java()?;
    Ok(format!("{} {}", java.path.display(), java.version))
}

/// The JDK home `detect_java` picks, with the same precedence: the first non-empty of
/// `SCPM_JAVA_HOME` and `JAVA_HOME`, else the home of the `java` found on `PATH`.
fn selected_java_home(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    if let Some(home) = ["SCPM_JAVA_HOME", "JAVA_HOME"].iter().find_map(|v| var(v).filter(|h| !h.is_empty())) {
        return Some(PathBuf::from(home));
    }
    env::split_paths(&var("PATH")?)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
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

/// A running worker. Requests may be sent from several threads at once.
pub struct Worker {
    name: String,
    own_group: bool,
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
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
        let log_file = spec.log_file;
        let mut child =
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| {
                WorkerError::Spawn {
                    java: format!("{} for {} (from {})", java.path.display(), spec.name, java.source),
                    message: e.to_string(),
                }
            })?;
        let name = spec.name.to_string();
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let stdin = child.stdin.take().expect("piped");
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let push_tail = |tail: &Mutex<VecDeque<String>>, line: String| {
            let mut t = tail.lock().expect("tail");
            if t.len() == 40 {
                t.pop_front();
            }
            t.push_back(line);
        };

        {
            let tail = stderr_tail.clone();
            let mut log = log_file.and_then(|p| {
                std::fs::create_dir_all(p.parent()?).ok()?;
                std::fs::OpenOptions::new().create(true).append(true).open(p).ok()
            });
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                    let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                    if let Some(log) = log.as_mut() {
                        let _ = writeln!(log, "{line}");
                    }
                    push_tail(&tail, line);
                    buf.clear();
                }
            });
        }
        {
            let pending = pending.clone();
            let tail = stderr_tail.clone();
            let name = name.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::new();
                while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                    match serde_json::from_slice::<Value>(&buf) {
                        Ok(msg) if msg.get("method").and_then(Value::as_str) == Some("event") => {
                            on_event(msg["params"].clone())
                        }
                        Ok(msg) => {
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
                        // Not JSON (shouldn't happen with the launch flags): keep it as context.
                        Err(_) => push_tail(&tail, format!("(stdout) {}", String::from_utf8_lossy(&buf).trim_end())),
                    }
                    buf.clear();
                }
                // EOF: fail everything still waiting, and refuse new requests.
                let context = format!("{name} exited unexpectedly{}", tail_context(&tail));
                let waiting = pending.lock().expect("pending").take().unwrap_or_default();
                for (_, tx) in waiting {
                    let _ = tx.send(Err(WorkerError::Exited(context.clone())));
                }
            });
        }
        let worker = Worker {
            name,
            own_group: spec.own_process_group,
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            stderr_tail,
        };
        if spec.initialize {
            let init = worker.request("initialize", json!({"protocolVersion": PROTOCOL_VERSION}))?;
            if init["protocolVersion"].as_u64() != Some(PROTOCOL_VERSION) {
                return Err(WorkerError::Setup(format!(
                    "worker speaks protocol {}, expected {PROTOCOL_VERSION}",
                    init["protocolVersion"]
                )));
            }
        }
        Ok(worker)
    }

    /// Sends a request and blocks until its response.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, WorkerError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        match self.pending.lock().expect("pending").as_mut() {
            Some(p) => p.insert(id, tx),
            None => return Err(self.exited()),
        };
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        {
            let mut stdin = self.stdin.lock().expect("stdin");
            let sent = stdin.as_mut().map(|s| writeln!(s, "{line}").and_then(|()| s.flush()));
            if !matches!(sent, Some(Ok(()))) {
                if let Some(p) = self.pending.lock().expect("pending").as_mut() {
                    p.remove(&id);
                }
                return Err(self.exited());
            }
        }
        match rx.recv() {
            // Closed stdout: add the exit status and stderr context. Other failures (a kill
            // with its own reason) pass through.
            Ok(Err(WorkerError::Exited(m))) if m.contains("exited unexpectedly") => Err(self.exited()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(self.exited()),
            Ok(reply) => reply,
        }
    }

    /// An `Exited` error with the exit status (if known) and the end of stderr.
    fn exited(&self) -> WorkerError {
        let status = self.child.lock().ok().and_then(|mut c| {
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

    /// Whether the process is still running.
    pub fn is_alive(&self) -> bool {
        self.child.lock().is_ok_and(|mut c| matches!(c.try_wait(), Ok(None)))
    }

    /// Kills the process (and its process group, if it has its own) and fails pending
    /// requests with `reason` right away, without waiting for stdout to close (a grandchild
    /// may keep it open).
    pub fn kill(&self, reason: &str) {
        if let Ok(mut child) = self.child.lock() {
            #[cfg(unix)]
            if self.own_group {
                // SAFETY: plain syscall on a pid we own; a negative pid targets its group.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill();
        }
        let waiting = self.pending.lock().expect("pending").take().unwrap_or_default();
        for (_, tx) in waiting {
            let _ = tx.send(Err(WorkerError::Exited(format!("{} {reason}", self.name))));
        }
    }

    /// Closes stdin and waits up to `limit` for the process to exit, killing it (and its group)
    /// after that. Returns the exit status if it exited by itself.
    pub fn wait_timeout(self, limit: Option<std::time::Duration>) -> Option<std::process::ExitStatus> {
        self.stdin.lock().expect("stdin").take();
        let started = std::time::Instant::now();
        loop {
            if let Ok(Some(status)) = self.child.lock().expect("child").try_wait() {
                return Some(status);
            }
            if limit.is_some_and(|l| started.elapsed() >= l) {
                self.kill("did not exit in time");
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Asks the worker to exit and waits for it.
    pub fn shutdown(self) {
        let _ = self.request("shutdown", json!({}));
        self.stdin.lock().expect("stdin").take();
        let _ = self.child.lock().expect("child").wait();
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
        self.stdin.lock().map(|mut s| s.take()).ok();
        if let Ok(mut child) = self.child.lock()
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
