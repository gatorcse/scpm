//! `scpm bsp`: a narrow Build Server Protocol 2.1 server over stdio (design §10.3).
//!
//! The subset is what IntelliJ's BSP client (intellij-scala `bsp-builtin`) and Metals v1
//! (`BuildServerConnection`, `ImportedBuild`) actually use. Every answer comes from the same
//! code paths as the CLI: targets and options from the workspace, the lock and
//! [`compile::compiler_options`]; builds through [`compile::compile`] on a shared worker slot
//! (the daemon when available), tests through [`testing::run_tests`], programs through
//! [`testing::main_command`].
//!
//! - **Framing:** LSP-style `Content-Length` headers. Only protocol messages go to stdout: on
//!   Unix the real stdout is moved to a private descriptor and descriptor 1 points at stderr,
//!   so stray output (ours or a child's) can't corrupt the stream.
//! - **Targets:** one per variant (`core@jvm-3`) and one for its tests (`core@jvm-3:test`, tag
//!   `test`, depending on the main target), both with the module directory as base directory,
//!   which is how IntelliJ merges them into one module. Variants other than a module's
//!   `ide.primary` one carry the `no-ide` tag: IntelliJ would otherwise merge every variant of a
//!   cross-built module into one module with both Scala versions' jars.
//! - **Threads:** messages are read on the main thread; requests run on their own threads.
//!   Builds (`compile`, `test`, `run`, `cleanCache`, `workspace/reload`) run one at a time and
//!   stop on `$/cancelRequest`; a cancelled request is answered with error -32800.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::compile::{self, Toolchain, WorkerHandle, WorkerSlot, classes_dir};
use crate::graph::Graph;
use crate::ide::{self, bsp::BSP_VERSION, file_uri};
use crate::install::{self, ClasspathKind};
use crate::lockfile::{ClasspathEntry, LOCK_FILE, Lockfile};
use crate::report::{Diagnostic, Event, Mode, Reporter, Severity, TaskStatus};
use crate::testing::{self, test_classes_dir};
use crate::workspace::{self, Variant, Workspace};

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const SERVER_NOT_INITIALIZED: i64 = -32002;
const REQUEST_CANCELLED: i64 = -32800;

/// `StatusCode`.
const STATUS_OK: u8 = 1;
const STATUS_ERROR: u8 = 2;
const STATUS_CANCELLED: u8 = 3;

/// `MessageType`.
const MSG_ERROR: u8 = 1;
const MSG_WARNING: u8 = 2;
const MSG_INFO: u8 = 3;
const MSG_LOG: u8 = 4;

/// How long `build/exit` (or the end of input) waits for cancelled requests to finish.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a program's output pipes are read after it exits (see `run_program`).
const OUTPUT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Per-variant test timeout for `buildTarget/test` (as `scpm test`'s default).
const TEST_TIMEOUT_SECS: u64 = 1800;

/// The requests served on their own threads (lifecycle requests are answered inline).
const METHODS: [&str; 18] = [
    "workspace/buildTargets",
    "workspace/reload",
    "buildTarget/sources",
    "buildTarget/resources",
    "buildTarget/outputPaths",
    "buildTarget/inverseSources",
    "buildTarget/scalacOptions",
    "buildTarget/javacOptions",
    "buildTarget/dependencySources",
    "buildTarget/dependencyModules",
    "buildTarget/jvmRunEnvironment",
    "buildTarget/jvmTestEnvironment",
    "buildTarget/scalaMainClasses",
    "buildTarget/scalaTestClasses",
    "buildTarget/compile",
    "buildTarget/test",
    "buildTarget/run",
    "buildTarget/cleanCache",
];

type Rpc<T> = Result<T, (i64, String)>;
/// Config and lock files with their modification time and size.
type Stamp = Vec<(PathBuf, Option<(SystemTime, u64)>)>;
type Out = Arc<Mutex<Box<dyn Write + Send>>>;

/// Locks a mutex, recovering it if a panicking thread poisoned it.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn internal(e: impl std::fmt::Display) -> (i64, String) {
    (INTERNAL_ERROR, e.to_string())
}

/// A build target: a variant's main or test sources.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TargetRef {
    uri: String,
    variant: String,
    test: bool,
}

/// The workspace as the server last loaded it, with the build targets derived from it.
struct Model {
    ws: Workspace,
    graph: Graph,
    lock: Lockfile,
    /// Config and lock files with their modification time and size when loaded.
    stamp: Stamp,
    /// `BuildTarget`s, in workspace order.
    targets: Vec<Value>,
    /// Target URI → target; and (variant, test) → URI.
    by_uri: HashMap<String, TargetRef>,
    uris: HashMap<(String, bool), String>,
    /// A digest of everything a target's answers depend on, for `buildTarget/didChange`.
    signatures: BTreeMap<String, String>,
}

impl Model {
    fn variant(&self, r: &TargetRef) -> &Variant {
        self.ws.variant(&r.variant).expect("targets are variants")
    }

    fn uri(&self, variant: &str, test: bool) -> Option<&String> {
        self.uris.get(&(variant.to_string(), test))
    }
}

/// One test-runner request: these suites (all when empty), limited to these cases (all when
/// empty). The runner applies `tests` to every suite of the request.
#[derive(Debug, Clone, Default, PartialEq)]
struct TestRun {
    suites: Vec<String>,
    tests: Vec<String>,
}

/// Adds suites to a variant's runs: whole suites join the one run without case names; a suite
/// with chosen cases gets a run of its own.
fn add_suites(runs: &mut Vec<TestRun>, suites: impl IntoIterator<Item = String>, tests: &[String]) {
    for suite in suites {
        if tests.is_empty() {
            match runs.iter_mut().find(|r| r.tests.is_empty()) {
                Some(r) if !r.suites.contains(&suite) => r.suites.push(suite),
                Some(_) => {}
                None => runs.push(TestRun { suites: vec![suite], tests: Vec::new() }),
            }
        } else {
            match runs.iter_mut().find(|r| r.suites == [suite.clone()] && !r.tests.is_empty()) {
                Some(r) => r.tests.extend(tests.iter().filter(|t| !r.tests.contains(t)).cloned().collect::<Vec<_>>()),
                None => runs.push(TestRun { suites: vec![suite], tests: tests.to_vec() }),
            }
        }
    }
}

/// The files whose changes make the server reload: the workspace file, every module file and
/// the lock.
fn stamp(ws: &Workspace) -> Stamp {
    let mut files = config_files(ws);
    files.push(ws.root.join(LOCK_FILE));
    // A test target exists only while its test source directories do (`has_test_target`).
    files.extend(ws.variants().flat_map(|v| v.test_sources.iter().map(|d| ws.root.join(d))));
    // Version- and platform-specific source directories (`src/main/scala-3`, …) are only
    // included while they exist, so watch every candidate: creating one must reload.
    for m in &ws.modules {
        for v in &m.variants {
            for suffix in workspace::source_suffixes(v.platform, v.scala.as_ref()) {
                for scope in ["main", "test"] {
                    files.push(ws.root.join(&m.dir).join(format!("src/{scope}/scala-{suffix}")));
                }
            }
        }
    }
    files.sort();
    files.dedup();
    files
        .into_iter()
        .map(|f| {
            let meta = std::fs::metadata(&f).ok().map(|m| (m.modified().unwrap_or(UNIX_EPOCH), m.len()));
            (f, meta)
        })
        .collect()
}

/// The config files: the workspace file and the module files the `modules` globs match now
/// (so an added or removed module changes the list), plus those of the loaded modules.
fn config_files(ws: &Workspace) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = ws.workspace_file.iter().map(|f| ws.root.join(f)).collect();
    files.extend(workspace::module_files(ws));
    files.extend(ws.modules.iter().map(|m| ws.root.join(workspace::module_file(m))));
    files
}

/// The stamp of a model loaded starting at `started`. Any watched file or directory modified
/// since then (less a second, for coarse file timestamps) may have changed after the load
/// looked at it, and a test source directory whose existence differs from what the load saw
/// (`observed`) certainly did: those entries are marked unknown, so the next request reloads.
/// The lock is exempt: the load itself writes it.
///
/// The lock's entry is the revision the load used: its metadata as stat'ed right after
/// `install::ensure` wrote it (`lock_seen`), so a lock another process replaces later still
/// differs. Module files the `modules` globs match now but the load didn't load (a module moved
/// in during the load keeps its old mtime) are marked unknown too.
fn stamp_after_load(
    ws: &Workspace,
    started: SystemTime,
    observed: &HashMap<PathBuf, bool>,
    lock_seen: Option<(SystemTime, u64)>,
) -> Stamp {
    let lock_file = ws.root.join(LOCK_FILE);
    let loaded: std::collections::HashSet<PathBuf> =
        ws.modules.iter().map(|m| ws.root.join(workspace::module_file(m))).collect();
    let missed: std::collections::HashSet<PathBuf> =
        workspace::module_files(ws).into_iter().filter(|f| !loaded.contains(f)).collect();
    let since = started - std::time::Duration::from_secs(1);
    mark_unseen(stamp(ws), &lock_file, lock_seen, since, observed, &missed)
}

/// Marks entries as unknown when they were modified at or after `since`, when a directory's
/// existence differs from `observed` (as a directory: a file at that path counts as absent,
/// like the load's own check), or when they are in `missed`. The `lock` entry gets `lock_seen`.
fn mark_unseen(
    stamp: Stamp,
    lock: &Path,
    lock_seen: Option<(SystemTime, u64)>,
    since: SystemTime,
    observed: &HashMap<PathBuf, bool>,
    missed: &std::collections::HashSet<PathBuf>,
) -> Stamp {
    const UNKNOWN: Option<(SystemTime, u64)> = Some((UNIX_EPOCH, u64::MAX));
    stamp
        .into_iter()
        .map(|(f, meta)| {
            if f == lock {
                return (f, lock_seen);
            }
            let changed = observed.get(&f).is_some_and(|seen| *seen != f.is_dir());
            let modified = meta.is_some_and(|(mtime, _)| mtime >= since);
            if changed || modified || missed.contains(&f) { (f, UNKNOWN) } else { (f, meta) }
        })
        .collect()
}

/// A directory as a `file:` URI with a trailing slash (IntelliJ treats a resource or source
/// URI as a directory only with it).
fn dir_uri(path: &Path) -> String {
    let mut uri = file_uri(path);
    if !uri.ends_with('/') {
        uri.push('/');
    }
    uri
}

/// A classpath entry: jars as files, anything else (classes directories) as a directory.
fn classpath_uri(path: &Path) -> String {
    if path.extension().is_some_and(|e| e == "jar") { file_uri(path) } else { dir_uri(path) }
}

/// The path of a `file:` URI (`file:///a%20b`, `file:/a`); `None` for other schemes.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://").or_else(|| uri.strip_prefix("file:"))?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) =
                bytes.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    Some(PathBuf::from(if cfg!(windows) { strip_drive_slash(&text) } else { &text }))
}

/// `/C:/x` → `C:/x`: on Windows, `file:///C:/x` names a drive path (on Unix `/C:/x` is a
/// path of its own).
fn strip_drive_slash(path: &str) -> &str {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' { &path[1..] } else { path }
}

/// A path given as a `file:` URI or as a plain path.
fn uri_or_path(s: &str) -> PathBuf {
    uri_to_path(s).unwrap_or_else(|| PathBuf::from(s))
}

/// Percent-encodes a target id for a URI query (`core@jvm-3:test` stays readable).
fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `JAVA_VERSION` from a JDK's `release` file.
fn java_version(home: &Path) -> Option<String> {
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    let line = release.lines().find(|l| l.starts_with("JAVA_VERSION="))?;
    Some(line["JAVA_VERSION=".len()..].trim_matches('"').to_string())
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Whether a variant gets a test target: one of its test source directories exists.
fn has_test_target(ws: &Workspace, v: &Variant, observed: &mut HashMap<PathBuf, bool>) -> bool {
    let mut any = false;
    for d in &v.test_sources {
        let dir = ws.root.join(d);
        let exists = dir.is_dir();
        observed.insert(dir, exists);
        any |= exists;
    }
    any
}

/// Why a load failed: an invalid config (retried only once a file changes), or a transient
/// failure such as resolving offline (also retried every [`TRANSIENT_RETRY`]).
struct LoadError {
    message: String,
    transient: bool,
}

/// The last failed load.
struct LoadFailure {
    /// The config/lock stamp of the revision that failed; `None` when there was no model to
    /// stamp, or the files changed during the attempt (then the next request retries).
    stamp: Option<Stamp>,
    error: String,
    at: std::time::Instant,
    transient: bool,
}

/// How often a transient load failure (e.g. resolving while offline) is retried without a file
/// change.
const TRANSIENT_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// Loads the workspace, installs (re-locking a stale lock, as every CLI command does) and
/// derives the build targets. Warnings go to `warn`.
fn load_model(start: &Path, warn: &dyn Fn(String)) -> Result<Model, LoadError> {
    let config = |d: crate::diag::Diagnostics| LoadError { message: d.to_string(), transient: false };
    let transient = |e: String| LoadError { message: e, transient: true };
    let started = SystemTime::now();
    let ws = workspace::load(start).map_err(config)?;
    let graph = Graph::build(&ws).map_err(config)?;
    let (lock, summary) = install::ensure(&ws, &graph, false).map_err(|e| transient(e.to_string()))?;
    // The lock revision this model is built from (`ensure` may just have written it).
    let lock_seen =
        std::fs::metadata(ws.root.join(LOCK_FILE)).ok().map(|m| (m.modified().unwrap_or(UNIX_EPOCH), m.len()));
    summary.warnings.iter().for_each(|w| warn(w.clone()));
    if !install::in_ci() {
        // Not the native IntelliJ files: rewriting them while IntelliJ imports over BSP fights it.
        let what = ide::RefreshOptions { idea: false };
        ide::refresh_with(&ws, &lock, what).warnings.into_iter().for_each(warn);
    }
    let tc = Toolchain::new(&ws, &lock).map_err(|e| transient(e.to_string()))?;
    let java_home = crate::worker::java_home();
    let jvm = json!({
        "javaHome": java_home.as_deref().map(dir_uri),
        "javaVersion": java_home.as_deref().and_then(java_version),
    });
    let (mut targets, mut by_uri, mut uris, mut signatures) =
        (Vec::new(), HashMap::new(), HashMap::new(), BTreeMap::new());
    let mut refs = Vec::new();
    // What the test target decisions saw, so the stamp can't record something else.
    let mut observed: HashMap<PathBuf, bool> = HashMap::new();
    for m in &ws.modules {
        let base = ws.root.join(&m.dir);
        for v in &m.variants {
            for test in [false, true] {
                if test && !has_test_target(&ws, v, &mut observed) {
                    continue;
                }
                let id = if test { format!("{}:test", v.id) } else { v.id.clone() };
                let uri = format!("{}?id={}", dir_uri(&base), query_escape(&id));
                uris.insert((v.id.clone(), test), uri.clone());
                by_uri.insert(uri.clone(), TargetRef { uri: uri.clone(), variant: v.id.clone(), test });
                refs.push((m, v, test, id, uri));
            }
        }
    }
    let target_id = |variant: &str, test: bool| uris.get(&(variant.to_string(), test)).map(|u| json!({"uri": u}));
    for (m, v, test, id, uri) in &refs {
        let mut deps: Vec<Value> = Vec::new();
        if *test {
            deps.extend(target_id(&v.id, false));
        }
        let kind = if *test { ClasspathKind::TestCompile } else { ClasspathKind::Compile };
        deps.extend(install::upstream_variants(&graph, &v.id, kind).iter().filter_map(|u| target_id(u, false)));
        let mut tags = vec![if *test {
            "test"
        } else if v.main_class.is_some() {
            "application"
        } else {
            "library"
        }];
        if m.primary_variant().id != v.id {
            tags.push("no-ide");
        }
        let (languages, data_kind, data) = match &v.scala {
            Some(s) => {
                let jars: Vec<String> = tc
                    .boot(&format!("scala-compiler@{}", s.full))
                    .map(|jars| jars.iter().map(|j| file_uri(j)).collect())
                    .unwrap_or_default();
                let data = json!({
                    "scalaOrganization": "org.scala-lang",
                    "scalaVersion": s.full,
                    "scalaBinaryVersion": s.binary,
                    "platform": 1,
                    "jars": jars,
                    "jvmBuildTarget": jvm,
                });
                (json!(["scala", "java"]), "scala", data)
            }
            None => (json!(["java"]), "jvm", jvm.clone()),
        };
        let target = json!({
            "id": {"uri": uri},
            "displayName": id,
            "baseDirectory": dir_uri(&ws.root.join(&m.dir)),
            "tags": tags,
            "languageIds": languages,
            "dependencies": deps,
            "capabilities": {"canCompile": true, "canTest": *test, "canRun": !*test, "canDebug": false},
            "dataKind": data_kind,
            "data": data,
        });
        let vlock = lock.variants.get(&v.id).map(|l| serde_json::to_string(l).unwrap_or_default());
        // Workspace-level settings (e.g. `ide.semanticdb`) change the options, not the variant.
        let options = compile::compiler_options(&tc, v, *test)
            .map(|o| format!("{:?} {:?}", o.scalac, o.javac))
            .unwrap_or_default();
        signatures.insert(
            uri.clone(),
            ide::sha256_hex(
                format!("{target}\n{}\n{vlock:?}\n{options}", serde_json::to_string(v).unwrap_or_default()).as_bytes(),
            ),
        );
        targets.push(target);
    }
    drop(tc);
    let stamp = stamp_after_load(&ws, started, &observed, lock_seen);
    Ok(Model { ws, graph, lock, stamp, targets, by_uri, uris, signatures })
}

/// Main classes in compiled classes: public classes with `public static void main(String[])`
/// (Scala objects' static forwarders and `@main` methods included), found by reading the class
/// files' method tables, sorted.
fn scan_main_classes(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in ignore::WalkBuilder::new(dir).standard_filters(false).build().flatten() {
        let p = entry.path();
        if p.extension().is_none_or(|e| e != "class")
            || p.file_name().is_some_and(|n| n.to_string_lossy().contains("-info"))
        {
            continue;
        }
        if let Some(name) = std::fs::read(p).ok().and_then(|b| class_main(&b)) {
            found.push(name);
        }
    }
    found.sort();
    found
}

/// The class's binary name if it is public and has `public static void main(String[])`.
fn class_main(b: &[u8]) -> Option<String> {
    struct Cursor<'a> {
        b: &'a [u8],
        i: usize,
    }
    impl<'a> Cursor<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let s = self.b.get(self.i..self.i + n)?;
            self.i += n;
            Some(s)
        }
        fn u2(&mut self) -> Option<u16> {
            self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
        }
        fn u4(&mut self) -> Option<u32> {
            self.take(4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        }
        fn skip_attributes(&mut self) -> Option<()> {
            for _ in 0..self.u2()? {
                self.take(2)?;
                let len = self.u4()? as usize;
                self.take(len)?;
            }
            Some(())
        }
    }
    let mut c = Cursor { b, i: 0 };
    if c.u4()? != 0xCAFE_BABE {
        return None;
    }
    c.take(4)?;
    let count = c.u2()?;
    let mut utf8: HashMap<u16, &[u8]> = HashMap::new();
    let mut classes: HashMap<u16, u16> = HashMap::new();
    let mut idx = 1u16;
    while idx < count {
        let tag = c.take(1)?[0];
        match tag {
            1 => {
                let len = c.u2()? as usize;
                utf8.insert(idx, c.take(len)?);
            }
            7 => {
                let name = c.u2()?;
                classes.insert(idx, name);
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => {
                c.take(4)?;
            }
            5 | 6 => {
                c.take(8)?;
                idx += 1;
            }
            8 | 16 | 19 | 20 => {
                c.take(2)?;
            }
            15 => {
                c.take(3)?;
            }
            _ => return None,
        }
        idx += 1;
    }
    let access = c.u2()?;
    let this = c.u2()?;
    c.take(2)?;
    let interfaces = c.u2()? as usize;
    c.take(2 * interfaces)?;
    for _ in 0..c.u2()? {
        c.take(6)?;
        c.skip_attributes()?;
    }
    let mut has_main = false;
    for _ in 0..c.u2()? {
        let flags = c.u2()?;
        let name = c.u2()?;
        let desc = c.u2()?;
        c.skip_attributes()?;
        if flags & 0x0009 == 0x0009
            && utf8.get(&name) == Some(&&b"main"[..])
            && utf8.get(&desc) == Some(&&b"([Ljava/lang/String;)V"[..])
        {
            has_main = true;
        }
    }
    if !has_main || access & 0x0001 == 0 {
        return None;
    }
    let name = utf8.get(classes.get(&this)?)?;
    Some(modified_utf8(name)?.replace('/', "."))
}

/// Decodes a class file's modified UTF-8 (JVMS §4.4.7): one to three bytes per UTF-16 unit,
/// `C0 80` for NUL, and supplementary characters as surrogate pairs of three bytes each.
fn modified_utf8(bytes: &[u8]) -> Option<String> {
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let cont = |b: Option<&u8>| b.filter(|b| *b & 0xC0 == 0x80).map(|b| u16::from(*b & 0x3F));
    while i < bytes.len() {
        let b = bytes[i];
        if b & 0x80 == 0 && b != 0 {
            units.push(u16::from(b));
            i += 1;
        } else if b & 0xE0 == 0xC0 {
            units.push((u16::from(b & 0x1F) << 6) | cont(bytes.get(i + 1))?);
            i += 2;
        } else if b & 0xF0 == 0xE0 {
            units.push((u16::from(b & 0x0F) << 12) | (cont(bytes.get(i + 1))? << 6) | cont(bytes.get(i + 2))?);
            i += 3;
        } else {
            return None;
        }
    }
    String::from_utf16(&units).ok()
}

/// A run-report `Diagnostic` as a BSP `Diagnostic` (0-based lines and characters; scpm's are
/// 1-based). Quick fixes become `scala` diagnostic data (`ScalaAction`s) when they only edit
/// the diagnostic's own file.
fn bsp_diagnostic(d: &Diagnostic) -> Value {
    let pos =
        |p: &crate::report::Position| json!({"line": p.line.saturating_sub(1), "character": p.col.saturating_sub(1)});
    let range = |r: Option<&crate::report::Range>| match r {
        Some(r) => json!({"start": pos(&r.start), "end": pos(&r.end)}),
        None => json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}),
    };
    let severity = match d.severity {
        Severity::Error => 1,
        Severity::Warning => 2,
        Severity::Info => 3,
    };
    let mut out =
        json!({"range": range(d.range.as_ref()), "severity": severity, "source": "scpm", "message": d.message});
    if let Some(code) = &d.code {
        out["code"] = code.clone().into();
    }
    let actions: Vec<Value> = d
        .actions
        .iter()
        .filter_map(|a| {
            let edits = a["edits"].as_array()?;
            let same_file = edits.iter().all(|e| e["file"].is_null() || e["file"].as_str() == d.file.as_deref());
            if !same_file || edits.is_empty() {
                return None;
            }
            let changes: Vec<Value> = edits
                .iter()
                .map(|e| {
                    let r = serde_json::from_value::<crate::report::Range>(e["range"].clone()).ok();
                    json!({"range": range(r.as_ref()), "newText": e["newText"]})
                })
                .collect();
            Some(json!({"title": a["title"], "edit": {"changes": changes}}))
        })
        .collect();
    if !actions.is_empty() {
        out["dataKind"] = "scala".into();
        out["data"] = json!({"actions": actions});
    }
    out
}

/// Build progress for one request: maps run events to BSP notifications.
struct Tasks {
    server: Arc<Server>,
    model: Arc<Model>,
    origin: Value,
    /// The request's own task, parent of the per-target ones.
    parent: String,
    /// The request's cancellation flag: tasks that stop because of it finish as cancelled.
    cancelled: Arc<AtomicBool>,
    state: Mutex<TaskState>,
}

#[derive(Default)]
struct TaskState {
    /// scpm task name → BSP task id, while the task runs.
    ids: HashMap<String, String>,
    /// Compile tasks already finished in this request: a later compile pass (a request compiling
    /// test and main targets makes two) reports them again as up to date; those repeats are
    /// dropped.
    finished: std::collections::HashSet<String>,
    diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// Test counts per test task: passed, failed, ignored, cancelled, skipped.
    tests: HashMap<String, [u64; 5]>,
    done: u64,
}

impl Tasks {
    /// The BSP target of a scpm task (`core@jvm-3#compile`, `…#test-compile`, `…#test`, `…#run`).
    fn target(&self, task: &str) -> Option<(String, &'static str)> {
        let (variant, kind) = task.rsplit_once('#')?;
        let (test, kind) = match kind {
            "compile" => (false, "compile"),
            "test-compile" => (true, "compile"),
            "test" => (true, "test"),
            "run" => (false, "run"),
            _ => return None,
        };
        Some((self.model.uri(variant, test)?.clone(), kind))
    }

    fn start(&self, task: &str) -> String {
        let id = self.server.task_id();
        lock(&self.state).ids.insert(task.to_string(), id.clone());
        let (data_kind, message) = match self.target(task) {
            Some((uri, "compile")) => {
                (Some(("compile-task", json!({"target": {"uri": uri}}))), format!("Compiling {task}"))
            }
            Some((uri, "test")) => (Some(("test-task", json!({"target": {"uri": uri}}))), format!("Testing {task}")),
            _ => (None, format!("Running {task}")),
        };
        let mut params = json!({
            "taskId": {"id": id, "parents": [self.parent]},
            "originId": self.origin, "eventTime": now_millis(), "message": message,
        });
        if let Some((kind, data)) = data_kind {
            params["dataKind"] = kind.into();
            params["data"] = data;
        }
        self.server.notify("build/taskStart", params);
        id
    }

    fn on(&self, event: &Event) {
        match event {
            Event::TaskStart { task } | Event::TaskEnd { task, .. } if lock(&self.state).finished.contains(task) => {}
            Event::TaskStart { task } => {
                self.start(task);
            }
            Event::Diagnostic(d) => lock(&self.state).diagnostics.entry(d.task.clone()).or_default().push(d.clone()),
            Event::Log { task, level, message } => {
                let id = lock(&self.state).ids.get(task).cloned();
                let kind = match level.as_str() {
                    "error" => MSG_ERROR,
                    "warn" | "warning" => MSG_WARNING,
                    _ => MSG_LOG,
                };
                let mut params =
                    json!({"type": kind, "originId": self.origin, "message": crate::report::plain_text(message)});
                if let Some(id) = id {
                    params["task"] = json!({"id": id});
                }
                self.server.notify("build/logMessage", params);
            }
            Event::TestCase(t) => {
                let parent = lock(&self.state).ids.get(&t.task).cloned().unwrap_or_else(|| self.parent.clone());
                let slot = match t.status.as_str() {
                    "passed" | "success" => 0,
                    "failed" | "error" => 1,
                    "ignored" | "pending" => 2,
                    "canceled" => 3,
                    _ => 4,
                };
                lock(&self.state).tests.entry(t.task.clone()).or_default()[slot] += 1;
                let id = self.server.task_id();
                let name = format!("{}.{}", t.suite, t.name);
                let location = t.failure.as_ref().and_then(|f| {
                    let file = f.file.as_ref()?;
                    let line = f.line.unwrap_or(1).saturating_sub(1);
                    let pos = json!({"line": line, "character": 0});
                    Some(json!({"uri": file_uri(&self.model.ws.root.join(file)), "range": {"start": pos, "end": pos}}))
                });
                let task_id = json!({"id": id, "parents": [parent]});
                self.server.notify(
                    "build/taskStart",
                    json!({"taskId": task_id, "originId": self.origin, "eventTime": now_millis(),
                           "message": format!("Running {name}"), "dataKind": "test-start",
                           "data": {"displayName": name, "location": location}}),
                );
                let status = [1, 2, 3, 4, 5][slot];
                let mut data = json!({"displayName": name, "status": status});
                if let Some(f) = &t.failure {
                    data["message"] = f.message.clone().into();
                }
                if let Some(l) = location {
                    data["location"] = l;
                }
                self.server.notify(
                    "build/taskFinish",
                    json!({"taskId": task_id, "originId": self.origin, "eventTime": now_millis(),
                           "message": format!("{name} {}", t.status), "status": if slot == 1 { STATUS_ERROR } else { STATUS_OK },
                           "dataKind": "test-finish", "data": data}),
                );
            }
            Event::TaskEnd { task, status, duration_ms, message } => {
                self.end(task, *status, *duration_ms, message.as_deref())
            }
            _ => {}
        }
    }

    fn end(&self, task: &str, status: TaskStatus, duration_ms: u64, message: Option<&str>) {
        let started = lock(&self.state).ids.get(task).cloned();
        let id = started.unwrap_or_else(|| self.start(task));
        {
            let mut state = lock(&self.state);
            state.ids.remove(task);
            // Only compile tasks repeat across passes; a variant's test task legitimately runs
            // once per runner request (per-suite selections), and each run is reported.
            if task.ends_with("#compile") || task.ends_with("#test-compile") {
                state.finished.insert(task.to_string());
            }
        }
        let target = self.target(task);
        // A task the cancel stopped (killed, or skipped after one was): cancelled, not an error.
        let status = match status {
            TaskStatus::Crashed | TaskStatus::Skipped | TaskStatus::Failed if self.cancelled.load(Ordering::SeqCst) => {
                TaskStatus::Cancelled
            }
            s => s,
        };
        let code = match status {
            TaskStatus::Ok | TaskStatus::UpToDate => STATUS_OK,
            TaskStatus::Cancelled => STATUS_CANCELLED,
            TaskStatus::Failed | TaskStatus::Crashed | TaskStatus::Skipped => STATUS_ERROR,
        };
        let text = match status {
            TaskStatus::Ok => format!("{task} done"),
            TaskStatus::UpToDate => format!("{task} up to date"),
            TaskStatus::Failed => format!("{task} failed"),
            TaskStatus::Skipped => format!("{task} skipped (an upstream target failed)"),
            TaskStatus::Cancelled => format!("{task} cancelled"),
            TaskStatus::Crashed => format!("{task} crashed: {}", message.unwrap_or("")),
        };
        if status == TaskStatus::Crashed
            && let Some(m) = message
        {
            self.server.log_message(MSG_ERROR, format!("{task} crashed: {m}"), Some(&self.origin));
        }
        let mut params = json!({"taskId": {"id": id, "parents": [self.parent]}, "originId": self.origin,
                                "eventTime": now_millis(), "message": text, "status": code});
        match target {
            Some((uri, "compile")) => {
                let diags = lock(&self.state).diagnostics.remove(task).unwrap_or_default();
                let diags = match status {
                    TaskStatus::Ok | TaskStatus::Failed => Some(diags),
                    // Nothing compiled: the task's current diagnostics are the persisted ones.
                    TaskStatus::UpToDate => Some(self.persisted(task)),
                    _ => None,
                };
                let (mut errors, mut warnings) = (0, 0);
                if let Some(diags) = diags {
                    errors = diags.iter().filter(|d| d.severity == Severity::Error).count();
                    warnings = diags.iter().filter(|d| d.severity == Severity::Warning).count();
                    self.server.publish(&self.model.ws.root, &uri, &self.origin, &diags);
                }
                params["dataKind"] = "compile-report".into();
                params["data"] = json!({"target": {"uri": uri}, "originId": self.origin, "errors": errors,
                                        "warnings": warnings, "time": duration_ms, "noOp": status == TaskStatus::UpToDate});
            }
            Some((uri, "test")) => {
                let c = lock(&self.state).tests.remove(task).unwrap_or_default();
                params["dataKind"] = "test-report".into();
                params["data"] = json!({"target": {"uri": uri}, "originId": self.origin, "passed": c[0], "failed": c[1],
                                        "ignored": c[2], "cancelled": c[3], "skipped": c[4], "time": duration_ms});
            }
            _ => {}
        }
        self.server.notify("build/taskFinish", params);
        let done = {
            let mut s = lock(&self.state);
            s.done += 1;
            s.done
        };
        self.server.notify(
            "build/taskProgress",
            json!({"taskId": {"id": self.parent}, "originId": self.origin, "eventTime": now_millis(),
                   "message": text, "progress": done, "unit": "tasks"}),
        );
    }

    /// Diagnostics `scpm compile` saved for an up-to-date task.
    fn persisted(&self, task: &str) -> Vec<Diagnostic> {
        let Some((variant, kind)) = task.rsplit_once('#') else { return Vec::new() };
        let name =
            if kind == "test-compile" { "test-fingerprint.diagnostics.json" } else { "fingerprint.diagnostics.json" };
        std::fs::read(self.model.ws.root.join(".scpm/out").join(variant).join(name))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

/// A variant's discovered test classes: the test fingerprint and target URI they were found
/// for (either changing is a miss), and the `scalaTestClasses` items.
type TestClasses = (String, String, Vec<Value>);

/// What the read loop does with a message.
enum Dispatch {
    Done,
    Spawn(Value, String, Value),
    Exit,
}

pub struct Server {
    start: Mutex<PathBuf>,
    out: Out,
    slot: Arc<WorkerSlot>,
    model: Mutex<Option<Arc<Model>>>,
    load_lock: Mutex<()>,
    /// Serializes builds.
    build_lock: Mutex<()>,
    /// Requests in flight: id → cancellation flag.
    requests: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// The build request running now.
    current: Mutex<Option<String>>,
    /// The process group of a program started by `buildTarget/run`.
    program: Mutex<Option<u32>>,
    /// The last load failure: the config/lock stamp it happened at (empty without a model) and
    /// the error, so it is reported once and not retried until something changes.
    failed: Mutex<Option<LoadFailure>>,
    /// The client is Metals (it wants SemanticDB), and whether it was told about
    /// `ide.semanticdb` yet.
    metals: AtomicBool,
    semanticdb_hinted: AtomicBool,
    initialized: AtomicBool,
    shutdown: AtomicBool,
    /// Files with diagnostics published, per target URI (to clear them when they go away).
    published: Mutex<HashMap<String, BTreeSet<String>>>,
    /// Discovered test classes per variant.
    test_classes: Mutex<HashMap<String, TestClasses>>,
    next_task: AtomicU64,
}

impl Server {
    pub fn new(start: PathBuf, out: Box<dyn Write + Send>) -> Server {
        Server {
            start: Mutex::new(start),
            out: Arc::new(Mutex::new(out)),
            slot: Arc::new(WorkerSlot::default()),
            model: Mutex::new(None),
            load_lock: Mutex::new(()),
            build_lock: Mutex::new(()),
            requests: Mutex::new(HashMap::new()),
            current: Mutex::new(None),
            program: Mutex::new(None),
            failed: Mutex::new(None),
            metals: AtomicBool::new(false),
            semanticdb_hinted: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            published: Mutex::new(HashMap::new()),
            test_classes: Mutex::new(HashMap::new()),
            next_task: AtomicU64::new(0),
        }
    }

    fn send(&self, msg: &Value) {
        let body = msg.to_string();
        let mut out = lock(&self.out);
        let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
        let _ = out.flush();
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn log_message(&self, kind: u8, message: String, origin: Option<&Value>) {
        eprintln!("scpm bsp: {message}");
        let mut params = json!({"type": kind, "message": message});
        if let Some(o) = origin.filter(|o| !o.is_null()) {
            params["originId"] = o.clone();
        }
        self.notify("build/logMessage", params);
    }

    fn task_id(&self) -> String {
        format!("scpm-{}", self.next_task.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Serves messages until `build/exit` or the end of input. Returns the exit code: 0 after
    /// `build/shutdown`, 1 otherwise (as LSP's `exit`).
    pub fn serve(self, input: impl Read + Send) -> io::Result<i32> {
        let server = Arc::new(self);
        let mut reader = BufReader::new(input);
        {
            loop {
                let body = match read_message(&mut reader) {
                    Ok(Some(body)) => body,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("scpm bsp: cannot read a message: {e}");
                        break;
                    }
                };
                let msg = match serde_json::from_slice::<Value>(&body) {
                    Ok(m) => m,
                    Err(e) => {
                        server.send(&error_reply(Value::Null, PARSE_ERROR, &format!("parse error: {e}")));
                        continue;
                    }
                };
                match server.dispatch(msg) {
                    Dispatch::Done => {}
                    Dispatch::Exit => break,
                    Dispatch::Spawn(id, method, params) => {
                        let server = server.clone();
                        std::thread::spawn(move || server.run_request(id, &method, &params));
                    }
                }
            }
            // Exit or end of input: stop in-flight work instead of finishing it. Builds stop at
            // once; a request that can't be cancelled (a resolve in progress) is not waited for
            // beyond a short grace period: the process exits under it.
            server.cancel_all();
            let deadline = std::time::Instant::now() + EXIT_GRACE;
            while !lock(&server.requests).is_empty() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        server.slot.shutdown();
        Ok(if server.shutdown.load(Ordering::SeqCst) { 0 } else { 1 })
    }

    fn dispatch(&self, msg: Value) -> Dispatch {
        let Some(obj) = msg.as_object() else {
            self.send(&error_reply(Value::Null, INVALID_REQUEST, "expected a JSON-RPC object"));
            return Dispatch::Done;
        };
        // Responses to requests we never send.
        if obj.contains_key("result") || obj.contains_key("error") {
            return Dispatch::Done;
        }
        let id = obj.get("id").cloned();
        if let Some(id) = &id
            && !(id.is_string() || id.is_number())
        {
            self.send(&error_reply(Value::Null, INVALID_REQUEST, "`id` must be a string or a number"));
            return Dispatch::Done;
        }
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            self.send(&error_reply(id.unwrap_or(Value::Null), INVALID_REQUEST, "missing `method`"));
            return Dispatch::Done;
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            match method {
                "build/exit" => return Dispatch::Exit,
                "$/cancelRequest" => self.cancel(&params["id"]),
                _ => {}
            }
            return Dispatch::Done;
        };
        match method {
            "build/initialize" => {
                let result = self.initialize(&params);
                self.initialized.store(true, Ordering::SeqCst);
                self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
            }
            _ if !self.initialized.load(Ordering::SeqCst) => {
                self.send(&error_reply(id, SERVER_NOT_INITIALIZED, "the server is not initialized yet"));
            }
            "build/shutdown" => {
                // Set under `current`, which builds register under: a build either sees the
                // flag and doesn't start, or is registered and gets cancelled here.
                let running = {
                    let current = lock(&self.current);
                    self.shutdown.store(true, Ordering::SeqCst);
                    current.clone()
                };
                // Stop a running build; queries in flight still get their answers.
                if let Some(key) = running {
                    self.cancel_key(&key);
                }
                self.send(&json!({"jsonrpc": "2.0", "id": id, "result": null}));
            }
            _ if self.shutdown.load(Ordering::SeqCst) => {
                self.send(&error_reply(id, INVALID_REQUEST, "the server is shutting down"));
            }
            _ if !METHODS.contains(&method) => {
                self.send(&error_reply(id, METHOD_NOT_FOUND, &format!("method not found: {method}")));
            }
            _ => {
                lock(&self.requests).insert(id.to_string(), Arc::new(AtomicBool::new(false)));
                return Dispatch::Spawn(id, method.to_string(), params);
            }
        }
        Dispatch::Done
    }

    /// Tells a Metals client once that SemanticDB is off (`ide.semanticdb`), which Metals v1
    /// needs for references, rename, implementations and code lenses.
    fn semanticdb_hint(&self, model: &Model) {
        let scala = model.ws.variants().any(|v| v.scala.is_some());
        if self.metals.load(Ordering::SeqCst)
            && scala
            && !install::semanticdb_enabled(&model.ws)
            && !self.semanticdb_hinted.swap(true, Ordering::SeqCst)
        {
            self.notify(
                "build/showMessage",
                json!({"type": MSG_WARNING, "message": "scpm compiles without SemanticDB, so Metals can't find references, rename, implementations or show run/test code lenses. Set `ide: {semanticdb: true}` in scpm-workspace.yaml (add one next to a lone scpm.yaml) to enable it for scpm compile and scpm bsp alike."}),
            );
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let metals = params["displayName"].as_str().is_some_and(|n| n.to_lowercase().contains("metals"))
            || params.pointer("/data/semanticdbVersion").is_some();
        self.metals.store(metals, Ordering::SeqCst);
        // Clients start the server in the workspace root; `rootUri` helps when they don't.
        let start = lock(&self.start).clone();
        if workspace::find_root(&start).is_err()
            && let Some(root) = params["rootUri"].as_str().and_then(uri_to_path).filter(|p| p.is_dir())
        {
            *lock(&self.start) = root;
        }
        let languages = json!({"languageIds": ["scala", "java"]});
        json!({
            "displayName": "scpm",
            "version": env!("CARGO_PKG_VERSION"),
            "bspVersion": BSP_VERSION,
            "capabilities": {
                "compileProvider": languages,
                "testProvider": languages,
                "runProvider": languages,
                "debugProvider": {"languageIds": []},
                "inverseSourcesProvider": true,
                "dependencySourcesProvider": true,
                "dependencyModulesProvider": true,
                "resourcesProvider": true,
                "outputPathsProvider": true,
                "buildTargetChangedProvider": true,
                "jvmRunEnvironmentProvider": true,
                "jvmTestEnvironmentProvider": true,
                "jvmCompileClasspathProvider": false,
                "cargoFeaturesProvider": false,
                "canReload": true,
            },
        })
    }

    /// Publishes `key` as the running build, unless `build/shutdown` came first (a build that
    /// was queued behind another then doesn't start).
    fn register(&self, key: &str) -> Rpc<()> {
        let mut current = lock(&self.current);
        if self.shutdown.load(Ordering::SeqCst) {
            return Err((INVALID_REQUEST, "the server is shutting down".into()));
        }
        *current = Some(key.to_string());
        Ok(())
    }

    fn cancel_all(&self) {
        let ids: Vec<String> = lock(&self.requests).keys().cloned().collect();
        for id in ids {
            self.cancel_key(&id);
        }
    }

    fn cancel(&self, id: &Value) {
        self.cancel_key(&id.to_string());
    }

    /// Flags the request, and stops its work if it is the running build.
    fn cancel_key(&self, key: &str) {
        let Some(flag) = lock(&self.requests).get(key).cloned() else { return };
        flag.store(true, Ordering::SeqCst);
        // Hold `current` while stopping the work: the build clears it (under this lock) before
        // releasing the build lock, so the next build can't start, and be hit, in between.
        let current = lock(&self.current);
        if current.as_deref() == Some(key) {
            self.slot.cancel("was stopped (request cancelled)");
            testing::cancel_all("was stopped (request cancelled)");
            if let Some(pid) = *lock(&self.program) {
                kill_group(pid);
            }
        }
        drop(current);
    }

    fn run_request(self: &Arc<Self>, id: Value, method: &str, params: &Value) {
        let key = id.to_string();
        let flag = lock(&self.requests).get(&key).cloned().unwrap_or_default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.route(method, params, &key, &flag)))
            .unwrap_or_else(|_| Err((INTERNAL_ERROR, format!("internal error in `{method}`"))));
        lock(&self.requests).remove(&key);
        let reply = if flag.load(Ordering::SeqCst) {
            error_reply(id, REQUEST_CANCELLED, "request cancelled")
        } else {
            match result {
                Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                Err((code, m)) => error_reply(id, code, &m),
            }
        };
        self.send(&reply);
    }

    fn route(self: &Arc<Self>, method: &str, params: &Value, key: &str, flag: &Arc<AtomicBool>) -> Rpc<Value> {
        match method {
            "workspace/buildTargets" => Ok(json!({"targets": self.model()?.targets})),
            "workspace/reload" => self.reload(flag),
            "buildTarget/sources" => self.per_target(params, |m, r| {
                let v = m.variant(r);
                let dirs = if r.test { &v.test_sources } else { &v.sources };
                let sources: Vec<Value> = dirs
                    .iter()
                    .map(|d| json!({"uri": dir_uri(&m.ws.root.join(d)), "kind": 2, "generated": false}))
                    .collect();
                Ok(json!({"target": {"uri": r.uri}, "sources": sources}))
            }),
            "buildTarget/resources" => self.per_target(params, |m, r| {
                let v = m.variant(r);
                let dirs = if r.test { &v.test_resources } else { &v.resources };
                let uris: Vec<String> = dirs.iter().map(|d| dir_uri(&m.ws.root.join(d))).collect();
                Ok(json!({"target": {"uri": r.uri}, "resources": uris}))
            }),
            "buildTarget/outputPaths" => self.per_target(params, |m, r| {
                // IntelliJ applies excludes only under a module's base directory: a module at the
                // workspace root excludes all of `.scpm/` (and the other tools' directories).
                let at_root = m.ws.module(&m.variant(r).module).is_some_and(|md| md.dir == ".");
                if at_root {
                    let dirs: Vec<Value> = [".scpm", ".metals", ".bsp"]
                        .iter()
                        .map(|d| json!({"uri": dir_uri(&m.ws.root.join(d)), "kind": 2}))
                        .collect();
                    return Ok(json!({"target": {"uri": r.uri}, "outputPaths": dirs}));
                }
                let out = m.ws.root.join(".scpm/out").join(&r.variant);
                Ok(json!({"target": {"uri": r.uri}, "outputPaths": [{"uri": dir_uri(&out), "kind": 2}]}))
            }),
            "buildTarget/inverseSources" => self.inverse_sources(params),
            "buildTarget/scalacOptions" => self.options(params, true),
            "buildTarget/javacOptions" => self.options(params, false),
            "buildTarget/dependencySources" => self.dependency_sources(params),
            "buildTarget/dependencyModules" => self.dependency_modules(params),
            "buildTarget/jvmRunEnvironment" => self.jvm_environment(params, false),
            "buildTarget/jvmTestEnvironment" => self.jvm_environment(params, true),
            "buildTarget/scalaMainClasses" => self.per_target(params, |m, r| {
                let v = m.variant(r);
                let env: Vec<String> = v.env.iter().map(|(k, val)| format!("{k}={val}")).collect();
                let classes: Vec<Value> = main_classes(m, r)
                    .into_iter()
                    .map(|c| json!({"class": c, "arguments": [], "jvmOptions": v.jvm_options, "environmentVariables": env}))
                    .collect();
                Ok(json!({"target": {"uri": r.uri}, "classes": classes}))
            }),
            "buildTarget/scalaTestClasses" => self.scala_test_classes(params),
            "buildTarget/compile" => self.compile(params, key, flag),
            "buildTarget/test" => self.test(params, key, flag),
            "buildTarget/run" => self.run(params, key, flag),
            "buildTarget/cleanCache" => self.clean_cache(params, key, flag),
            _ => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
        }
    }

    /// The current model, reloaded first if a config file or the lock changed since it was
    /// loaded (then `buildTarget/didChange` tells the client).
    fn model(&self) -> Rpc<Arc<Model>> {
        let _guard = lock(&self.load_lock);
        let current = lock(&self.model).clone();
        if let Some(m) = &current {
            let now = stamp(&m.ws);
            // Unchanged, or changed into a config that already failed to load: keep the model.
            let suppressed = lock(&self.failed)
                .as_ref()
                .is_some_and(|f| f.stamp.as_ref() == Some(&now) && (!f.transient || f.at.elapsed() < TRANSIENT_RETRY));
            if now == m.stamp || suppressed {
                return Ok(m.clone());
            }
        }
        self.load(current, false)
    }

    /// Loads the workspace. On failure the last good model (if any) stays in use, and the
    /// client is told once per distinct error with `build/showMessage`.
    fn load(&self, previous: Option<Arc<Model>>, explicit: bool) -> Rpc<Arc<Model>> {
        let start = lock(&self.start).clone();
        let before = previous.as_ref().map(|m| stamp(&m.ws));
        let model = match load_model(&start, &|w| self.log_message(MSG_WARNING, w, None)) {
            Ok(m) => Arc::new(m),
            Err(LoadError { message: e, transient }) => {
                // The revision that failed: if the files changed during the attempt, the new
                // revision stays eligible for the next request.
                let after = previous.as_ref().map(|m| stamp(&m.ws));
                let stamp = before.filter(|b| Some(b) == after.as_ref());
                let mut failed = lock(&self.failed);
                if explicit || failed.as_ref().is_none_or(|f| f.error != e) {
                    let kept = if previous.is_some() { " (still serving the last good configuration)" } else { "" };
                    let message = format!("scpm: cannot load the workspace{kept}:\n{e}");
                    eprintln!("scpm bsp: {message}");
                    self.notify("build/showMessage", json!({"type": MSG_ERROR, "message": message}));
                }
                *failed = Some(LoadFailure { stamp, error: e.clone(), at: std::time::Instant::now(), transient });
                return match previous {
                    Some(m) if !explicit => Ok(m),
                    _ => Err(internal(e)),
                };
            }
        };
        if lock(&self.failed).take().is_some() {
            self.log_message(MSG_INFO, "the workspace loads again".into(), None);
        }
        *lock(&self.model) = Some(model.clone());
        self.semanticdb_hint(&model);
        if let Some(old) = previous {
            let mut changes = Vec::new();
            for (uri, sig) in &model.signatures {
                match old.signatures.get(uri) {
                    None => changes.push(json!({"target": {"uri": uri}, "kind": 1})),
                    Some(s) if s != sig => changes.push(json!({"target": {"uri": uri}, "kind": 2})),
                    _ => {}
                }
            }
            for uri in old.signatures.keys().filter(|u| !model.signatures.contains_key(*u)) {
                changes.push(json!({"target": {"uri": uri}, "kind": 3}));
            }
            if !changes.is_empty() {
                self.notify("buildTarget/didChange", json!({"changes": changes}));
            }
        }
        Ok(model)
    }

    /// Resolves `targets` (`[{uri}]`); unknown ones are an error when `strict`, else skipped.
    fn refs(model: &Model, targets: &Value, strict: bool) -> Rpc<Vec<TargetRef>> {
        let Some(list) = targets.as_array() else { return Err((INVALID_PARAMS, "`targets` must be an array".into())) };
        let mut out = Vec::new();
        for t in list {
            let uri = t["uri"].as_str().unwrap_or_default();
            match model.by_uri.get(uri) {
                Some(r) => out.push(r.clone()),
                None if strict => return Err((INVALID_PARAMS, format!("unknown build target `{uri}`"))),
                None => {}
            }
        }
        Ok(out)
    }

    /// `{items: [...]}` with one item per known requested target.
    fn per_target(&self, params: &Value, item: impl Fn(&Model, &TargetRef) -> Rpc<Value>) -> Rpc<Value> {
        let model = self.model()?;
        let items: Rpc<Vec<Value>> =
            Self::refs(&model, &params["targets"], false)?.iter().map(|r| item(&model, r)).collect();
        Ok(json!({"items": items?}))
    }

    fn inverse_sources(&self, params: &Value) -> Rpc<Value> {
        let model = self.model()?;
        let Some(file) = params.pointer("/textDocument/uri").and_then(Value::as_str).and_then(uri_to_path) else {
            return Err((INVALID_PARAMS, "`textDocument.uri` must be a file URI".into()));
        };
        let file = file.canonicalize().unwrap_or(file);
        let mut targets = Vec::new();
        for (uri, r) in model.by_uri.iter().collect::<BTreeMap<_, _>>() {
            let v = model.variant(r);
            let dirs = if r.test { &v.test_sources } else { &v.sources };
            let under = |d: &PathBuf| {
                let dir = model.ws.root.join(d);
                file.starts_with(&dir) || dir.canonicalize().is_ok_and(|c| file.starts_with(c))
            };
            if dirs.iter().any(under) {
                targets.push(json!({"uri": uri}));
            }
        }
        Ok(json!({"targets": targets}))
    }

    fn options(&self, params: &Value, scala: bool) -> Rpc<Value> {
        let model = self.model()?;
        let tc = Toolchain::new(&model.ws, &model.lock).map_err(internal)?;
        let mut items = Vec::new();
        for r in Self::refs(&model, &params["targets"], false)? {
            let v = model.variant(&r);
            if scala && v.scala.is_none() {
                continue;
            }
            let opts = compile::compiler_options(&tc, v, r.test).map_err(internal)?;
            // Exactly the classpath `scpm compile` compiles against.
            let kind = if r.test { ClasspathKind::TestCompile } else { ClasspathKind::Compile };
            let mut classpath: Vec<PathBuf> =
                tc.classpath(&v.id, kind).map_err(internal)?.into_iter().map(|(_, p)| p).collect();
            let out = if r.test {
                classpath.insert(0, classes_dir(&model.ws.root, &v.id));
                test_classes_dir(&model.ws.root, &v.id)
            } else {
                classes_dir(&model.ws.root, &v.id)
            };
            items.push(json!({
                "target": {"uri": r.uri},
                "options": if scala { opts.scalac } else { opts.javac },
                "classpath": classpath.iter().map(|p| classpath_uri(p)).collect::<Vec<_>>(),
                "classDirectory": dir_uri(&out),
            }));
        }
        Ok(json!({"items": items}))
    }

    /// Artifact keys on a target's classpaths: compile for a main target; test compile plus test
    /// runtime for a test target (as for Metals v2's mbt.json).
    fn artifact_keys(model: &Model, r: &TargetRef) -> Vec<String> {
        let Some(vlock) = model.lock.variants.get(&r.variant) else { return Vec::new() };
        let kinds: &[ClasspathKind] =
            if r.test { &[ClasspathKind::TestCompile, ClasspathKind::TestRuntime] } else { &[ClasspathKind::Compile] };
        let mut keys = Vec::new();
        for kind in kinds {
            for e in kind.entries(vlock) {
                if let ClasspathEntry::Artifact(k) = e
                    && ide::is_jar(k)
                    && !keys.contains(k)
                {
                    keys.push(k.clone());
                }
            }
        }
        keys
    }

    fn dependency_sources(&self, params: &Value) -> Rpc<Value> {
        let model = self.model()?;
        let tc = Toolchain::new(&model.ws, &model.lock).map_err(internal)?;
        let refs = Self::refs(&model, &params["targets"], false)?;
        let all: BTreeSet<String> = refs.iter().flat_map(|r| Self::artifact_keys(&model, r)).collect();
        // Cached sources jars, plus one `cs` call for missing ones (as `scpm ide …`).
        let mut warnings = Vec::new();
        let (sources, _) =
            ide::sources_jars(&tc, &all, ide::SourcesOptions { fetch: true, refresh: false }, &mut warnings);
        warnings.into_iter().for_each(|w| self.log_message(MSG_WARNING, w, None));
        let items: Vec<Value> = refs
            .iter()
            .map(|r| {
                let mut jars: Vec<String> = Vec::new();
                for key in Self::artifact_keys(&model, r) {
                    if let Some(jar) = ide::sources_module(&key).and_then(|m| sources.get(&m)) {
                        let uri = file_uri(jar);
                        if !jars.contains(&uri) {
                            jars.push(uri);
                        }
                    }
                }
                json!({"target": {"uri": r.uri}, "sources": jars})
            })
            .collect();
        Ok(json!({"items": items}))
    }

    fn dependency_modules(&self, params: &Value) -> Rpc<Value> {
        let model = self.model()?;
        let tc = Toolchain::new(&model.ws, &model.lock).map_err(internal)?;
        let refs = Self::refs(&model, &params["targets"], false)?;
        let all: BTreeSet<String> = refs.iter().flat_map(|r| Self::artifact_keys(&model, r)).collect();
        let (sources, _) = ide::sources_jars(&tc, &all, ide::SourcesOptions::default(), &mut Vec::new());
        let items: Vec<Value> = refs
            .iter()
            .map(|r| {
                let modules: Vec<Value> = Self::artifact_keys(&model, r)
                    .iter()
                    .filter_map(|key| {
                        let parts: Vec<&str> = key.split(':').collect();
                        let (org, name, version, classifier) = match parts.as_slice() {
                            [g, a, v] => (*g, *a, *v, None),
                            [g, a, v, c] => (*g, *a, *v, Some(*c)),
                            _ => return None,
                        };
                        let mut artifacts = vec![json!({"uri": file_uri(&tc.file(key)), "classifier": classifier})];
                        if let Some(src) = ide::sources_module(key).and_then(|m| sources.get(&m)) {
                            artifacts.push(json!({"uri": file_uri(src), "classifier": "sources"}));
                        }
                        Some(json!({
                            "name": format!("{org}:{name}"), "version": version, "dataKind": "maven",
                            "data": {"organization": org, "name": name, "version": version, "artifacts": artifacts},
                        }))
                    })
                    .collect();
                json!({"target": {"uri": r.uri}, "modules": modules})
            })
            .collect();
        Ok(json!({"items": items}))
    }

    /// The environment IntelliJ's and Metals' own runners start JVMs with: the runtime
    /// classpath scpm runs with (test runtime for tests, or for a test target), the variant's
    /// `jvmOptions` and `env`, and the working directory `scpm test` uses (the module
    /// directory) or, to run programs, the workspace root.
    fn jvm_environment(&self, params: &Value, tests: bool) -> Rpc<Value> {
        let model = self.model()?;
        let tc = Toolchain::new(&model.ws, &model.lock).map_err(internal)?;
        let mut items = Vec::new();
        for r in Self::refs(&model, &params["targets"], false)? {
            let v = model.variant(&r);
            let test = tests || r.test;
            let classpath = testing::runtime_classpath_unchecked(&tc, v, test).map_err(internal)?;
            let cwd = if tests {
                model.ws.module(&v.module).map(|m| model.ws.root.join(&m.dir)).unwrap_or_else(|| model.ws.root.clone())
            } else {
                model.ws.root.clone()
            };
            let main_classes: Vec<Value> = if tests {
                Vec::new()
            } else {
                main_classes(&model, &r).into_iter().map(|c| json!({"className": c, "arguments": []})).collect()
            };
            items.push(json!({
                "target": {"uri": r.uri},
                "classpath": classpath.iter().map(|p| classpath_uri(p)).collect::<Vec<_>>(),
                "jvmOptions": v.jvm_options,
                "workingDirectory": file_uri(&cwd),
                "environmentVariables": v.env,
                "mainClasses": main_classes,
            }));
        }
        Ok(json!({"items": items}))
    }

    /// Test classes from the test runner's own discovery, cached per variant until its tests
    /// recompile (keyed by the test-compile fingerprint). Uncompiled tests list nothing.
    fn scala_test_classes(&self, params: &Value) -> Rpc<Value> {
        let model = self.model()?;
        let refs: Vec<TargetRef> =
            Self::refs(&model, &params["targets"], false)?.into_iter().filter(|r| r.test).collect();
        if refs.is_empty() {
            return Ok(json!({"items": []}));
        }
        let tc = Toolchain::new(&model.ws, &model.lock).map_err(internal)?;
        let mut items = Vec::new();
        for r in refs {
            let v = model.variant(&r);
            let fp_file = model.ws.root.join(".scpm/out").join(&v.id).join("test-fingerprint");
            let Ok(fingerprint) = std::fs::read_to_string(&fp_file) else { continue };
            if let Some((fp, uri, cached)) = lock(&self.test_classes).get(&v.id)
                && *fp == fingerprint
                && *uri == r.uri
            {
                items.extend(cached.iter().cloned());
                continue;
            }
            match testing::discover_tests(&tc, v) {
                Ok(suites) => {
                    let found: Vec<Value> = suites
                        .into_iter()
                        .map(|(framework, classes)| json!({"target": {"uri": r.uri}, "framework": framework, "classes": classes}))
                        .collect();
                    lock(&self.test_classes).insert(v.id.clone(), (fingerprint, r.uri.clone(), found.clone()));
                    items.extend(found);
                }
                Err(e) => self.log_message(MSG_WARNING, format!("test discovery in {}: {e}", v.id), None),
            }
        }
        Ok(json!({"items": items}))
    }

    /// Publishes a target's current diagnostics, one `build/publishDiagnostics` per file with
    /// `reset: true`, and an empty reset for files that had diagnostics and have none now.
    fn publish(&self, root: &Path, target: &str, origin: &Value, diags: &[Diagnostic]) {
        let mut by_file: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for d in diags {
            match &d.file {
                Some(f) => by_file.entry(file_uri(&root.join(f))).or_default().push(bsp_diagnostic(d)),
                None => self.log_message(
                    if d.severity == Severity::Error { MSG_ERROR } else { MSG_WARNING },
                    d.message.clone(),
                    Some(origin),
                ),
            }
        }
        let mut published = lock(&self.published);
        let previous = published.remove(target).unwrap_or_default();
        let send = |uri: &str, list: &[Value]| {
            self.notify(
                "build/publishDiagnostics",
                json!({"textDocument": {"uri": uri}, "buildTarget": {"uri": target}, "originId": origin,
                       "diagnostics": list, "reset": true}),
            );
        };
        for (uri, list) in &by_file {
            send(uri, list);
        }
        for uri in previous.iter().filter(|u| !by_file.contains_key(*u)) {
            send(uri, &[]);
        }
        published.insert(target.to_string(), by_file.into_keys().collect());
    }

    /// Runs a build: one at a time, on the shared worker, holding the workspace's build lock,
    /// with a `build/taskStart`/`taskFinish` pair around it and run events turned into BSP
    /// notifications. Returns a `StatusCode`.
    fn build(
        self: &Arc<Self>,
        key: &str,
        flag: &Arc<AtomicBool>,
        origin: &Value,
        title: &str,
        body: impl FnOnce(&Build) -> Result<bool, String>,
    ) -> Rpc<u8> {
        let _guard = lock(&self.build_lock);
        // Reset before publishing `current`: a cancel landing in between must not be undone.
        self.slot.reset_cancel();
        testing::reset_cancel();
        self.register(key)?;
        let result = (|| -> Rpc<u8> {
            if flag.load(Ordering::SeqCst) {
                return Ok(STATUS_CANCELLED);
            }
            let model = self.model()?;
            let Some(_ws_lock) = compile::workspace_lock_unless(&model.ws.root, flag).map_err(internal)? else {
                return Ok(STATUS_CANCELLED);
            };
            let parent = self.task_id();
            self.notify(
                "build/taskStart",
                json!({"taskId": {"id": parent}, "originId": origin, "eventTime": now_millis(), "message": title}),
            );
            let run = format!("{}-{}-bsp", now_millis(), std::process::id());
            let reporter = Arc::new(Reporter::new(Mode::Quiet, &model.ws.root, &run));
            let tasks = Arc::new(Tasks {
                server: self.clone(),
                model: model.clone(),
                origin: origin.clone(),
                parent: parent.clone(),
                cancelled: flag.clone(),
                state: Mutex::new(TaskState::default()),
            });
            {
                let tasks = tasks.clone();
                reporter.set_listener(move |e| tasks.on(e));
            }
            reporter.emit(Event::RunStart { run, command: title.to_string() });
            let started = std::time::Instant::now();
            let result = Toolchain::new(&model.ws, &model.lock).map_err(|e| e.to_string()).and_then(|tc| {
                let handle = WorkerHandle::shared(&tc, reporter.clone(), self.slot.clone(), false);
                let build = Build { server: self, model: &model, tc: &tc, handle: &handle, reporter: &reporter, flag };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&build)))
                    .unwrap_or_else(|_| Err(format!("internal error in {title}")));
                handle.shutdown();
                result
            });
            let cancelled = flag.load(Ordering::SeqCst);
            let (ok, error) = match &result {
                Ok(ok) => (*ok, None),
                Err(e) => (false, Some(e.as_str())),
            };
            reporter.finish(title, ok, started.elapsed().as_millis() as u64, error);
            if let Some(e) = error.filter(|_| !cancelled) {
                self.log_message(MSG_ERROR, e.to_string(), Some(origin));
                self.notify("build/showMessage", json!({"type": MSG_ERROR, "originId": origin, "message": e}));
            }
            let status = if cancelled {
                STATUS_CANCELLED
            } else if ok {
                STATUS_OK
            } else {
                STATUS_ERROR
            };
            self.notify(
                "build/taskFinish",
                json!({"taskId": {"id": parent}, "originId": origin, "eventTime": now_millis(),
                       "message": format!("{title}: {}", match status { STATUS_OK => "ok", STATUS_CANCELLED => "cancelled", _ => "failed" }),
                       "status": status}),
            );
            Ok(status)
        })();
        *lock(&self.current) = None;
        result
    }

    fn compile(self: &Arc<Self>, params: &Value, key: &str, flag: &Arc<AtomicBool>) -> Rpc<Value> {
        let origin = params.get("originId").cloned().unwrap_or(Value::Null);
        let refs = Self::refs(&*self.model()?, &params["targets"], true)?;
        // Nothing to compile (Metals pings with an empty compile): no need to wait for a build.
        if refs.is_empty() {
            return Ok(json!({"originId": origin, "statusCode": STATUS_OK}));
        }
        let status = self.build(key, flag, &origin, "compile", |b| {
            // The build's own model (it may have reloaded): a target gone since is an error.
            let refs = current_targets(b.model, &refs)?;
            let tests: BTreeSet<String> = refs.iter().filter(|r| r.test).map(|r| r.variant.clone()).collect();
            let mains: BTreeSet<String> =
                refs.iter().filter(|r| !r.test && !tests.contains(&r.variant)).map(|r| r.variant.clone()).collect();
            let mut ok = true;
            for (set, with_tests) in [(tests, true), (mains, false)] {
                if set.is_empty() || b.flag.load(Ordering::SeqCst) {
                    continue;
                }
                let scope = compile::Scope { tests: with_tests, runtime: false };
                ok &= compile::compile(b.tc, &b.model.graph, &set, scope, b.handle).map_err(|e| e.to_string())?;
            }
            Ok(ok)
        })?;
        Ok(json!({"originId": origin, "statusCode": status}))
    }

    fn test(self: &Arc<Self>, params: &Value, key: &str, flag: &Arc<AtomicBool>) -> Rpc<Value> {
        let origin = params.get("originId").cloned().unwrap_or(Value::Null);
        let model = self.model()?;
        let refs = Self::refs(&model, &params["targets"], true)?;
        // Which suites (and cases) to run, per variant; absent = all. Test names apply to every
        // suite of a runner request, so whole suites share one run and each suite with chosen
        // cases gets its own (selecting `A.foo` and `B.bar` must not also run `A.bar`).
        let mut selection: HashMap<String, Vec<TestRun>> = HashMap::new();
        let mut only_listed = false;
        match params["dataKind"].as_str() {
            Some("scala-test") => {
                let listed = params.pointer("/data/testClasses").and_then(Value::as_array);
                // A selection was given: only what it names runs, even if that is nothing current.
                only_listed = listed.is_some_and(|l| !l.is_empty());
                for item in listed.into_iter().flatten() {
                    let uri = item.pointer("/target/uri").and_then(Value::as_str).unwrap_or_default();
                    if let Some(r) = model.by_uri.get(uri) {
                        let classes = item["classes"].as_array().into_iter().flatten().filter_map(Value::as_str);
                        add_suites(selection.entry(r.variant.clone()).or_default(), classes.map(str::to_string), &[]);
                    }
                }
            }
            Some("scala-test-suites") => {
                let classes: Vec<String> = params["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                for r in &refs {
                    add_suites(selection.entry(r.variant.clone()).or_default(), classes.iter().cloned(), &[]);
                }
            }
            Some("scala-test-suites-selection") => {
                for suite in params.pointer("/data/suites").and_then(Value::as_array).into_iter().flatten() {
                    let tests: Vec<String> = suite["tests"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                    for r in &refs {
                        let runs = selection.entry(r.variant.clone()).or_default();
                        add_suites(runs, suite["className"].as_str().map(str::to_string), &tests);
                    }
                }
            }
            _ => {}
        }
        if only_listed && selection.is_empty() {
            return Err((
                INVALID_PARAMS,
                "the scala-test selection names no current build target (re-run test discovery)".into(),
            ));
        }
        let status = self.build(key, flag, &origin, "test", |b| {
            let variants: BTreeSet<String> = current_targets(b.model, &refs)?
                .into_iter()
                .map(|r| r.variant)
                .filter(|v| !only_listed || selection.contains_key(v))
                .collect();
            let with_tests = testing::with_tests(b.tc, &variants).map_err(|e| e.to_string())?;
            let runner_cp = if with_tests.is_empty() {
                Vec::new()
            } else {
                testing::runner_classpath(b.tc).map_err(|e| e.to_string())?
            };
            testing::clear_reports(b.tc, &with_tests);
            let scope = compile::Scope { tests: true, runtime: true };
            if !compile::compile(b.tc, &b.model.graph, &variants, scope, b.handle).map_err(|e| e.to_string())? {
                return Ok(false);
            }
            if with_tests.is_empty() {
                b.server.log_message(MSG_INFO, "no test sources in the selected targets".into(), Some(&origin));
                return Ok(true);
            }
            let mut ok = true;
            for v in with_tests {
                if b.flag.load(Ordering::SeqCst) {
                    return Ok(false);
                }
                let runs = selection.get(&v.id).cloned().unwrap_or_else(|| vec![TestRun::default()]);
                for (i, run) in runs.into_iter().enumerate() {
                    if b.flag.load(Ordering::SeqCst) {
                        return Ok(false);
                    }
                    let opts = testing::TestOptions {
                        filters: run.suites,
                        test_names: run.tests,
                        timeout: Some(std::time::Duration::from_secs(TEST_TIMEOUT_SECS)),
                        // Each run keeps its own report: an earlier failure must not be overwritten.
                        report_name: (i > 0).then(|| format!("junit-{}.xml", i + 1)),
                    };
                    ok &= testing::run_tests(b.tc, &[v], &runner_cp, &opts, b.reporter);
                }
            }
            Ok(ok)
        })?;
        Ok(json!({"originId": origin, "statusCode": status}))
    }

    fn run(self: &Arc<Self>, params: &Value, key: &str, flag: &Arc<AtomicBool>) -> Rpc<Value> {
        let origin = params.get("originId").cloned().unwrap_or(Value::Null);
        // Checked now for a quick answer, and again against the build's own model.
        run_spec(&*self.model()?, params)?;
        let status = self.build(key, flag, &origin, "run", |b| {
            let spec = run_spec(b.model, params).map_err(|(_, m)| m)?;
            let v = &spec.variant;
            let target_set: BTreeSet<String> = [v.id.clone()].into();
            let scope = compile::Scope { tests: false, runtime: true };
            if !compile::compile(b.tc, &b.model.graph, &target_set, scope, b.handle).map_err(|e| e.to_string())? {
                return Ok(false);
            }
            let task = format!("{}#run", v.id);
            b.reporter.emit(Event::TaskStart { task: task.clone() });
            let started = std::time::Instant::now();
            let result = b.server.run_program(b, &spec, &task);
            let (status, message) = match &result {
                Ok(0) => (TaskStatus::Ok, None),
                Ok(code) => (TaskStatus::Failed, Some(format!("exited with code {code}"))),
                Err(e) => (TaskStatus::Crashed, Some(e.clone())),
            };
            let status = if b.flag.load(Ordering::SeqCst) { TaskStatus::Cancelled } else { status };
            if let Some(m) = &message {
                b.reporter.emit(Event::Log {
                    task: task.clone(),
                    level: "error".into(),
                    message: format!("{} {m}", spec.main),
                });
            }
            b.reporter.emit(Event::TaskEnd {
                task,
                status,
                duration_ms: started.elapsed().as_millis() as u64,
                message,
            });
            Ok(status == TaskStatus::Ok)
        })?;
        Ok(json!({"originId": origin, "statusCode": status}))
    }

    /// Runs a program like `scpm run`, its output becoming `build/logMessage`s. Returns its
    /// exit code (-1 if it was killed).
    ///
    /// The run ends when the JVM exits. Background processes it started may keep its output
    /// open: after a grace period they are stopped with its process group, so the build (and
    /// its locks) never wait for them.
    fn run_program(&self, b: &Build, spec: &RunSpec, task: &str) -> Result<i32, String> {
        let mut cmd = testing::main_command(b.tc, &spec.variant, &spec.main, &spec.jvm_options, &spec.args)
            .map_err(|e| e.to_string())?;
        cmd.envs(spec.env.iter().cloned())
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start java: {e}"))?;
        *lock(&self.program) = Some(child.id());
        // A cancel that came before the program was registered.
        if b.flag.load(Ordering::SeqCst) {
            kill_group(child.id());
        }
        let pipes: Vec<(Box<dyn Read + Send>, &str)> = vec![
            (Box::new(child.stdout.take().expect("piped")), "info"),
            (Box::new(child.stderr.take().expect("piped")), "error"),
        ];
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        for (pipe, level) in pipes {
            let (reporter, task, done) = (b.reporter.clone(), task.to_string(), done_tx.clone());
            std::thread::spawn(move || {
                // Byte lines decoded lossily: one invalid UTF-8 line mustn't stop forwarding (and
                // close the pipe on the program).
                for line in BufReader::new(pipe).split(b'\n') {
                    let Ok(line) = line else { break };
                    let line = String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(&line)).into_owned();
                    reporter.emit(Event::Log { task: task.clone(), level: level.into(), message: line });
                }
                let _ = done.send(());
            });
        }
        drop(done_tx);
        let status = child.wait().map_err(|e| e.to_string());
        // Output still in the pipes is read; a background process holding them open is not
        // waited for.
        let drained = |limit: std::time::Duration| {
            let deadline = std::time::Instant::now() + limit;
            (0..2).all(|_| done_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).is_ok())
        };
        if !drained(OUTPUT_GRACE) {
            kill_group(child.id());
            b.server.log_message(
                MSG_WARNING,
                format!("{} exited, but a process it started kept its output open; stopped it", spec.main),
                None,
            );
            drained(OUTPUT_GRACE);
        }
        *lock(&self.program) = None;
        Ok(status?.code().unwrap_or(-1))
    }

    fn clean_cache(self: &Arc<Self>, params: &Value, key: &str, flag: &Arc<AtomicBool>) -> Rpc<Value> {
        let model = self.model()?;
        let refs = Self::refs(&model, &params["targets"], true)?;
        let _guard = lock(&self.build_lock);
        if flag.load(Ordering::SeqCst) {
            return Ok(json!({"cleaned": false}));
        }
        self.register(key)?;
        let lock_file = compile::workspace_lock_unless(&model.ws.root, flag);
        *lock(&self.current) = None;
        let Some(_ws_lock) = lock_file.map_err(internal)? else { return Ok(json!({"cleaned": false})) };
        let mut failed = Vec::new();
        for r in &refs {
            let out = model.ws.root.join(".scpm/out").join(&r.variant);
            let names: &[&str] = if r.test {
                &[
                    "test-classes",
                    "test-analysis.zip",
                    "test-fingerprint",
                    "test-fingerprint.diagnostics.json",
                    "test-reports",
                ]
            } else {
                &["classes", "analysis.zip", "fingerprint", "fingerprint.diagnostics.json"]
            };
            for name in names {
                if flag.load(Ordering::SeqCst) {
                    return Ok(json!({"cleaned": false, "message": "cancelled"}));
                }
                let p = out.join(name);
                let removed = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                if let Err(e) = removed
                    && e.kind() != io::ErrorKind::NotFound
                {
                    failed.push(format!("{}: {e}", p.display()));
                }
            }
        }
        if failed.is_empty() {
            Ok(json!({"cleaned": true, "message": format!("cleaned {} target(s)", refs.len())}))
        } else {
            Ok(json!({"cleaned": false, "message": failed.join("; ")}))
        }
    }

    /// Reloads unconditionally. Builds keep the model they started with, so this doesn't wait
    /// for a running build.
    fn reload(&self, flag: &Arc<AtomicBool>) -> Rpc<Value> {
        let _load = lock(&self.load_lock);
        if flag.load(Ordering::SeqCst) {
            return Ok(Value::Null);
        }
        let previous = lock(&self.model).clone();
        self.load(previous, true)?;
        lock(&self.test_classes).clear();
        Ok(Value::Null)
    }
}

/// `refs` resolved against `model` (a build's own, which may have reloaded since the request
/// was checked): a target that no longer exists fails the build instead of being skipped.
fn current_targets(model: &Model, refs: &[TargetRef]) -> Result<Vec<TargetRef>, String> {
    refs.iter()
        .map(|r| {
            model.by_uri.get(&r.uri).cloned().ok_or_else(|| {
                format!("build target `{}` no longer exists (the workspace changed); re-import the build", r.uri)
            })
        })
        .collect()
}

/// A `buildTarget/run` request resolved against a model.
struct RunSpec {
    variant: Variant,
    main: String,
    args: Vec<String>,
    jvm_options: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
}

fn run_spec(model: &Model, params: &Value) -> Rpc<RunSpec> {
    let target = Server::refs(model, &json!([params["target"]]), true)?.remove(0);
    let variant = model.variant(&target).clone();
    let data = (params["dataKind"] == "scala-main-class").then(|| params["data"].clone());
    let main = data
        .as_ref()
        .and_then(|d| d["class"].as_str().map(str::to_string))
        .or_else(|| variant.main_class.clone())
        .ok_or_else(|| (INVALID_PARAMS, format!("{} has no `mainClass`; pass a scala-main-class", variant.id)))?;
    let strings = |v: &Value| -> Vec<String> {
        v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
    };
    let args = match &data {
        Some(d) if d["arguments"].is_array() => strings(&d["arguments"]),
        _ => strings(&params["arguments"]),
    };
    // `scalaMainClasses` hands out the variant's own `jvmOptions`, and the launch adds them
    // again: a client echoing them back would apply each twice (two `-javaagent`s). Drop that
    // echoed prefix; options the client added are kept.
    let mut jvm_options = data.as_ref().map(|d| strings(&d["jvmOptions"])).unwrap_or_default();
    if jvm_options.starts_with(&variant.jvm_options) {
        jvm_options.drain(..variant.jvm_options.len());
    }
    let mut env: Vec<(String, String)> = params["environmentVariables"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect();
    if let Some(d) = &data {
        env.extend(strings(&d["environmentVariables"]).iter().filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        }));
    }
    let cwd = params["workingDirectory"].as_str().map(uri_or_path).unwrap_or_else(|| model.ws.root.clone());
    Ok(RunSpec { variant, main, args, jvm_options, env, cwd })
}

/// What a build body works with.
struct Build<'a> {
    server: &'a Arc<Server>,
    model: &'a Arc<Model>,
    tc: &'a Toolchain<'a>,
    handle: &'a WorkerHandle<'a>,
    reporter: &'a Arc<Reporter>,
    flag: &'a Arc<AtomicBool>,
}

/// A target's main classes: the configured `mainClass` first, then those found in its compiled
/// classes.
fn main_classes(model: &Model, r: &TargetRef) -> Vec<String> {
    let v = model.variant(r);
    let mut classes: Vec<String> = if r.test { Vec::new() } else { v.main_class.iter().cloned().collect() };
    let dir = if r.test { test_classes_dir(&model.ws.root, &v.id) } else { classes_dir(&model.ws.root, &v.id) };
    for c in scan_main_classes(&dir) {
        if !classes.contains(&c) {
            classes.push(c);
        }
    }
    classes
}

/// Kills a program started by `buildTarget/run`: its process group on Unix, its tree elsewhere.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    // SAFETY: plain syscall on a process group we started.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    // No process groups: kill the process tree.
    #[cfg(not(unix))]
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Reads one `Content-Length`-framed message; `None` at the end of input.
fn read_message(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, value)) = l.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let n = value
                .trim()
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad header `{l}`")))?;
            length = Some(n);
        }
    }
    let mut body = vec![0; length.unwrap_or(0)];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// The protocol channel: the real stdout, moved to a private descriptor, with descriptor 1
/// pointed at stderr so nothing else can write to the protocol stream (on Unix).
fn protocol_stdout() -> Box<dyn Write + Send> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        let _ = io::stdout().flush();
        // SAFETY: dup/dup2/fcntl on this process's standard descriptors; the new descriptor is
        // owned by the returned File alone.
        unsafe {
            let fd = libc::dup(1);
            if fd >= 0 {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                if libc::dup2(2, 1) >= 0 {
                    return Box::new(std::fs::File::from_raw_fd(fd));
                }
                libc::close(fd);
            }
        }
    }
    Box::new(io::stdout())
}

/// `scpm bsp`: serves stdin/stdout. Returns the process exit code.
pub fn serve(start: PathBuf) -> io::Result<i32> {
    let out = protocol_stdout();
    Server::new(start, out).serve(io::stdin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selections_keep_cases_per_suite() {
        let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut runs = Vec::new();
        add_suites(&mut runs, strings(&["A"]), &strings(&["foo"]));
        add_suites(&mut runs, strings(&["B"]), &strings(&["bar"]));
        add_suites(&mut runs, strings(&["C", "D"]), &[]);
        add_suites(&mut runs, strings(&["A"]), &strings(&["baz", "foo"]));
        add_suites(&mut runs, strings(&["C"]), &[]);
        assert_eq!(
            runs,
            vec![
                TestRun { suites: strings(&["A"]), tests: strings(&["foo", "baz"]) },
                TestRun { suites: strings(&["B"]), tests: strings(&["bar"]) },
                TestRun { suites: strings(&["C", "D"]), tests: Vec::new() },
            ]
        );
    }

    /// A sink tests can read back.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn frame(msg: &Value) -> Vec<u8> {
        let body = msg.to_string();
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    fn messages(sink: &Sink) -> Vec<Value> {
        let bytes = sink.0.lock().unwrap().clone();
        let mut r = BufReader::new(&bytes[..]);
        let mut out = Vec::new();
        while let Some(body) = read_message(&mut r).unwrap() {
            out.push(serde_json::from_slice(&body).unwrap());
        }
        out
    }

    #[test]
    fn framing_round_trips_and_tolerates_extra_headers() {
        let mut input =
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: 2\r\n\r\n{}".to_vec();
        input.extend(frame(&json!({"a": "é"})));
        let mut r = BufReader::new(&input[..]);
        assert_eq!(read_message(&mut r).unwrap().unwrap(), b"{}");
        let second: Value = serde_json::from_slice(&read_message(&mut r).unwrap().unwrap()).unwrap();
        assert_eq!(second["a"], "é");
        assert!(read_message(&mut r).unwrap().is_none());
        let bad = b"Content-Length: x\r\n\r\n".to_vec();
        assert!(read_message(&mut BufReader::new(&bad[..])).is_err());
    }

    #[test]
    fn uris() {
        let p = if cfg!(windows) { Path::new("C:/a b/c%d") } else { Path::new("/a b/c%d") };
        assert_eq!(uri_to_path(&file_uri(p)).unwrap(), p);
        #[cfg(windows)]
        assert_eq!(uri_to_path("file:/C:/x/y").unwrap(), Path::new("C:/x/y"));
        #[cfg(not(windows))]
        assert_eq!(uri_to_path("file:/x/y").unwrap(), Path::new("/x/y"));
        assert_eq!(strip_drive_slash("/C:/x/y"), "C:/x/y", "Windows drive");
        assert_eq!(strip_drive_slash("/x/C:"), "/x/C:");
        #[cfg(windows)]
        assert_eq!(uri_to_path("file:///c%3A/x").unwrap(), Path::new("c:/x"));
        let (a, prefix) = if cfg!(windows) { ("C:/a", "file:///C:/a") } else { ("/a", "file:///a") };
        assert_eq!(dir_uri(Path::new(a)), format!("{prefix}/"));
        assert_eq!(classpath_uri(&Path::new(a).join("b.jar")), format!("{prefix}/b.jar"));
        assert_eq!(classpath_uri(&Path::new(a).join("classes")), format!("{prefix}/classes/"));
        assert_eq!(query_escape("core@jvm-3:test"), "core@jvm-3:test");
        assert_eq!(query_escape("a b&c"), "a%20b%26c");
        assert_eq!(uri_or_path("/plain"), Path::new("/plain"));
    }

    #[test]
    fn diagnostics_are_zero_based() {
        let d = Diagnostic {
            task: "a#compile".into(),
            severity: Severity::Error,
            file: Some("A.scala".into()),
            range: Some(crate::report::Range {
                start: crate::report::Position { line: 3, col: 19 },
                end: crate::report::Position { line: 3, col: 25 },
            }),
            code: Some("E007".into()),
            message: "Found: String".into(),
            rendered: None,
            actions: vec![
                json!({"title": "fix", "edits": [{"file": "A.scala", "range": {"start": {"line": 3, "col": 19}, "end": {"line": 3, "col": 25}}, "newText": "1"}]}),
            ],
        };
        let b = bsp_diagnostic(&d);
        assert_eq!(b["range"]["start"], json!({"line": 2, "character": 18}));
        assert_eq!(b["severity"], 1);
        assert_eq!(b["code"], "E007");
        assert_eq!(b["data"]["actions"][0]["edit"]["changes"][0]["newText"], "1");
    }

    #[test]
    fn lifecycle_and_errors() {
        let sink = Sink::default();
        let server = Server::new(PathBuf::from("/nonexistent"), Box::new(sink.clone()));
        let mut input = Vec::new();
        for msg in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "workspace/buildTargets"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "build/initialize", "params": {"displayName": "t", "version": "0", "bspVersion": "2.1.0", "rootUri": "file:///nonexistent", "capabilities": {"languageIds": ["scala"]}}}),
            json!({"jsonrpc": "2.0", "method": "build/initialized"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "build/shutdown"}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "workspace/buildTargets"}),
            json!({"jsonrpc": "2.0", "method": "build/exit"}),
        ] {
            input.extend(frame(&msg));
        }
        input.extend(b"Content-Length: 3\r\n\r\n{x}");
        let code = server.serve(&input[..]).unwrap();
        assert_eq!(code, 0, "exit after shutdown");
        let replies = messages(&sink);
        let by_id =
            |id: u64| replies.iter().find(|m| m["id"] == id).unwrap_or_else(|| panic!("no reply {id}: {replies:?}"));
        assert_eq!(by_id(1)["error"]["code"], SERVER_NOT_INITIALIZED);
        let caps = &by_id(2)["result"]["capabilities"];
        assert_eq!(by_id(2)["result"]["bspVersion"], BSP_VERSION);
        assert_eq!(caps["testProvider"]["languageIds"], json!(["scala", "java"]));
        assert_eq!(caps["canReload"], true);
        assert_eq!(by_id(3)["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(by_id(4)["result"], Value::Null);
        assert_eq!(by_id(5)["error"]["code"], INVALID_REQUEST);
        assert!(!replies.iter().any(|m| m["error"]["code"] == PARSE_ERROR), "nothing after exit");
    }

    #[test]
    fn entries_the_load_may_not_have_seen_are_marked_unknown() {
        const UNKNOWN: Option<(SystemTime, u64)> = Some((UNIX_EPOCH, u64::MAX));
        let dir = tempfile::tempdir().unwrap();
        let t0 = SystemTime::now();
        let old = t0 - std::time::Duration::from_secs(60);
        let new = t0 + std::time::Duration::from_secs(1);
        let lock_file = dir.path().join("scpm-lock.yaml");
        let manifest = dir.path().join("scpm.yaml");
        let (none, no_missed) = (HashMap::new(), std::collections::HashSet::new());
        let mark = |stamp: Stamp, seen, observed: &HashMap<PathBuf, bool>, missed| {
            mark_unseen(stamp, &lock_file, seen, t0, observed, missed)
        };
        // Edited during the load: reload next time. The lock gets the revision the load used.
        let stamp = vec![(manifest.clone(), Some((new, 10))), (lock_file.clone(), Some((new, 20)))];
        let marked = mark(stamp, Some((old, 20)), &none, &no_missed);
        assert_eq!(marked[0].1, UNKNOWN);
        assert_eq!(marked[1].1, Some((old, 20)), "a lock replaced after the load differs next time");
        let older = vec![(manifest.clone(), Some((old, 10)))];
        assert_eq!(mark(older.clone(), None, &none, &no_missed), older);
        // A module the load didn't see.
        let missed: std::collections::HashSet<PathBuf> = [manifest.clone()].into();
        assert_eq!(mark(older.clone(), None, &none, &missed)[0].1, UNKNOWN);
        // Test directories: compared as directories. One that appeared differs; a file at that
        // path is as absent as the load saw it, every time (no endless reloads).
        let tests = dir.path().join("tests");
        let seen_missing: HashMap<PathBuf, bool> = [(tests.clone(), false)].into();
        std::fs::write(&tests, "").unwrap();
        let entry = vec![(tests.clone(), Some((old, 0)))];
        assert_eq!(mark(entry.clone(), None, &seen_missing, &no_missed), entry);
        std::fs::remove_file(&tests).unwrap();
        std::fs::create_dir(&tests).unwrap();
        assert_eq!(mark(entry.clone(), None, &seen_missing, &no_missed)[0].1, UNKNOWN);
    }

    #[test]
    fn class_names_are_modified_utf8() {
        assert_eq!(modified_utf8(b"a/b/Main").as_deref(), Some("a/b/Main"));
        assert_eq!(modified_utf8("é/Ünï".as_bytes()).as_deref(), Some("é/Ünï"));
        // U+1F600 as a surrogate pair, three bytes each (not 4-byte UTF-8).
        assert_eq!(modified_utf8(&[0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]).as_deref(), Some("😀"));
        assert_eq!(modified_utf8(&[0xC0, 0x80]).as_deref(), Some("\0"));
        assert_eq!(modified_utf8(&[0xF0, 0x9F, 0x98, 0x80]), None, "4-byte UTF-8 isn't modified UTF-8");
    }

    #[test]
    fn exit_without_shutdown_is_an_error() {
        let sink = Sink::default();
        let server = Server::new(PathBuf::from("."), Box::new(sink));
        assert_eq!(server.serve(&frame(&json!({"jsonrpc": "2.0", "method": "build/exit"}))[..]).unwrap(), 1);
    }
}
