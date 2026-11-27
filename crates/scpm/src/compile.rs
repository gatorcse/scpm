//! `scpm compile`: schedule `#compile` and `#test-compile` tasks in dependency order, skip
//! unchanged ones without a JVM, and compile the rest on the worker (design §3.2, §6).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
            Ok(true) => return Ok(file),
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
        Ok(keys.iter().map(|k| self.file(k)).collect())
    }

    /// Classpath files of a variant, with upstream modules as their classes directories.
    pub fn classpath(&self, id: &str, kind: ClasspathKind) -> Result<Vec<(Option<String>, PathBuf)>, CompileError> {
        let vlock = self.lock.variants.get(id).ok_or_else(|| InstallError::Stale(vec![id.to_string()]))?;
        Ok(kind
            .entries(vlock)
            .iter()
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

    let mut scalac_options = v.scalac_options.clone();
    let vlock = &tc.lock.variants[&v.id];
    for key in &vlock.compiler_plugins {
        scalac_options.push(format!("-Xplugin:{}", tc.file(key).display()));
    }
    let mut javac_options = v.javac_options.clone();
    // The release target defaults to the project JDK (design §6.3).
    let release = v.release.clone().or_else(|| ws.config.toolchain.jdk.as_deref().and_then(install::jdk_major));
    if let Some(release) = &release {
        javac_options.extend(["--release".to_string(), release.clone()]);
        if let Some(s) = &v.scala {
            if s.binary == "3" {
                scalac_options.extend(["-java-output-version".to_string(), release.clone()]);
            } else {
                scalac_options.push(format!("-release:{release}"));
            }
        }
    }

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
}

/// A compile worker that can outlive one command (the MCP server and, later, the daemon keep
/// it warm). Commands using one slot must not overlap.
#[derive(Default)]
pub struct WorkerSlot {
    worker: Mutex<Option<(Vec<PathBuf>, Arc<Worker>)>>,
    route: Arc<Mutex<Route>>,
    cancelled: AtomicBool,
}

impl WorkerSlot {
    /// The running worker for `classpath`, starting (or restarting) it if needed.
    ///
    /// The slot's lock is never held while waiting on a worker, so [`WorkerSlot::cancel`] (run
    /// on the MCP reader thread) can't block behind a worker that hangs starting up.
    fn get(&self, classpath: Vec<PathBuf>, log: &Path) -> Result<Arc<Worker>, CompileError> {
        let cancelled = || Err(CompileError::Setup("cancelled".into()));
        let old = {
            let mut slot = self.worker.lock().expect("worker slot");
            // Checked under the lock: `cancel` sets the flag before it takes the lock.
            if self.cancelled.load(Ordering::SeqCst) {
                return cancelled();
            }
            if let Some((cp, w)) = slot.as_ref()
                && *cp == classpath
                && w.is_alive()
            {
                return Ok(w.clone());
            }
            slot.take()
        };
        if let Some((_, old)) = old
            && let Ok(old) = Arc::try_unwrap(old)
        {
            old.shutdown();
        }
        let route = self.route.clone();
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
        };
        let w = Arc::new(Worker::spawn(spec, move |event| {
            if event["t"] != "diagnostic" {
                return;
            }
            let route = route.lock().expect("route");
            if let Some(reporter) = route.reporter.as_ref() {
                let task = event["target"].as_str().unwrap_or_default();
                reporter.emit(Event::Diagnostic(diagnostic(&route.root, task, &event)));
            }
        })?);
        {
            let mut slot = self.worker.lock().expect("worker slot");
            *slot = Some((classpath, w.clone()));
            // A cancel that ran before the worker was published couldn't kill it.
            if self.cancelled.load(Ordering::SeqCst) {
                w.kill("was stopped (request cancelled)");
                return cancelled();
            }
        }
        if let Err(e) = w.handshake() {
            // Don't keep a worker that failed its handshake (it may still be running).
            let mut slot = self.worker.lock().expect("worker slot");
            if slot.as_ref().is_some_and(|(_, s)| Arc::ptr_eq(s, &w)) {
                slot.take();
            }
            w.kill("failed to start");
            return Err(e.into());
        }
        Ok(w)
    }

    /// Cancels the current command (e.g. a cancelled MCP call): in-flight compiles fail with
    /// `reason`, and no worker is (re)started until [`WorkerSlot::reset_cancel`].
    pub fn cancel(&self, reason: &str) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some((_, w)) = self.worker.lock().expect("worker slot").as_ref() {
            w.kill(reason);
        }
    }

    /// Allows the next command to use (and start) the worker again.
    pub fn reset_cancel(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    /// Stops the worker, if one is running.
    pub fn shutdown(&self) {
        if let Some((_, w)) = self.worker.lock().expect("worker slot").take()
            && let Ok(w) = Arc::try_unwrap(w)
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
    let response = worker.get().map_err(WorkerError::Setup).and_then(|w| w.request("compile", plan.params.clone()));
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
}
