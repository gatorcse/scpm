//! `scpm test` and `scpm run`: forked JVMs on the locked runtime classpaths.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::compile::{CompileError, Toolchain, classes_dir};
use crate::install::{self, ClasspathKind};
use crate::lockfile::ClasspathEntry;
use crate::report::{Event, Reporter, TaskStatus, TestCase, TestFailure, plain_text};
use crate::worker::{self, Worker};
use crate::workspace::Variant;

/// How long a test JVM may take to exit after it reported its results.
const EXIT_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default)]
pub struct TestOptions {
    /// Globs on fully qualified suite names.
    pub filters: Vec<String>,
    /// Only these test cases.
    pub test_names: Vec<String>,
    /// Per-variant timeout for the forked test JVM (`None`: no limit).
    pub timeout: Option<Duration>,
}

/// `.scpm/out/<id>/test-classes`.
pub fn test_classes_dir(root: &Path, id: &str) -> PathBuf {
    root.join(".scpm/out").join(id).join("test-classes")
}

/// The runtime classpath of a variant as files: its own outputs and resources, upstream
/// modules' outputs and resources, then locked jars. Missing outputs or jars are errors
/// (missing resource directories are just skipped).
pub fn runtime_classpath(tc: &Toolchain, v: &Variant, test: bool) -> Result<Vec<PathBuf>, CompileError> {
    let ws = tc.ws;
    let kind = if test { ClasspathKind::TestRuntime } else { ClasspathKind::Runtime };
    let resources =
        |dirs: &[PathBuf]| -> Vec<PathBuf> { dirs.iter().map(|r| ws.root.join(r)).filter(|p| p.is_dir()).collect() };
    let mut required = Vec::new();
    let mut cp = Vec::new();
    if test {
        required.push(cp.len());
        cp.push(test_classes_dir(&ws.root, &v.id));
        cp.extend(resources(&v.test_resources));
    }
    required.push(cp.len());
    cp.push(classes_dir(&ws.root, &v.id));
    cp.extend(resources(&v.resources));
    let vlock = tc.lock.variants.get(&v.id).ok_or_else(|| install::InstallError::Stale(vec![v.id.clone()]))?;
    for entry in kind.entries(vlock) {
        required.push(cp.len());
        match entry {
            ClasspathEntry::Module { module } => {
                cp.push(classes_dir(&ws.root, module));
                if let Some(up) = ws.variant(module) {
                    cp.extend(resources(&up.resources));
                }
            }
            ClasspathEntry::Artifact(key) => cp.push(tc.file(key)),
        }
    }
    if let Some(missing) = required.iter().map(|&i| &cp[i]).find(|p| !p.exists()) {
        return Err(CompileError::Setup(format!(
            "{} is missing; run `scpm install` / `scpm compile`",
            missing.display()
        )));
    }
    Ok(cp)
}

/// The test runner's classpath, checked before anything is compiled.
pub fn runner_classpath(tc: &Toolchain) -> Result<Vec<PathBuf>, CompileError> {
    let mut cp = vec![worker::helper_jar("SCPM_TEST_RUNNER_JAR", "scpm-test-runner")?];
    cp.extend(tc.boot(&install::test_runner_boot())?);
    Ok(cp)
}

/// Variants among `targets` that have test sources.
pub fn with_tests<'a>(tc: &'a Toolchain, targets: &BTreeSet<String>) -> Vec<&'a Variant> {
    targets
        .iter()
        .filter_map(|id| tc.ws.variant(id))
        .filter(|v| !crate::compile::source_files(&tc.ws.root, &v.test_sources).is_empty())
        .collect()
}

/// Removes old JUnit reports of `variants`, so a run that fails before tests start (e.g. a
/// compile error) can't leave a stale passing report for CI to pick up.
pub fn clear_reports(tc: &Toolchain, variants: &[&Variant]) {
    for v in variants {
        let _ = fs::remove_dir_all(tc.ws.root.join(".scpm/out").join(&v.id).join("test-reports"));
    }
}

/// Runs the tests of `variants` (already compiled). Returns whether all passed.
pub fn run_tests(
    tc: &Toolchain,
    variants: &[&Variant],
    runner_cp: &[PathBuf],
    opts: &TestOptions,
    reporter: &Arc<Reporter>,
) -> bool {
    reporter.set_stream_logs(variants.len() == 1);
    let all_ok = Mutex::new(true);
    let queue = Mutex::new(variants.to_vec());
    let parallelism = std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(2).clamp(1, 4);
    std::thread::scope(|s| {
        for _ in 0..parallelism {
            s.spawn(|| {
                while let Some(v) = queue.lock().expect("queue").pop() {
                    if !run_variant_tests(tc, v, runner_cp, opts, reporter) {
                        *all_ok.lock().expect("ok") = false;
                    }
                }
            });
        }
    });
    all_ok.into_inner().expect("ok")
}

/// Resolves a runner-reported failure file (relative to its source root) to a
/// workspace-relative path: by relative path first, then by file name (a file's package need
/// not match its directory in Scala).
fn resolve_failure_file(root: &Path, roots: &[PathBuf], rel: &str) -> Option<String> {
    let found = roots.iter().map(|d| d.join(rel)).find(|p| root.join(p).is_file()).or_else(|| {
        let name = Path::new(rel).file_name()?;
        roots.iter().find_map(|d| {
            ignore::WalkBuilder::new(root.join(d))
                .standard_filters(false)
                .build()
                .flatten()
                .find(|e| e.file_name() == name && e.path().is_file())
                .and_then(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
        })
    })?;
    Some(found.to_string_lossy().replace('\\', "/"))
}

fn run_variant_tests(
    tc: &Toolchain,
    v: &Variant,
    runner_cp: &[PathBuf],
    opts: &TestOptions,
    reporter: &Arc<Reporter>,
) -> bool {
    let task = format!("{}#test", v.id);
    reporter.emit(Event::TaskStart { task: task.clone() });
    let started = Instant::now();
    let cases: Arc<Mutex<Vec<TestCase>>> = Arc::new(Mutex::new(Vec::new()));
    let (status, message) = match fork_runner(tc, v, runner_cp, opts, reporter, &task, &cases) {
        Ok(outcome) => outcome,
        Err(e) => (TaskStatus::Crashed, Some(e.to_string())),
    };
    let cases = cases.lock().expect("cases").clone();
    if let Err(e) = write_junit(&tc.ws.root, v, &cases, status, message.as_deref()) {
        reporter.emit(Event::Log { task: task.clone(), level: "warn".into(), message: format!("JUnit report: {e}") });
    }
    reporter.emit(Event::TaskEnd { task, status, duration_ms: started.elapsed().as_millis() as u64, message });
    status == TaskStatus::Ok
}

fn fork_runner(
    tc: &Toolchain,
    v: &Variant,
    runner_cp: &[PathBuf],
    opts: &TestOptions,
    reporter: &Arc<Reporter>,
    task: &str,
    cases: &Arc<Mutex<Vec<TestCase>>>,
) -> Result<(TaskStatus, Option<String>), CompileError> {
    let mut classpath = runner_cp.to_vec();
    classpath.extend(runtime_classpath(tc, v, true)?);
    let module_dir = tc.ws.module(&v.module).map(|m| tc.ws.root.join(&m.dir)).unwrap_or_else(|| tc.ws.root.clone());
    let spec = worker::Spawn {
        name: "the test JVM",
        classpath: &classpath,
        main: "scpm.testrunner.Main",
        jvm_options: &v.jvm_options,
        env: v.env.iter().map(|(k, val)| (k.clone(), val.clone())).collect(),
        cwd: Some(&module_dir),
        log_file: None,
        initialize: false,
        own_process_group: true,
    };
    let on_event = {
        let reporter = reporter.clone();
        let cases = cases.clone();
        let task = task.to_string();
        let roots: Vec<PathBuf> = v.test_sources.iter().chain(&v.sources).cloned().collect();
        let ws_root = tc.ws.root.clone();
        move |e: Value| match e["t"].as_str() {
            Some("test.case") => {
                let failure = e.get("failure").filter(|f| f.is_object()).map(|f| TestFailure {
                    message: plain_text(f["message"].as_str().unwrap_or_default()),
                    file: f["file"].as_str().and_then(|rel| resolve_failure_file(&ws_root, &roots, rel)),
                    line: f["line"].as_u64(),
                    stack_trace: f["stackTrace"].as_str().map(plain_text),
                });
                let case = TestCase {
                    task: task.clone(),
                    suite: e["suite"].as_str().unwrap_or_default().to_string(),
                    name: e["name"].as_str().unwrap_or_default().to_string(),
                    status: e["status"].as_str().unwrap_or("error").to_string(),
                    duration_ms: e["durationMs"].as_u64().unwrap_or(0),
                    failure,
                };
                cases.lock().expect("cases").push(case.clone());
                reporter.emit(Event::TestCase(case));
            }
            Some("log") => reporter.emit(Event::Log {
                task: task.clone(),
                level: e["level"].as_str().unwrap_or("info").to_string(),
                message: e["message"].as_str().unwrap_or_default().to_string(),
            }),
            _ => {}
        }
    };
    let runner = Arc::new(Worker::spawn(spec, on_event)?);
    let started = Instant::now();

    // The watchdog stops waiting as soon as the run finishes (no thread sleeps out the full
    // timeout), and on timeout kills the runner's whole process group.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = opts.timeout.map(|limit| {
        let runner = runner.clone();
        std::thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(limit) {
                runner.kill(&format!("timed out after {}s", limit.as_secs()));
            }
        })
    });
    let params = json!({
        "target": v.id,
        "testClassesDirs": [test_classes_dir(&tc.ws.root, &v.id)],
        "filters": opts.filters,
        "testNames": opts.test_names,
    });
    let result = runner.request("run", params);
    let _ = done_tx.send(());
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    let remaining = opts.timeout.map(|t| t.saturating_sub(started.elapsed()).min(EXIT_GRACE)).unwrap_or(EXIT_GRACE);
    let exit = Arc::try_unwrap(runner).ok().and_then(|r| r.wait_timeout(Some(remaining)));
    Ok(match result {
        Ok(r) if r["status"] == "ok" && exit.is_none() => (
            TaskStatus::Crashed,
            Some("the test JVM did not exit after reporting (non-daemon threads or shutdown hooks?)".into()),
        ),
        Ok(r) if r["status"] == "ok" => (TaskStatus::Ok, None),
        Ok(r) if r["status"] == "failed" => (TaskStatus::Failed, None),
        Ok(r) => (TaskStatus::Crashed, Some(r["message"].as_str().unwrap_or("the test runner crashed").to_string())),
        Err(e) => {
            let message = match exit.and_then(|s| s.code()) {
                Some(code) => {
                    format!("the test JVM exited with code {code} before reporting (does a test call System.exit?)")
                }
                None => e.to_string(),
            };
            (TaskStatus::Crashed, Some(message))
        }
    })
}

/// `.scpm/out/<id>/test-reports/junit.xml`, one `<testsuite>` per suite. A crashed or timed-out
/// run adds a synthetic error case so CI never shows it as green.
fn write_junit(
    root: &Path,
    v: &Variant,
    cases: &[TestCase],
    status: TaskStatus,
    message: Option<&str>,
) -> std::io::Result<()> {
    let dir = root.join(".scpm/out").join(&v.id).join("test-reports");
    fs::create_dir_all(&dir)?;
    let mut cases = cases.to_vec();
    if status == TaskStatus::Crashed {
        cases.push(TestCase {
            task: format!("{}#test", v.id),
            suite: "(test runner)".into(),
            name: "(run)".into(),
            status: "error".into(),
            duration_ms: 0,
            failure: Some(TestFailure {
                message: message.unwrap_or("the test JVM crashed").into(),
                file: None,
                line: None,
                stack_trace: None,
            }),
        });
    }
    let suites: BTreeSet<&str> = cases.iter().map(|c| c.suite.as_str()).collect();
    let mut xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites name=\"{}\">\n", esc(&v.id));
    for suite in suites {
        let in_suite: Vec<&TestCase> = cases.iter().filter(|c| c.suite == suite).collect();
        let failures = in_suite.iter().filter(|c| c.status == "failed").count();
        let errors = in_suite.iter().filter(|c| c.status == "error").count();
        let skipped = in_suite.iter().filter(|c| c.is_skipped()).count();
        let time: u64 = in_suite.iter().map(|c| c.duration_ms).sum();
        xml.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\" time=\"{:.3}\">\n",
            esc(suite),
            in_suite.len(),
            time as f64 / 1000.0
        ));
        xml.push_str(&format!("    <properties><property name=\"variant\" value=\"{}\"/></properties>\n", esc(&v.id)));
        for c in in_suite {
            xml.push_str(&format!(
                "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\"",
                esc(suite),
                esc(&c.name),
                c.duration_ms as f64 / 1000.0
            ));
            if c.is_failure() {
                let f = c.failure.as_ref();
                let tag = if c.status == "error" { "error" } else { "failure" };
                xml.push_str(&format!(
                    ">\n      <{tag} message=\"{}\">{}</{tag}>\n    </testcase>\n",
                    esc(f.map(|f| f.message.as_str()).unwrap_or("")),
                    esc_text(f.and_then(|f| f.stack_trace.as_deref()).unwrap_or(""))
                ));
            } else if c.is_skipped() {
                xml.push_str(">\n      <skipped/>\n    </testcase>\n");
            } else {
                xml.push_str("/>\n");
            }
        }
        xml.push_str("  </testsuite>\n");
    }
    xml.push_str("</testsuites>\n");
    fs::write(dir.join("junit.xml"), xml)
}

/// XML text content: escapes markup and drops characters XML 1.0 forbids.
fn esc_text(s: &str) -> String {
    plain_text(s).replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// XML attribute values: also escapes quotes and keeps newlines (parsers normalize raw ones).
fn esc(s: &str) -> String {
    esc_text(s).replace('"', "&quot;").replace('\n', "&#10;").replace('\t', "&#9;")
}

/// Runs a main class on `v`'s runtime classpath with inherited stdio. On Unix the scpm process
/// is replaced by the JVM, so signals and exit codes behave exactly as for `java` itself;
/// elsewhere the exit code is passed back.
pub fn exec_main(tc: &Toolchain, v: &Variant, main_class: Option<&str>, args: &[String]) -> Result<i32, CompileError> {
    let main = main_class.or(v.main_class.as_deref()).ok_or_else(|| {
        CompileError::Setup(format!("{} has no `mainClass`; set it in scpm.yaml or pass --main", v.module))
    })?;
    let classpath = runtime_classpath(tc, v, false)?;
    let java = worker::java()?;
    let joined = std::env::join_paths(&classpath).map_err(|e| CompileError::Setup(e.to_string()))?;
    let mut cmd = Command::new(&java.path);
    cmd.args(&v.jvm_options).arg("-cp").arg(joined).arg(main).args(args).envs(v.env.iter());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(CompileError::Setup(format!("cannot start {}: {err}", java.path.display())))
    }
    #[cfg(not(unix))]
    {
        let status =
            cmd.status().map_err(|e| CompileError::Setup(format!("cannot start {}: {e}", java.path.display())))?;
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escaping() {
        assert_eq!(esc("a<b & \"c\"\nd\u{7}"), "a&lt;b &amp; &quot;c&quot;&#10;d");
        assert_eq!(esc_text("x\u{1b}[31my"), "xy");
    }

    #[test]
    fn failure_files_resolve_by_path_then_name() {
        let ws = tempfile::tempdir().unwrap();
        fs::create_dir_all(ws.path().join("m/src/test/scala/other")).unwrap();
        fs::write(ws.path().join("m/src/test/scala/other/Weird.scala"), "").unwrap();
        let roots = vec![PathBuf::from("m/src/test/scala")];
        assert_eq!(
            resolve_failure_file(ws.path(), &roots, "other/Weird.scala").as_deref(),
            Some("m/src/test/scala/other/Weird.scala")
        );
        assert_eq!(
            resolve_failure_file(ws.path(), &roots, "app/weird/Weird.scala").as_deref(),
            Some("m/src/test/scala/other/Weird.scala")
        );
        assert_eq!(resolve_failure_file(ws.path(), &roots, "Nope.scala"), None);
    }
}
