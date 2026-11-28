//! `scpm compile`: schedule `#compile` and `#test-compile` tasks in dependency order, skip
//! unchanged ones without a JVM, and compile the rest on the worker (design §3.2, §6).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::coursier;
use crate::fetch;
use crate::graph::Graph;
use crate::install::{self, ClasspathKind, InstallError};
use crate::lockfile::{ClasspathEntry, Lockfile};
use crate::report::{Diagnostic, Event, Position, Range, Reporter, Severity, TaskStatus};
use crate::worker::{self, Worker, WorkerError};
use crate::workspace::Workspace;

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error(transparent)]
    Worker(#[from] WorkerError),
    #[error("{0}")]
    Setup(String),
}

/// Paths for a variant's main outputs.
pub fn classes_dir(root: &Path, id: &str) -> PathBuf {
    root.join(".scpm/out").join(id).join("classes")
}

/// Holds `.scpm/out/.lock` for the duration of a build so two scpm commands in one workspace
/// don't compile into the same directories at once.
pub fn workspace_lock(root: &Path) -> Result<File, CompileError> {
    let never = AtomicBool::new(false);
    Ok(workspace_lock_unless(root, &never)?.expect("never cancelled"))
}

/// [`workspace_lock`] that gives up (returning `None`) once `cancelled` is set.
pub fn workspace_lock_unless(root: &Path, cancelled: &AtomicBool) -> Result<Option<File>, CompileError> {
    let dir = root.join(".scpm/out");
    fs::create_dir_all(&dir).map_err(|e| CompileError::Setup(format!("{}: {e}", dir.display())))?;
    let path = dir.join(".lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| CompileError::Setup(format!("{}: {e}", path.display())))?;
    let mut warned = false;
    loop {
        match fetch::try_lock(&file) {
            Ok(true) => return Ok(Some(file)),
            Ok(false) if cancelled.load(Ordering::SeqCst) => return Ok(None),
            Ok(false) => {
                if !warned {
                    eprintln!("waiting for another scpm command in this workspace to finish…");
                    warned = true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(CompileError::Setup(format!("{}: {e}", path.display()))),
        }
    }
}

pub struct Toolchain<'a> {
    pub ws: &'a Workspace,
    pub lock: &'a Lockfile,
    files: HashMap<String, PathBuf>,
}

impl<'a> Toolchain<'a> {
    pub fn new(ws: &'a Workspace, lock: &'a Lockfile) -> Result<Toolchain<'a>, CompileError> {
        // The same repositories, mirrors and credentials as the install that just ran (design
        // §5.9); its warnings were printed there, so they aren't repeated here.
        let (repos, _) = install::repositories(ws)?;
        let cache = coursier::cache_dir();
        let creds = crate::credentials::CoursierCredentials::load();
        let files = install::fetch_items(lock, &repos, &creds, &cache, &mut Vec::new())?
            .into_iter()
            .map(|i| (i.key, i.dest))
            .collect();
        Ok(Toolchain { ws, lock, files })
    }

    pub fn file(&self, key: &str) -> PathBuf {
        self.files[key].clone()
    }

    pub fn boot(&self, name: &str) -> Result<Vec<PathBuf>, CompileError> {
        let keys = self.lock.boot.get(name).ok_or_else(|| {
            CompileError::Setup(format!("the lockfile has no `{name}` boot entry; run `scpm install`"))
        })?;
        Ok(keys
            .common()
            .into_iter()
            .filter_map(|e| match e {
                ClasspathEntry::Artifact(k) => Some(self.file(k)),
                ClasspathEntry::Module { .. } => None,
            })
            .collect())
    }

    /// Classpath files of a variant, with upstream modules as their classes directories.
    pub fn classpath(&self, id: &str, kind: ClasspathKind) -> Result<Vec<(Option<String>, PathBuf)>, CompileError> {
        let vlock = self.lock.variants.get(id).ok_or_else(|| InstallError::Stale(vec![id.to_string()]))?;
        Ok(kind
            .entries(vlock)
            .into_iter()
            .map(|e| match e {
                ClasspathEntry::Module { module } => (None, classes_dir(&self.ws.root, module)),
                ClasspathEntry::Artifact(key) => (Some(key.clone()), self.file(key)),
            })
            .collect())
    }

    /// Classpath for the compile worker process itself.
    pub fn worker_classpath(&self) -> Result<Vec<PathBuf>, CompileError> {
        let mut cp = vec![worker::helper_jar("SCPM_WORKER_JAR", "scpm-compile-worker")?];
        cp.extend(self.boot(&install::worker_boot())?);
        Ok(cp)
    }
}

/// The compiler options scpm passes for a variant (shared with the IDE generators).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompilerOptions {
    /// The variant's `scalacOptions`, its release target and `-Xplugin:<jar>` per plugin.
    pub scalac: Vec<String>,
    pub javac: Vec<String>,
    /// The Java release target: `release`, else the project JDK's major version (design §6.3).
    pub release: Option<String>,
    /// Compiler plugin jars (also in `scalac` as `-Xplugin:`), not counting SemanticDB's.
    pub plugins: Vec<PathBuf>,
    /// The SemanticDB options at the end of `scalac` (`ide.semanticdb`), so IDE generators
    /// whose own compiler shouldn't write SemanticDB can leave them out.
    pub semanticdb: Vec<String>,
}

/// The scalac and javac options a variant's main (or, with `test`, test) sources compile with:
/// its own, the release target, its locked compiler plugins, and SemanticDB output into the
/// classes directory when the workspace sets `ide.semanticdb`.
pub fn compiler_options(
    tc: &Toolchain,
    v: &crate::workspace::Variant,
    test: bool,
) -> Result<CompilerOptions, CompileError> {
    let ws = tc.ws;
    let vlock = tc.lock.variants.get(&v.id).ok_or_else(|| InstallError::Stale(vec![v.id.clone()]))?;
    // Relative `-Xplugin:` paths resolve against the workspace root (daemon and foreground alike).
    let mut scalac = absolute_plugins(&ws.root, &v.scalac_options);
    let plugins: Vec<PathBuf> = vlock
        .compiler_plugins
        .common()
        .into_iter()
        .filter_map(|e| match e {
            ClasspathEntry::Artifact(k) => Some(tc.file(k)),
            ClasspathEntry::Module { .. } => None,
        })
        .collect();
    for jar in &plugins {
        scalac.push(format!("-Xplugin:{}", jar.display()));
    }
    let mut javac = v.javac_options.clone();
    let release = v.release.clone().or_else(|| ws.config.toolchain.jdk.as_deref().and_then(install::jdk_major));
    if let Some(release) = &release {
        javac.extend(["--release".to_string(), release.clone()]);
        if let Some(s) = &v.scala {
            if s.binary == "3" {
                scalac.extend(["-java-output-version".to_string(), release.clone()]);
            } else {
                scalac.push(format!("-release:{release}"));
            }
        }
    }
    let semanticdb = match &v.scala {
        Some(s) if install::semanticdb_enabled(ws) => {
            let out =
                if test { crate::testing::test_classes_dir(&ws.root, &v.id) } else { classes_dir(&ws.root, &v.id) };
            semanticdb_options(tc, &s.full, &ws.root, &out)?
        }
        _ => Vec::new(),
    };
    scalac.extend(semanticdb.iter().cloned());
    Ok(CompilerOptions { scalac, javac, release, plugins, semanticdb })
}

/// SemanticDB output for one Scala version: files go to `<out>/META-INF/semanticdb/`, named by
/// their path relative to the workspace root (the layout Metals reads; the target root is set
/// explicitly because the compiler runs in the daemon's directory).
fn semanticdb_options(tc: &Toolchain, scala: &str, root: &Path, out: &Path) -> Result<Vec<String>, CompileError> {
    let (root, out) = (root.display().to_string(), out.display().to_string());
    if crate::coord::scala_binary_version(scala) == "3" {
        return Ok(vec!["-Xsemanticdb".into(), "-sourceroot".into(), root, "-semanticdb-target".into(), out]);
    }
    let boot = install::semanticdb_boot(scala)
        .ok_or_else(|| CompileError::Setup(format!("no SemanticDB plugin is published for Scala {scala}")))?;
    let plugin = tc.boot(&boot)?;
    let jar = plugin.first().ok_or_else(|| CompileError::Setup(format!("no SemanticDB plugin for Scala {scala}")))?;
    Ok(vec![
        format!("-Xplugin:{}", jar.display()),
        "-Yrangepos".into(),
        format!("-P:semanticdb:sourceroot:{root}"),
        format!("-P:semanticdb:targetroot:{out}"),
        "-P:semanticdb:failures:warning".into(),
    ])
}

fn is_scala_library(key: &str) -> bool {
    key.starts_with("org.scala-lang:scala-library:") || key.starts_with("org.scala-lang:scala3-library_3:")
}

/// Source files under the variant's source directories, sorted. A source directory that
/// doesn't exist contributes nothing; any other error reading one fails the scan, so an
/// unreadable directory is never mistaken for an empty one.
pub fn source_files(root: &Path, dirs: &[PathBuf]) -> Result<Vec<PathBuf>, CompileError> {
    let mut files = Vec::new();
    for dir in dirs {
        let dir = root.join(dir);
        match fs::metadata(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(CompileError::Setup(format!("{}: {e}", dir.display()))),
        }
        let walker = ignore::WalkBuilder::new(&dir).standard_filters(false).hidden(true).build();
        for entry in walker {
            let entry = entry.map_err(|e| CompileError::Setup(format!("{}: {e}", dir.display())))?;
            let p = entry.path();
            if p.is_file() && p.extension().is_some_and(|e| e == "scala" || e == "java") {
                files.push(p.to_path_buf());
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Removes stale build output (`dir`: a directory tree, else a file). Output that is already
/// gone is fine; any other failure is an error, so a later run can retry the cleanup.
fn remove_stale(path: &Path, dir: bool) -> Result<(), CompileError> {
    let result = if dir { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    match result {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(CompileError::Setup(format!("removing stale output {}: {e}", path.display())))
        }
        _ => Ok(()),
    }
}

/// A schedulable unit: a variant's main or test sources.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Node {
    variant: String,
    test: bool,
}

impl Node {
    fn main(variant: &str) -> Node {
        Node { variant: variant.to_string(), test: false }
    }

    fn task(&self) -> String {
        format!("{}#{}", self.variant, if self.test { "test-compile" } else { "compile" })
    }
}

/// Everything needed to compile one node, in the worker's terms.
struct Plan {
    node: Node,
    params: Value,
    sources_empty: bool,
    fingerprint: String,
    classes: PathBuf,
    analysis: PathBuf,
    fingerprint_file: PathBuf,
}

/// Identity of the tools that produce the output (JDK, worker build), so switching either
/// recompiles. Found without starting a JVM, so a no-op build stays JVM-free (design §3.2).
fn tool_identity() -> Result<String, CompileError> {
    let java = worker::java_identity()?;
    let jar = worker::helper_jar("SCPM_WORKER_JAR", "scpm-compile-worker")?;
    let meta = fs::metadata(&jar).map_err(|e| CompileError::Setup(format!("{}: {e}", jar.display())))?;
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    Ok(format!("java {java}\nworker {} {} {mtime} {}\n", jar.display(), meta.len(), install::worker_boot()))
}

/// `upstream`: fingerprints of the main nodes this node depends on.
fn plan(tc: &Toolchain, node: &Node, upstream: &[String], tools: &str) -> Result<Plan, CompileError> {
    let ws = tc.ws;
    let v = ws.variant(&node.variant).expect("scheduled nodes are variants");
    let scala = install::compiler_scala(ws, v);
    let (src_dirs, kind, classes_name, analysis_name, fp_name) = if node.test {
        (&v.test_sources, ClasspathKind::TestCompile, "test-classes", "test-analysis.zip", "test-fingerprint")
    } else {
        (&v.sources, ClasspathKind::Compile, "classes", "analysis.zip", "fingerprint")
    };
    let out_dir = ws.root.join(".scpm/out").join(&v.id);
    let classes = out_dir.join(classes_name);
    let sources = source_files(&ws.root, src_dirs)?;
    let mut classpath: Vec<(Option<String>, PathBuf)> = tc.classpath(&v.id, kind)?;
    if node.test {
        classpath.insert(0, (None, classes_dir(&ws.root, &v.id)));
    }

    let CompilerOptions { scalac: scalac_options, javac: javac_options, .. } = compiler_options(tc, v, node.test)?;

    // The fingerprint covers everything that can change the output; equal fingerprints let us
    // skip the worker entirely.
    let mut h = Sha256::new();
    // The variant's own compiler bridge (its boot name carries the Zinc version for Scala 2).
    h.update(format!("v2\n{}\n{scala}\n{}\n{tools}", node.task(), install::bridge_boot(&scala)));
    for f in &sources {
        let meta = fs::metadata(f).map_err(|e| CompileError::Setup(format!("{}: {e}", f.display())))?;
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        h.update(format!("src {} {} {mtime}\n", f.display(), meta.len()));
    }
    for (key, path) in &classpath {
        h.update(format!("cp {}\n", key.clone().unwrap_or_else(|| path.display().to_string())));
    }
    for up in upstream {
        h.update(format!("up {up}\n"));
    }
    h.update(format!("scalac {scalac_options:?}\njavac {javac_options:?}\n"));
    let fingerprint = fetch::hex(&h.finalize());

    // The Scala library goes to the worker separately (after hashing, so a library change
    // still changes the fingerprint).
    let library: Vec<PathBuf> = if v.scala.is_some() {
        classpath.iter().filter(|(k, _)| k.as_deref().is_some_and(is_scala_library)).map(|(_, p)| p.clone()).collect()
    } else {
        Vec::new()
    };
    classpath.retain(|(k, _)| !k.as_deref().is_some_and(is_scala_library));
    let analysis = out_dir.join(analysis_name);
    let cache = coursier::cache_dir();
    // Upstream modules' analyses let Zinc see their API changes (like sbt's inter-project
    // lookup) instead of treating their classes directories as opaque libraries.
    let upstream_analyses: Vec<Value> = classpath
        .iter()
        .filter(|(key, _)| key.is_none())
        .filter_map(|(_, dir)| {
            let out = dir.parent()?;
            Some(json!({"classesDir": dir, "analysisFile": out.join("analysis.zip")}))
        })
        .collect();
    let params = json!({
        "target": node.task(),
        "scalaVersion": scala,
        "scalaCompilerClasspath": tc.boot(&format!("scala-compiler@{scala}"))?,
        "scalaLibraryClasspath": library,
        "compilerBridge": tc.boot(&install::bridge_boot(&scala))?.first().cloned().unwrap_or_default(),
        "sources": sources,
        "classpath": classpath.iter().map(|(_, p)| p).collect::<Vec<_>>(),
        "classesDir": classes,
        "analysisFile": analysis,
        "scalacOptions": scalac_options,
        "javacOptions": javac_options,
        "rootPaths": {"WS": ws.root, "CACHE": cache},
        "upstreamAnalyses": upstream_analyses,
    });
    Ok(Plan {
        node: node.clone(),
        params,
        sources_empty: sources.is_empty(),
        fingerprint,
        classes,
        analysis,
        fingerprint_file: out_dir.join(fp_name),
    })
}

/// `-Xplugin:` paths relative to the workspace root, made absolute: the compiler would
/// resolve them against its own working directory, which is the daemon's (or wherever scpm
/// was started), not the workspace.
fn absolute_plugins(root: &Path, options: &[String]) -> Vec<String> {
    options
        .iter()
        .map(|o| match o.strip_prefix("-Xplugin:") {
            // Comma-separated plugins, each a classpath (entries joined by the platform's
            // path separator).
            Some(plugins) => {
                let plugins: Vec<String> = plugins
                    .split(',')
                    .map(|cp| {
                        if cp.is_empty() {
                            return String::new();
                        }
                        let entries = std::env::split_paths(cp).map(|e| root.join(e));
                        std::env::join_paths(entries)
                            .map_or_else(|_| cp.to_string(), |j| j.to_string_lossy().into_owned())
                    })
                    .collect();
                format!("-Xplugin:{}", plugins.join(","))
            }
            None => o.clone(),
        })
        .collect()
}

/// Makes the edit paths of a quick-fix action workspace-relative.
fn relative_edits(root: &Path, mut action: Value) -> Value {
    if let Some(edits) = action.get_mut("edits").and_then(Value::as_array_mut) {
        for edit in edits {
            if let Some(file) = edit["file"].as_str().map(Path::new).and_then(|f| f.strip_prefix(root).ok()) {
                edit["file"] = file.to_string_lossy().replace('\\', "/").into();
            }
        }
    }
    action
}

/// Converts a worker diagnostic event to the run schema (workspace-relative paths).
fn diagnostic(root: &Path, task: &str, e: &Value) -> Diagnostic {
    let severity = match e["severity"].as_str() {
        Some("error") => Severity::Error,
        Some("warning") => Severity::Warning,
        _ => Severity::Info,
    };
    let file = e["file"].as_str().map(|f| {
        let p = Path::new(f);
        p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
    });
    let pos = |v: &Value| Position { line: v["line"].as_u64().unwrap_or(0), col: v["col"].as_u64().unwrap_or(0) };
    let range =
        e.get("range").filter(|r| r.is_object()).map(|r| Range { start: pos(&r["start"]), end: pos(&r["end"]) });
    Diagnostic {
        task: task.to_string(),
        severity,
        file,
        range,
        code: e["code"].as_str().map(str::to_string),
        message: e["message"].as_str().unwrap_or_default().to_string(),
        rendered: e["rendered"].as_str().map(str::to_string),
        actions: e["actions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|a| relative_edits(root, a))
            .collect(),
    }
}

/// Where worker events go for the command currently using the worker.
#[derive(Default)]
struct Route {
    reporter: Option<Arc<Reporter>>,
    root: PathBuf,
    /// Events of workers (connections) with this generation or older are dropped: they
    /// belong to a cancelled command. A worker that was merely replaced (e.g. a retry on a
    /// new daemon connection) keeps routing, so requests still in flight on it lose nothing.
    cancelled_upto: u64,
}

/// How long to wait before trying the daemon again after it failed.
const DAEMON_RETRY: Duration = Duration::from_secs(60);

struct SlotWorker {
    classpath: Vec<PathBuf>,
    worker: Arc<Worker>,
    daemon: bool,
}

/// A compile worker that can outlive one command: a connection to the shared daemon, or a
/// worker JVM this process owns (in CI, with `--no-daemon`, or if the daemon can't start).
/// The MCP server keeps one slot for its lifetime. Commands using one slot must not overlap.
pub struct WorkerSlot {
    worker: Mutex<Option<SlotWorker>>,
    /// The installed worker, for [`WorkerSlot::cancel`] (which must not wait for `worker`,
    /// held while a worker starts).
    live: Mutex<Option<Arc<Worker>>>,
    route: Arc<Mutex<Route>>,
    generations: AtomicU64,
    use_daemon: bool,
    /// After the daemon failed: when to try it again.
    daemon_retry_at: Mutex<Option<Instant>>,
    /// Set at the start of a command when a fallback worker should give way to the daemon.
    retry_daemon: AtomicBool,
    cancelled: AtomicBool,
}

impl Default for WorkerSlot {
    fn default() -> Self {
        WorkerSlot {
            worker: Mutex::new(None),
            live: Mutex::new(None),
            route: Arc::new(Mutex::new(Route::default())),
            generations: AtomicU64::new(0),
            use_daemon: crate::daemon::enabled(),
            daemon_retry_at: Mutex::new(None),
            retry_daemon: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }
    }
}

impl WorkerSlot {
    fn daemon_due(&self) -> bool {
        self.use_daemon && self.daemon_retry_at.lock().expect("retry").is_none_or(|t| Instant::now() >= t)
    }

    /// At the start of a command: if we fell back to our own worker a while ago, try the
    /// daemon again (nothing is in flight between commands, so switching is safe).
    fn begin_command(&self) {
        let fallback = self.worker.lock().expect("worker slot").as_ref().is_some_and(|w| !w.daemon);
        self.retry_daemon.store(fallback && self.daemon_due(), Ordering::SeqCst);
    }

    /// The running worker for `classpath`, starting (or restarting) it if needed.
    ///
    /// The slot's lock is held while a worker starts, but [`WorkerSlot::cancel`] (run on the MCP
    /// reader thread) never waits for it: it reaches the worker through `live`. A new worker is
    /// published there before its handshake, so a cancel can kill one that hangs starting up.
    fn get(&self, classpath: Vec<PathBuf>, log: &Path) -> Result<Arc<Worker>, CompileError> {
        let cancelled = || Err(CompileError::Setup("cancelled".into()));
        if self.cancelled.load(Ordering::SeqCst) {
            return cancelled();
        }
        let mut slot = self.worker.lock().expect("worker slot");
        let retry = self.retry_daemon.swap(false, Ordering::SeqCst);
        if let Some(s) = slot.as_ref()
            && s.classpath == classpath
            && s.worker.is_alive()
            && !retry
        {
            return Ok(s.worker.clone());
        }
        let generation = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
        let route = self.route.clone();
        let on_event = move |event: Value| {
            if event["t"] != "diagnostic" {
                return;
            }
            let route = route.lock().expect("route");
            if generation <= route.cancelled_upto {
                return;
            }
            if let Some(reporter) = route.reporter.as_ref() {
                let task = event["target"].as_str().unwrap_or_default();
                reporter.emit(Event::Diagnostic(diagnostic(&route.root, task, &event)));
            }
        };
        let install = |slot: &mut Option<SlotWorker>, w: Worker, daemon: bool| {
            self.live.lock().expect("live").take();
            if let Some(old) = slot.take()
                && let Ok(old) = Arc::try_unwrap(old.worker)
            {
                old.shutdown();
            }
            let w = Arc::new(w);
            *self.live.lock().expect("live") = Some(w.clone());
            {
                // A cancel that came in while the worker started applies to it too.
                let route = self.route.lock().expect("route");
                if self.cancelled.load(Ordering::SeqCst) {
                    drop(route);
                    w.kill("was stopped (request cancelled)");
                    return Err(CompileError::Setup("cancelled".into()));
                }
            }
            *slot = Some(SlotWorker { classpath: classpath.clone(), worker: w.clone(), daemon });
            Ok(w)
        };
        if self.daemon_due() {
            match crate::daemon::connect_or_start(&classpath, on_event.clone()) {
                Ok(w) => {
                    *self.daemon_retry_at.lock().expect("retry") = None;
                    return install(&mut slot, w, true);
                }
                Err(e) => {
                    eprintln!("warning: compiling without the shared daemon: {e}");
                    *self.daemon_retry_at.lock().expect("retry") = Some(Instant::now() + DAEMON_RETRY);
                    // Keep using our own worker if it is fine.
                    if let Some(s) = slot.as_ref()
                        && s.classpath == classpath
                        && s.worker.is_alive()
                    {
                        return Ok(s.worker.clone());
                    }
                }
            }
        }
        // A cancel during a slow daemon start: don't start a worker of our own either.
        if self.cancelled.load(Ordering::SeqCst) {
            return cancelled();
        }
        let spec = worker::Spawn {
            name: "the compile worker",
            classpath: &classpath,
            main: "scpm.worker.Main",
            jvm_options: &[],
            env: Vec::new(),
            cwd: None,
            log_file: Some(log),
            // The handshake runs below, once a cancel can reach the worker.
            initialize: false,
            own_process_group: false,
            clear_env: false,
        };
        let w = install(&mut slot, Worker::spawn(spec, on_event)?, false)?;
        if let Err(e) = w.initialize(None) {
            // Don't keep a worker that failed its handshake (it may still be running).
            slot.take();
            self.live.lock().expect("live").take();
            w.kill("failed to start");
            return Err(e.into());
        }
        Ok(w)
    }

    /// Cancels the current command (e.g. a cancelled MCP call): in-flight compiles fail with
    /// `reason` (and are cancelled in the daemon), their late events are dropped, and no
    /// worker is (re)started until [`WorkerSlot::reset_cancel`].
    pub fn cancel(&self, reason: &str) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.route.lock().expect("route").cancelled_upto = self.generations.load(Ordering::SeqCst);
        if let Some(w) = self.live.lock().expect("live").as_ref() {
            w.kill(reason);
        }
    }

    /// Forgets `w` (if it is still the slot's worker), so the next request gets a new one.
    fn discard(&self, w: &Arc<Worker>) {
        let mut slot = self.worker.lock().expect("worker slot");
        if slot.as_ref().is_some_and(|s| Arc::ptr_eq(&s.worker, w)) {
            slot.take();
            self.live.lock().expect("live").take();
        }
    }

    /// Allows the next command to use (and start) the worker again.
    pub fn reset_cancel(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    /// Stops the worker, if one is running.
    pub fn shutdown(&self) {
        self.live.lock().expect("live").take();
        if let Some(s) = self.worker.lock().expect("worker slot").take()
            && let Ok(w) = Arc::try_unwrap(s.worker)
        {
            w.shutdown();
        }
    }
}

/// One command's view of a worker slot: routes the worker's events to this command's reporter.
pub struct WorkerHandle<'a> {
    tc: &'a Toolchain<'a>,
    reporter: Arc<Reporter>,
    slot: Arc<WorkerSlot>,
    owned: bool,
    /// Why the worker couldn't start (not retried within one command).
    failed: Mutex<Option<String>>,
}

impl<'a> WorkerHandle<'a> {
    /// A worker that lives only for this command.
    pub fn new(tc: &'a Toolchain<'a>, reporter: Arc<Reporter>) -> Self {
        Self::shared(tc, reporter, Arc::new(WorkerSlot::default()), true)
    }

    /// Uses `slot`'s worker (starting it if needed); leaves it running afterwards unless
    /// `owned`.
    pub fn shared(tc: &'a Toolchain<'a>, reporter: Arc<Reporter>, slot: Arc<WorkerSlot>, owned: bool) -> Self {
        {
            let mut route = slot.route.lock().expect("route");
            route.reporter = Some(reporter.clone());
            route.root = tc.ws.root.clone();
        }
        slot.begin_command();
        WorkerHandle { tc, reporter, slot, owned, failed: Mutex::new(None) }
    }

    fn get(&self) -> Result<Arc<Worker>, String> {
        let mut failed = self.failed.lock().expect("failed");
        if let Some(e) = failed.as_ref() {
            return Err(e.clone());
        }
        let started = self
            .tc
            .worker_classpath()
            .and_then(|cp| self.slot.get(cp, &self.tc.ws.root.join(".scpm/logs/worker.log")))
            .map_err(|e| e.to_string());
        if let Err(e) = &started {
            *failed = Some(e.clone());
        }
        started
    }

    /// Sends a compile request. One that reaches a daemon while it stops (after its idle
    /// timeout, or `scpm daemon stop`) is retried once on a new connection.
    fn compile(&self, params: &Value) -> Result<Value, WorkerError> {
        let w = self.get().map_err(WorkerError::Setup)?;
        match w.request("compile", params.clone()) {
            Err(WorkerError::Remote { code, .. }) if code == "unavailable" => {
                self.slot.discard(&w);
                self.get().map_err(WorkerError::Setup)?.request("compile", params.clone())
            }
            reply => reply,
        }
    }

    pub fn shutdown(self) {
        self.slot.route.lock().expect("route").reporter = None;
        if self.owned {
            self.slot.shutdown();
        }
    }
}

/// What to build for a set of targets.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scope {
    /// Also compile the targets' test sources.
    pub tests: bool,
    /// Also compile modules that are only on the runtime classpath (for `run` and `test`).
    pub runtime: bool,
}

/// Compiles `targets` (and what they depend on) in dependency order. Returns whether
/// everything compiled.
pub fn compile(
    tc: &Toolchain,
    graph: &Graph,
    targets: &BTreeSet<String>,
    scope: Scope,
    worker: &WorkerHandle,
) -> Result<bool, CompileError> {
    let with_tests = scope.tests;
    // Main nodes depend on their compile-scope upstream; test nodes depend on their own main
    // node and on the main nodes of their test-compile upstream. Test-scope workspace edges
    // only ever point from a test node to main nodes, so this graph has no cycles even when
    // the module graph has test-scope cycles.
    let deps_of = |node: &Node| -> Vec<Node> {
        let main_nodes = |kind| -> Vec<Node> {
            install::upstream_variants(graph, &node.variant, kind).iter().map(|u| Node::main(u)).collect()
        };
        if node.test {
            let mut d = vec![Node::main(&node.variant)];
            d.extend(main_nodes(ClasspathKind::TestCompile));
            d
        } else {
            main_nodes(ClasspathKind::Compile)
        }
    };
    let mut deps: BTreeMap<Node, Vec<Node>> = BTreeMap::new();
    let mut frontier: Vec<Node> = targets.iter().map(|t| Node::main(t)).collect();
    if scope.runtime {
        // Runtime-only workspace dependencies must exist before a JVM is forked, even though
        // nothing compiles against them.
        let kind = if with_tests { ClasspathKind::TestRuntime } else { ClasspathKind::Runtime };
        for t in targets {
            frontier.extend(install::upstream_variants(graph, t, kind).iter().map(|u| Node::main(u)));
        }
    }
    if with_tests {
        for t in targets {
            let Some(v) = tc.ws.variant(t) else { continue };
            if source_files(&tc.ws.root, &v.test_sources)?.is_empty() {
                // No test sources (any more): drop output left by deleted tests rather than
                // scheduling a test node, which would also build test-only upstream modules.
                // The fingerprint goes first so a partial cleanup is never mistaken for a
                // finished build.
                let out_dir = tc.ws.root.join(".scpm/out").join(&v.id);
                remove_stale(&out_dir.join("test-fingerprint"), false)?;
                remove_stale(&out_dir.join("test-analysis.zip"), false)?;
                remove_stale(&out_dir.join("test-classes"), true)?;
            } else {
                frontier.push(Node { variant: t.clone(), test: true });
            }
        }
    }
    while let Some(node) = frontier.pop() {
        if deps.contains_key(&node) {
            continue;
        }
        let d = deps_of(&node);
        frontier.extend(d.iter().cloned());
        deps.insert(node, d);
    }
    // Queue in dependency order: main nodes by the graph's order, then test nodes.
    let position: HashMap<&str, usize> = graph.order.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
    let mut order: Vec<Node> = deps.keys().cloned().collect();
    order.sort_by_key(|n| (n.test, position.get(n.variant.as_str()).copied().unwrap_or(usize::MAX)));
    let tools = tool_identity()?;

    // Scheduler state: finished fingerprints (None = failed or skipped).
    let done: Mutex<BTreeMap<Node, Option<String>>> = Mutex::new(BTreeMap::new());
    let changed = Condvar::new();
    let parallelism = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let queue: Mutex<Vec<Node>> = Mutex::new(order.into_iter().rev().collect());
    let first_error: Mutex<Option<CompileError>> = Mutex::new(None);

    std::thread::scope(|s| {
        for _ in 0..parallelism {
            s.spawn(|| {
                loop {
                    // Take a queued node whose dependencies have all finished.
                    let node = {
                        let mut q = queue.lock().expect("queue");
                        loop {
                            if q.is_empty() {
                                return;
                            }
                            let d = done.lock().expect("done");
                            if let Some(pos) = q.iter().rposition(|n| deps[n].iter().all(|u| d.contains_key(u))) {
                                break q.remove(pos);
                            }
                            drop(q);
                            drop(changed.wait(d).expect("condvar"));
                            q = queue.lock().expect("queue");
                        }
                    };
                    let upstream: Option<Vec<String>> = {
                        let d = done.lock().expect("done");
                        deps[&node].iter().map(|u| d[u].clone()).collect()
                    };
                    // A panic must not leave the other threads waiting forever.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &upstream {
                        None => {
                            worker.reporter.emit(Event::TaskEnd {
                                task: node.task(),
                                status: TaskStatus::Skipped,
                                duration_ms: 0,
                                message: None,
                            });
                            Ok(None)
                        }
                        Some(ups) => plan(tc, &node, ups, &tools).and_then(|p| compile_plan(&p, worker)),
                    }));
                    let result = match outcome {
                        Ok(Ok(fp)) => fp,
                        Ok(Err(e)) => {
                            first_error.lock().expect("error").get_or_insert(e);
                            None
                        }
                        Err(_) => {
                            first_error.lock().expect("error").get_or_insert(CompileError::Setup(format!(
                                "internal error while building {}",
                                node.task()
                            )));
                            None
                        }
                    };
                    done.lock().expect("done").insert(node, result);
                    changed.notify_all();
                }
            });
        }
    });
    if let Some(e) = first_error.into_inner().expect("error") {
        return Err(e);
    }
    Ok(done.into_inner().expect("done").values().all(Option::is_some))
}

/// Compiles one node; returns its fingerprint, or `None` if it didn't compile.
fn compile_plan(plan: &Plan, worker: &WorkerHandle) -> Result<Option<String>, CompileError> {
    let task = plan.node.task();
    let end = |status: TaskStatus, duration_ms: u64, message: Option<String>| {
        worker.reporter.emit(Event::TaskEnd { task: task.clone(), status, duration_ms, message });
    };
    if fs::read_to_string(&plan.fingerprint_file).is_ok_and(|old| old == plan.fingerprint) && plan.classes.is_dir() {
        end(TaskStatus::UpToDate, 0, None);
        return Ok(Some(plan.fingerprint.clone()));
    }
    let diagnostics_file = plan.fingerprint_file.with_extension("diagnostics.json");
    if plan.sources_empty {
        // Nothing to compile: make sure no output from deleted sources lingers.
        remove_stale(&plan.fingerprint_file, false)?;
        remove_stale(&plan.analysis, false)?;
        remove_stale(&plan.classes, true)?;
        remove_stale(&diagnostics_file, false)?;
        fs::create_dir_all(&plan.classes).map_err(|e| CompileError::Setup(e.to_string()))?;
        let _ = fs::write(&plan.fingerprint_file, &plan.fingerprint);
        end(TaskStatus::Ok, 0, Some("no sources".into()));
        return Ok(Some(plan.fingerprint.clone()));
    }
    worker.reporter.emit(Event::TaskStart { task: task.clone() });
    let started = Instant::now();
    let response = worker.compile(&plan.params);
    let elapsed = started.elapsed().as_millis() as u64;
    let result = match response {
        Ok(r) => r,
        Err(e) => {
            let _ = fs::remove_file(&plan.fingerprint_file);
            end(TaskStatus::Crashed, elapsed, Some(e.to_string()));
            return Ok(None);
        }
    };
    let status = match result["status"].as_str() {
        Some("ok") => TaskStatus::Ok,
        Some("failed") => TaskStatus::Failed,
        Some("cancelled") => TaskStatus::Cancelled,
        _ => TaskStatus::Crashed,
    };
    if status == TaskStatus::Ok {
        let _ = fs::write(&plan.fingerprint_file, &plan.fingerprint);
    } else {
        let _ = fs::remove_file(&plan.fingerprint_file);
    }
    // The task's current diagnostics (warnings stay valid while it is up to date); the MCP
    // `diagnostics` tool reads these.
    // Without the ANSI rendering (agents read these), written atomically.
    let diagnostics: Vec<Diagnostic> =
        worker.reporter.diagnostics_for(&task).into_iter().map(|d| Diagnostic { rendered: None, ..d }).collect();
    let tmp = diagnostics_file.with_extension(format!("tmp-{}", std::process::id()));
    if fs::write(&tmp, serde_json::to_vec(&diagnostics).unwrap_or_default()).is_ok() {
        let _ = fs::rename(&tmp, &diagnostics_file);
    }
    end(status, elapsed, result["message"].as_str().map(str::to_string));
    Ok((status == TaskStatus::Ok).then(|| plan.fingerprint.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancelled_slot_starts_no_worker() {
        let slot = WorkerSlot::default();
        slot.cancel("was stopped");
        let started = slot.get(vec![PathBuf::from("/nonexistent.jar")], Path::new("/nonexistent/worker.log"));
        assert!(matches!(started, Err(CompileError::Setup(m)) if m == "cancelled"));
        assert!(slot.worker.lock().unwrap().is_none());
    }

    #[test]
    fn source_files_skips_absent_roots_and_reports_unreadable_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src/a")).unwrap();
        fs::write(root.join("src/a/A.scala"), "object A").unwrap();
        fs::write(root.join("src/a/notes.txt"), "").unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();

        let dirs = [PathBuf::from("src"), PathBuf::from("missing"), PathBuf::from("empty")];
        assert_eq!(source_files(root, &dirs).unwrap(), vec![root.join("src/a/A.scala")]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = root.join("src/a");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            // Root (and some sandboxes) can read the directory anyway; only check when it's
            // actually unreadable.
            let unreadable = fs::read_dir(&locked).is_err();
            let result = source_files(root, &dirs);
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            if unreadable {
                assert!(result.is_err(), "unreadable source directory scanned as {result:?}");
            }
        }
    }

    #[test]
    fn remove_stale_ignores_only_missing_output() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("classes");
        fs::create_dir_all(dir.join("p")).unwrap();
        fs::write(dir.join("p/A.class"), "").unwrap();
        let file = tmp.path().join("fingerprint");
        fs::write(&file, "x").unwrap();

        remove_stale(&dir, true).unwrap();
        remove_stale(&file, false).unwrap();
        assert!(!dir.exists() && !file.exists());
        remove_stale(&dir, true).unwrap();
        remove_stale(&file, false).unwrap();
        // A directory where a file is expected is a real failure, not "already gone".
        fs::create_dir_all(&file).unwrap();
        assert!(remove_stale(&file, false).is_err());
    }

    #[test]
    fn relative_plugin_paths_resolve_against_the_workspace() {
        let root = Path::new("/ws");
        let opts = ["-Xplugin:tools/a.jar,/abs/b.jar".to_string(), "-deprecation".to_string()];
        assert_eq!(absolute_plugins(root, &opts), ["-Xplugin:/ws/tools/a.jar,/abs/b.jar", "-deprecation"]);
        // One plugin whose classpath has two entries.
        let sep = if cfg!(windows) { ";" } else { ":" };
        let opts = [format!("-Xplugin:tools/p.jar{sep}tools/support.jar,c.jar")];
        assert_eq!(
            absolute_plugins(root, &opts),
            [format!("-Xplugin:/ws/tools/p.jar{sep}/ws/tools/support.jar,/ws/c.jar")]
        );
    }
}
