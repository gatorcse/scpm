//! `scpm compile` end to end: needs `cs`, a JDK, network access, and the worker jar built by
//! `sbt package` in worker/ (or SCPM_WORKER_JAR). Skipped with a message otherwise.

use std::fs;
use std::path::{Path, PathBuf};

use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value;

mod shared_daemon;

/// Prerequisites of the daemon tests: also a platform the daemon runs on.
fn daemon_prerequisites() -> bool {
    prerequisites() && scpm::daemon::SUPPORTED
}

fn prerequisites() -> bool {
    let cs = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    let has_cs = StdCommand::new(cs).arg("version").output().is_ok_and(|o| o.status.success());
    let has_worker = scpm::worker::helper_jar("SCPM_WORKER_JAR", "scpm-compile-worker").is_ok();
    if !(has_cs && has_worker) {
        eprintln!("skipping: needs `cs` and the worker jar (run `sbt package` in worker/)");
    }
    has_cs && has_worker
}

fn workspace() -> tempfile::TempDir {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/app");
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&src, dir.path());
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn worker_jar() -> PathBuf {
    scpm::worker::helper_jar("SCPM_WORKER_JAR", "scpm-compile-worker").expect("checked by prerequisites()")
}

fn scpm(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("scpm").unwrap();
    cmd.arg("-C")
        .arg(dir)
        .env("COURSIER_CACHE", Path::new(env!("CARGO_TARGET_TMPDIR")).join("coursier-cache"))
        .env("SCPM_WORKER_JAR", worker_jar())
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        // The shared warm daemon; tests of the daemon itself override this with their own.
        .envs(shared_daemon::env())
        .timeout(std::time::Duration::from_secs(300));
    cmd
}

/// `scpm` with its own cold worker and no daemon: the path that tests of the worker's start
/// (and a few others, to keep it covered) take.
fn cold_scpm(dir: &Path) -> Command {
    let mut cmd = scpm(dir);
    cmd.env("SCPM_NO_DAEMON", "1");
    cmd
}

fn compile_json(dir: &Path, args: &[&str]) -> (bool, Value) {
    json_doc(scpm(dir), args)
}

fn cold_compile_json(dir: &Path, args: &[&str]) -> (bool, Value) {
    json_doc(cold_scpm(dir), args)
}

fn json_doc(mut cmd: Command, args: &[&str]) -> (bool, Value) {
    let out = cmd.args(["--json", "compile"]).args(args).output().unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!("stdout: {}\nstderr: {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    });
    (out.status.success(), doc)
}

fn statuses(doc: &Value) -> Vec<(String, String)> {
    doc["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["task"].as_str().unwrap().to_string(), t["status"].as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn compiles_cross_built_and_java_modules_then_skips_unchanged_ones() {
    if !prerequisites() {
        return;
    }
    // On the cold path on purpose: its own worker JVM, and no daemon (as in CI by default).
    let ws = workspace();
    let (ok, doc) = cold_compile_json(ws.path(), &["--tests"]);
    assert!(ok, "{doc:#}");
    let mut tasks = statuses(&doc);
    tasks.sort();
    let names: Vec<&str> = tasks.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        names,
        [
            "app@jvm-3#compile",
            "app@jvm-3#test-compile",
            "javalib@jvm#compile",
            "util@jvm-2.13#compile",
            "util@jvm-3#compile"
        ]
    );
    assert!(ws.path().join(".scpm/out/util@jvm-2.13/classes/util/Platform$.class").is_file());
    assert!(ws.path().join(".scpm/out/app@jvm-3/test-classes/app/MainSuite.class").is_file());

    // Nothing changed: every task is up to date and no JVM is started (no new worker log).
    let log = ws.path().join(".scpm/logs/worker.log");
    let before = fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let (ok, doc) = cold_compile_json(ws.path(), &["--tests"]);
    assert!(ok);
    assert!(statuses(&doc).iter().all(|(_, s)| s == "upToDate"), "{doc:#}");
    assert_eq!(fs::metadata(&log).map(|m| m.len()).unwrap_or(0), before, "the worker must not start");

    // Changing a Scala 3-only source recompiles util@jvm-3 (and checks app) but not 2.13.
    let platform = ws.path().join("modules/util/src/main/scala-3/util/Platform.scala");
    fs::write(&platform, "package util\n\nobject Platform:\n  def name: String = \"scala 3!\"\n").unwrap();
    let (ok, doc) = cold_compile_json(ws.path(), &[]);
    assert!(ok);
    let s = statuses(&doc);
    assert!(s.contains(&("util@jvm-3#compile".into(), "ok".into())), "{s:?}");
    assert!(s.contains(&("util@jvm-2.13#compile".into(), "upToDate".into())), "{s:?}");
}

#[test]
fn reports_diagnostics_and_skips_dependents() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    fs::write(
        ws.path().join("modules/util/src/main/scala-3/util/Broken.scala"),
        "package util\n\nval broken: Int = \"nope\"\n",
    )
    .unwrap();
    let (ok, doc) = compile_json(ws.path(), &["app"]);
    assert!(!ok);
    let d = &doc["diagnostics"][0];
    assert_eq!(d["file"], "modules/util/src/main/scala-3/util/Broken.scala");
    assert_eq!(d["severity"], "error");
    assert_eq!(d["code"], "E007");
    assert_eq!(d["range"]["start"], serde_json::json!({"line": 3, "col": 19}));
    assert!(d.get("rendered").is_none(), "JSON output stays compact");
    let s = statuses(&doc);
    assert!(s.contains(&("util@jvm-3#compile".into(), "failed".into())), "{s:?}");
    assert!(s.contains(&("app@jvm-3#compile".into(), "skipped".into())), "{s:?}");

    // The same run in GitHub Actions: workflow-command annotations and a step summary.
    let summary = ws.path().join("summary.md");
    let out = scpm(ws.path())
        .args(["compile", "app"])
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("::error file=modules/util/src/main/scala-3/util/Broken.scala,line=3,col=19,"), "{out}");
    let md = fs::read_to_string(summary).unwrap();
    assert!(md.contains("❌ failed") && md.contains("`modules/util/src/main/scala-3/util/Broken.scala:3:19`"), "{md}");

    // Every run leaves an event log.
    let log = fs::read_to_string(ws.path().join(".scpm/runs/latest.ndjson")).unwrap();
    assert!(log.lines().any(|l| l.contains(r#""t":"diagnostic""#)));
    assert!(log.lines().last().unwrap().contains(r#""t":"run.end","ok":false"#));
}

/// Adds a `testkit` module used by app's tests, whose own tests use app: a test-scope cycle.
fn add_testkit(ws: &Path) {
    let dir = ws.join("modules/testkit");
    fs::create_dir_all(dir.join("src/main/scala/testkit")).unwrap();
    fs::write(dir.join("scpm.yaml"), "dependencies:\n  util: workspace:*\ntestDependencies:\n  app: workspace:*\n")
        .unwrap();
    fs::write(
        dir.join("src/main/scala/testkit/Fixtures.scala"),
        "package testkit\n\nobject Fixtures:\n  def names: List[String] = List(\"a\", \"b\")\n",
    )
    .unwrap();
    let app = ws.join("modules/app/scpm.yaml");
    let text = fs::read_to_string(&app).unwrap();
    fs::write(&app, text.replace("testDependencies:\n", "testDependencies:\n  testkit: workspace:*\n")).unwrap();
    fs::write(
        ws.join("modules/app/src/test/scala/app/KitSuite.scala"),
        "package app\n\nclass KitSuite extends munit.FunSuite:\n  test(\"kit\") { assertEquals(testkit.Fixtures.names.size, 2) }\n",
    )
    .unwrap();
}

#[test]
fn test_scope_cycles_compile_and_test_fingerprints_follow_test_dependencies() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    add_testkit(ws.path());
    let (ok, doc) = compile_json(ws.path(), &["--tests"]);
    assert!(ok, "a test-scope cycle must not hang or fail: {doc:#}");

    // Renaming something only a test uses must recompile (and break) that test.
    fs::write(
        ws.path().join("modules/testkit/src/main/scala/testkit/Fixtures.scala"),
        "package testkit\n\nobject Fixtures:\n  def names2: List[String] = Nil\n",
    )
    .unwrap();
    let (ok, doc) = compile_json(ws.path(), &["--tests", "app"]);
    assert!(!ok);
    assert!(statuses(&doc).contains(&("app@jvm-3#test-compile".into(), "failed".into())), "{doc:#}");

    // Without --tests, a broken test-only dependency doesn't matter.
    let (ok, doc) = compile_json(ws.path(), &["app"]);
    assert!(ok, "{doc:#}");
    assert!(!statuses(&doc).iter().any(|(t, _)| t.starts_with("testkit")), "{doc:#}");
}

#[test]
fn deleting_every_source_clears_old_classes() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    assert!(compile_json(ws.path(), &["app"]).0);
    fs::remove_file(ws.path().join("modules/javalib/src/main/java/javalib/Shout.java")).unwrap();
    let (ok, doc) = compile_json(ws.path(), &["app"]);
    assert!(!ok, "app uses javalib.Shout, which no longer exists: {doc:#}");
    assert!(!ws.path().join(".scpm/out/javalib@jvm/classes/javalib/Shout.class").exists());
}

#[test]
fn deleting_every_test_source_clears_old_test_classes() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    assert!(compile_json(ws.path(), &["--tests", "app"]).0);
    let out = ws.path().join(".scpm/out/app@jvm-3");
    assert!(out.join("test-classes/app/MainSuite.class").is_file());
    fs::remove_file(ws.path().join("modules/app/src/test/scala/app/MainSuite.scala")).unwrap();
    let (ok, doc) = compile_json(ws.path(), &["--tests", "app"]);
    assert!(ok, "{doc:#}");
    assert!(!out.join("test-classes").exists());
    assert!(!out.join("test-analysis.zip").exists());
    assert!(!out.join("test-fingerprint").exists());
}

#[test]
fn json_output_is_one_document_when_the_worker_cannot_start() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    let out = cold_scpm(ws.path())
        .args(["--json", "compile", "app"])
        .env("SCPM_WORKER_JAR", "/nonexistent/worker.jar")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).expect("exactly one JSON document");
    assert_eq!(doc["ok"], false);
    assert!(doc["error"].as_str().unwrap().contains("SCPM_WORKER_JAR points to /nonexistent/worker.jar"), "{doc:#}");
}

#[test]
fn runs_tests_and_mains() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    let out = scpm(ws.path()).args(["run", "app", "--", "Ann", "Bob"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "HELLO, ANN; HELLO, BOB!\n", "build output stays off stdout");
    // `run` intersects its target with the global --filter.
    for filter in ["!app", "util"] {
        let out = scpm(ws.path()).args(["-F", filter, "run", "app"]).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "-F {filter}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("`app` is excluded by --filter"), "{out:?}");
    }
    for filter in ["!util", "app"] {
        let out = scpm(ws.path()).args(["-F", filter, "run", "app", "--", "Ann"]).output().unwrap();
        assert!(out.status.success(), "-F {filter}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "HELLO, ANN!\n");
    }

    let out = scpm(ws.path()).args(["--json", "test", "app"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["tests"]["passed"], 2);

    fs::write(
        ws.path().join("modules/app/src/test/scala/app/FailSuite.scala"),
        "package app\n\nclass FailSuite extends munit.FunSuite:\n  test(\"fails\") {\n    assertEquals(1 + 1, 3)\n  }\n",
    )
    .unwrap();
    let out = scpm(ws.path()).args(["--json", "test", "app", "--suite", "app.Fail*"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["tests"]["passed"], 0, "--suite filters suites");
    let f = &doc["tests"]["failures"][0];
    assert_eq!(f["suite"], "app.FailSuite");
    assert_eq!(f["failure"]["file"], "modules/app/src/test/scala/app/FailSuite.scala");
    assert_eq!(f["failure"]["line"], 5);
    assert!(!f["failure"]["message"].as_str().unwrap().contains('\u{1b}'), "no ANSI in JSON");
    let junit = fs::read_to_string(ws.path().join(".scpm/out/app@jvm-3/test-reports/junit.xml")).unwrap();
    assert!(junit.contains(r#"<testsuite name="app.FailSuite" tests="1" failures="1""#), "{junit}");
    assert!(!junit.contains('\u{1b}'));

    // A program's exit code passes through.
    fs::write(
        ws.path().join("modules/app/src/main/scala/app/Exit.scala"),
        "package app\n\n@main def Exit(): Unit = sys.exit(7)\n",
    )
    .unwrap();
    let out = scpm(ws.path()).args(["run", "app", "--main", "app.Exit"]).output().unwrap();
    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn run_compiles_runtime_only_dependencies() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    let rt = ws.path().join("modules/rt");
    fs::create_dir_all(rt.join("src/main/java/rt")).unwrap();
    fs::write(rt.join("scpm.yaml"), "scala: none\n").unwrap();
    fs::write(
        rt.join("src/main/java/rt/Impl.java"),
        "package rt;\npublic class Impl { public String toString() { return \"rt!\"; } }\n",
    )
    .unwrap();
    let app = ws.path().join("modules/app/scpm.yaml");
    fs::write(&app, fs::read_to_string(&app).unwrap() + "runtimeDependencies:\n  rt: workspace:*\n").unwrap();
    fs::write(
        ws.path().join("modules/app/src/main/scala/app/Reflect.scala"),
        "package app\n\n@main def Reflect(): Unit = println(Class.forName(\"rt.Impl\").getDeclaredConstructor().newInstance())\n",
    )
    .unwrap();
    let out = scpm(ws.path()).args(["run", "app", "--main", "app.Reflect"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "rt!\n");
}

#[test]
fn test_failures_crashes_and_timeouts_are_reported() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    let tests = ws.path().join("modules/app/src/test/scala/app");
    // A filter that matches nothing is an error, not a silent pass.
    let out = scpm(ws.path()).args(["--json", "test", "app", "--suite", "app.NoSuch*"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["error"].as_str().unwrap().contains("no tests matched"), "{doc:#}");

    // System.exit in a test: crashed, and the JUnit report says so.
    fs::write(
        tests.join("ExitSuite.scala"),
        "package app\n\nclass ExitSuite extends munit.FunSuite:\n  test(\"exits\") { sys.exit(3) }\n",
    )
    .unwrap();
    let out = scpm(ws.path()).args(["--json", "test", "app", "--suite", "app.ExitSuite"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let task = doc["tasks"].as_array().unwrap().iter().find(|t| t["task"] == "app@jvm-3#test").unwrap().clone();
    assert_eq!(task["status"], "crashed", "{doc:#}");
    let junit = fs::read_to_string(ws.path().join(".scpm/out/app@jvm-3/test-reports/junit.xml")).unwrap();
    assert!(junit.contains("<error message="), "{junit}");
    fs::remove_file(tests.join("ExitSuite.scala")).unwrap();

    // A JVM that reports success but then exits nonzero (here from a shutdown hook) crashed.
    fs::write(
        tests.join("HaltSuite.scala"),
        "package app\n\nclass HaltSuite extends munit.FunSuite:\n  test(\"halts later\") { sys.addShutdownHook(Runtime.getRuntime.halt(7)) }\n",
    )
    .unwrap();
    let out = scpm(ws.path()).args(["--json", "test", "app", "--suite", "app.HaltSuite"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let task = doc["tasks"].as_array().unwrap().iter().find(|t| t["task"] == "app@jvm-3#test").unwrap().clone();
    assert_eq!(task["status"], "crashed", "{doc:#}");
    assert!(task["message"].as_str().unwrap().contains("exited with code 7"), "{doc:#}");
    let junit = fs::read_to_string(ws.path().join(".scpm/out/app@jvm-3/test-reports/junit.xml")).unwrap();
    assert!(junit.contains("<error message="), "{junit}");
    fs::remove_file(tests.join("HaltSuite.scala")).unwrap();

    // A hanging test whose grandchild keeps stdout open is killed at the timeout, grandchild too.
    let pid_file = ws.path().join("sleep.pid");
    fs::write(
        tests.join("HangSuite.scala"),
        format!(
            "package app\n\nclass HangSuite extends munit.FunSuite:\n  override def munitTimeout = scala.concurrent.duration.Duration.Inf\n  test(\"hangs\") {{\n    val p = new ProcessBuilder(\"sleep\", \"60\").inheritIO().start()\n    java.nio.file.Files.writeString(java.nio.file.Path.of({:?}), p.pid.toString)\n    Thread.sleep(60000)\n  }}\n",
            pid_file.to_str().unwrap()
        ),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let out = scpm(ws.path())
        .args(["--json", "test", "app", "--suite", "app.HangSuite", "--timeout", "20"])
        .output()
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(75), "took {:?}", started.elapsed());
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let task = doc["tasks"].as_array().unwrap().iter().find(|t| t["task"] == "app@jvm-3#test").unwrap().clone();
    assert!(task["message"].as_str().unwrap().contains("timed out after 20s"), "{doc:#}");
    // The timeout must leave the test JVM time to start and reach the test body (slow CI runners
    // took over 5 s), or there is no grandchild to check.
    let pid = fs::read_to_string(&pid_file)
        .unwrap_or_else(|e| panic!("the test body never started before the timeout ({e}): {doc:#}"));
    let alive = || StdCommand::new("kill").args(["-0", pid.trim()]).status().is_ok_and(|s| s.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while alive() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if alive() {
        let _ = StdCommand::new("kill").args(["-9", pid.trim()]).status();
        panic!("the test's `sleep` (pid {}) outlived the timeout", pid.trim());
    }
}

#[test]
fn a_test_compile_failure_leaves_no_stale_junit_report() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    let junit = ws.path().join(".scpm/out/app@jvm-3/test-reports/junit.xml");
    assert!(scpm(ws.path()).args(["test", "app"]).output().unwrap().status.success());
    assert!(junit.is_file());
    fs::write(ws.path().join("modules/app/src/test/scala/app/Bad.scala"), "package app\n\nval bad: Int = \"x\"\n")
        .unwrap();
    let out = scpm(ws.path()).args(["test", "app"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!junit.exists(), "the previous run's passing report must not survive a failed build");

    // A name filter that only skips cases is "no tests matched" too.
    fs::remove_file(ws.path().join("modules/app/src/test/scala/app/Bad.scala")).unwrap();
    let out = scpm(ws.path()).args(["--json", "test", "app", "-t", "no such test"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["error"].as_str().unwrap_or_default().contains("no tests matched"), "{doc:#}");
}

/// An `scpm mcp` child driven by a test, with a deadline on every reply; a test that fails
/// (or times out) kills and reaps it.
struct Mcp {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Mcp {
    const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    fn spawn(mut cmd: StdCommand) -> Mcp {
        use std::io::BufRead;
        let mut child = cmd
            .arg("mcp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Mcp { child, stdin, lines }
    }

    fn send(&mut self, msg: Value) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// Reads messages until the response with `id`; every stdout line must be JSON-RPC.
    fn read_until(&mut self, id: u64) -> (Value, Vec<Value>) {
        let deadline = std::time::Instant::now() + Self::REPLY_TIMEOUT;
        let mut others = Vec::new();
        loop {
            let line = match self.lines.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(line) => line,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("no reply to request {id} in time"),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("server closed stdout"),
            };
            let msg: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line}"));
            assert_eq!(msg["jsonrpc"], "2.0", "{line}");
            if msg["id"] == id {
                return (msg, others);
            }
            others.push(msg);
        }
    }

    /// Closes stdin and waits (with a deadline) for the server to exit.
    fn finish(mut self) -> std::process::ExitStatus {
        self.stdin.take();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(std::time::Instant::now() < deadline, "the MCP server did not exit");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn removing_every_test_clears_the_report_and_filters_still_must_match() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    let junit = ws.path().join(".scpm/out/app@jvm-3/test-reports/junit.xml");
    assert!(scpm(ws.path()).args(["test", "app"]).output().unwrap().status.success());
    assert!(junit.is_file());
    fs::remove_dir_all(ws.path().join("modules/app/src/test")).unwrap();
    let out = scpm(ws.path()).args(["test", "app"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!junit.exists(), "a variant without tests keeps no old report");
    let out = scpm(ws.path()).args(["--json", "test", "app", "--suite", "app.*"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["error"].as_str().unwrap_or_default().contains("no tests matched"), "{doc:#}");
}

#[test]
fn mcp_server_compiles_and_reports() {
    if !prerequisites() {
        return;
    }
    let ws = workspace();
    fs::write(
        ws.path().join("modules/app/src/test/scala/app/LoudSuite.scala"),
        "package app\n\nclass LoudSuite extends munit.FunSuite:\n  test(\"loud\") {\n    println(\"to stdout\")\n    assertEquals(1, 2)\n  }\n",
    )
    .unwrap();
    let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin("scpm"));
    cmd.arg("-C")
        .arg(ws.path())
        .env("COURSIER_CACHE", Path::new(env!("CARGO_TARGET_TMPDIR")).join("coursier-cache"))
        .env("SCPM_WORKER_JAR", worker_jar())
        .envs(shared_daemon::env())
        .env_remove("CI");
    let mut mcp = Mcp::spawn(cmd);
    let call = |id: u64, name: &str, args: Value| serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}});
    mcp.send(
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
    );
    assert_eq!(mcp.read_until(1).0["result"]["serverInfo"]["name"], "scpm");

    // A ping sent while a compile runs is answered before the compile finishes.
    let mut compile = call(2, "compile", serde_json::json!({"targets": ["app"]}));
    compile["params"]["_meta"] = serde_json::json!({"progressToken": "p1"});
    mcp.send(compile);
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}));
    let (pong, before) = mcp.read_until(3);
    assert_eq!(pong["result"], serde_json::json!({}));
    assert!(!before.iter().any(|m| m["id"] == 2), "ping waited for the compile");
    let (ok, progress) = mcp.read_until(2);
    let r = &ok["result"]["structuredContent"];
    assert_eq!(r["ok"], true, "{ok:#}");
    assert_eq!(r["tasks"]["problems"], serde_json::json!([]), "a green compile lists no tasks");
    // Progress comes from the compile thread, so some may arrive before the ping's reply.
    assert!(
        before.iter().chain(&progress).any(|m| m["method"] == "notifications/progress"),
        "before: {before:?}; after: {progress:?}"
    );

    fs::write(
        ws.path().join("modules/app/src/main/scala/app/Broken.scala"),
        "package app\n\nval broken: Int = \"x\"\n",
    )
    .unwrap();
    mcp.send(call(4, "compile", serde_json::json!({"targets": ["app"]})));
    let r = mcp.read_until(4).0["result"]["structuredContent"].clone();
    assert_eq!(r["ok"], false);
    assert_eq!(r["diagnostics"][0]["file"], "modules/app/src/main/scala/app/Broken.scala");
    assert_eq!(r["tasks"]["problems"][0]["status"], "failed");

    mcp.send(call(5, "diagnostics", serde_json::json!({"file": "Broken.scala"})));
    let d = mcp.read_until(5).0["result"]["structuredContent"].clone();
    assert_eq!(d["diagnostics"][0]["code"], "E007", "{d:#}");
    fs::remove_file(ws.path().join("modules/app/src/main/scala/app/Broken.scala")).unwrap();

    // A failing test that prints to stdout: the result has the failure; stdout stays JSON.
    mcp.send(call(6, "test", serde_json::json!({"targets": ["app"], "suites": ["app.LoudSuite"]})));
    let t = mcp.read_until(6).0["result"].clone();
    assert_eq!(t["isError"], false, "{t:#}");
    assert_eq!(t["structuredContent"]["tests"]["failed"], 1, "{t:#}");
    assert_eq!(
        t["structuredContent"]["tests"]["failures"][0]["failure"]["file"],
        "modules/app/src/test/scala/app/LoudSuite.scala"
    );

    // A test call whose compile fails clears the old report, as `scpm test` does.
    let junit = ws.path().join(".scpm/out/app@jvm-3/test-reports/junit.xml");
    assert!(junit.is_file());
    fs::write(ws.path().join("modules/app/src/test/scala/app/Bad.scala"), "package app\n\nval bad: Int = \"x\"\n")
        .unwrap();
    mcp.send(call(16, "test", serde_json::json!({"targets": ["app"]})));
    let t = mcp.read_until(16).0["result"]["structuredContent"].clone();
    assert_eq!(t["ok"], false, "{t:#}");
    assert!(!junit.exists(), "a stale report survived a failed test compile");
    fs::remove_file(ws.path().join("modules/app/src/test/scala/app/Bad.scala")).unwrap();

    // As in the CLI, a module without tests passes unless filters asked for tests.
    mcp.send(call(17, "test", serde_json::json!({"targets": ["javalib"]})));
    let t = mcp.read_until(17).0["result"].clone();
    assert_eq!(t["isError"], false, "{t:#}");
    assert_eq!(t["structuredContent"]["ok"], true, "{t:#}");
    mcp.send(call(18, "test", serde_json::json!({"targets": ["javalib"], "suites": ["*"]})));
    let t = mcp.read_until(18).0["result"].clone();
    assert_eq!(t["isError"], true, "{t:#}");
    assert!(t["structuredContent"]["error"].as_str().unwrap_or_default().contains("no tests matched"), "{t:#}");

    // Every tool returns an object as structuredContent; bad arguments are tool errors.
    for (id, name, args) in [
        (7, "ls", serde_json::json!({})),
        (8, "config", serde_json::json!({"targets": ["javalib"]})),
        (9, "why", serde_json::json!({"artifact": "cats-kernel", "targets": ["util@jvm-3"]})),
        (10, "classpath", serde_json::json!({"target": "app"})),
        (11, "graph", serde_json::json!({})),
    ] {
        mcp.send(call(id, name, args));
        let r = mcp.read_until(id).0;
        assert!(r["result"]["structuredContent"].is_object(), "{name}: {r:#}");
    }
    mcp.send(call(12, "compile", serde_json::json!({"targets": "app"})));
    let bad = mcp.read_until(12).0;
    assert_eq!(bad["result"]["isError"], true);

    // A call cancelled while it waits behind a running build is never answered.
    mcp.send(call(13, "test", serde_json::json!({"targets": ["app"], "suites": ["app.MainSuite"]})));
    mcp.send(call(14, "compile", serde_json::json!({"targets": ["app"]})));
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 14}}));
    let (_, before13) = mcp.read_until(13);
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "id": 15, "method": "ping"}));
    let (_, before15) = mcp.read_until(15);
    assert!(!before13.iter().chain(&before15).any(|m| m["id"] == 14), "cancelled request 14 was answered");

    assert!(mcp.finish().success());
}

/// A private daemon directory with a short path, whatever `TMPDIR` is: a long one would
/// fall back to the user's shared `/tmp/scpm-<uid>` (tests also point the fallback here).
fn daemon_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("scpm-d").tempdir_in("/tmp").unwrap()
}

/// Stops the test's private daemons even if an assertion fails.
struct DaemonGuard(PathBuf);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = Command::cargo_bin("scpm")
            .unwrap()
            .args(["daemon", "stop"])
            .env("SCPM_DAEMON_DIR", &self.0)
            .env("SCPM_DAEMON_FALLBACK_DIR", &self.0)
            .output();
    }
}

#[test]
fn shared_daemon_serves_compiles_and_routes_diagnostics() {
    if !daemon_prerequisites() {
        return;
    }
    let ws = workspace();
    let daemons = daemon_dir();
    let _guard = DaemonGuard(daemons.path().to_path_buf());
    let with_daemon = |args: &[&str]| {
        let mut cmd = scpm(ws.path());
        cmd.env_remove("SCPM_NO_DAEMON")
            .env("SCPM_DAEMON_DIR", daemons.path())
            .env("SCPM_DAEMON_FALLBACK_DIR", daemons.path())
            .env("SCPM_DAEMON_IDLE_SECS", "300");
        cmd.args(args).output().unwrap()
    };
    let status = || -> Value {
        let out = Command::cargo_bin("scpm")
            .unwrap()
            .args(["--json", "daemon", "ls"])
            .env("SCPM_DAEMON_DIR", daemons.path())
            .env("SCPM_DAEMON_FALLBACK_DIR", daemons.path())
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let out = with_daemon(&["--json", "compile", "app"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let first = status();
    assert_eq!(first.as_array().unwrap().len(), 1, "{first:#}");
    let pid = first[0]["pid"].clone();
    let requests = first[0]["requests"].as_u64().unwrap();
    assert!(requests >= 1);
    assert!(
        first[0]["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| ws.path().canonicalize().unwrap() == Path::new(w.as_str().unwrap()))
    );

    // A diagnostic produced inside the daemon reaches the right client.
    fs::write(
        ws.path().join("modules/app/src/main/scala/app/Broken.scala"),
        "package app\n\nval broken: Int = \"x\"\n",
    )
    .unwrap();
    let out = with_daemon(&["--json", "compile", "app"]);
    assert_eq!(out.status.code(), Some(1));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["diagnostics"][0]["code"], "E007", "{doc:#}");
    let second = status();
    assert_eq!(second[0]["pid"], pid, "the same daemon served both");
    assert!(second[0]["requests"].as_u64().unwrap() > requests);

    // --no-daemon bypasses it.
    fs::remove_file(ws.path().join("modules/app/src/main/scala/app/Broken.scala")).unwrap();
    let before = status()[0]["requests"].as_u64().unwrap();
    let out = with_daemon(&["--no-daemon", "compile", "app"]);
    assert!(out.status.success());
    assert_eq!(status()[0]["requests"].as_u64().unwrap(), before);

    let out = Command::cargo_bin("scpm")
        .unwrap()
        .args(["daemon", "stop"])
        .env("SCPM_DAEMON_DIR", daemons.path())
        .env("SCPM_DAEMON_FALLBACK_DIR", daemons.path())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "stopped 1 daemon");
    assert_eq!(status(), serde_json::json!([]));
}

/// A private daemon directory for one test, and helpers to use it.
struct Daemons {
    // Fields drop in order: stop the daemons before their directory goes away.
    _guard: DaemonGuard,
    dir: tempfile::TempDir,
}

impl Daemons {
    fn new() -> Daemons {
        let dir = daemon_dir();
        let guard = DaemonGuard(dir.path().to_path_buf());
        Daemons { _guard: guard, dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `scpm` in `ws`, using this test's daemons.
    fn scpm(&self, ws: &Path) -> Command {
        let mut cmd = scpm(ws);
        cmd.env_remove("SCPM_NO_DAEMON")
            .env("SCPM_DAEMON_DIR", self.path())
            .env("SCPM_DAEMON_FALLBACK_DIR", self.path())
            .env("SCPM_DAEMON_IDLE_SECS", "300");
        cmd
    }

    /// The same as a `std::process::Command`, for processes the test drives itself.
    fn std_scpm(&self, ws: &Path) -> StdCommand {
        let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin("scpm"));
        cmd.arg("-C")
            .arg(ws)
            .env("COURSIER_CACHE", Path::new(env!("CARGO_TARGET_TMPDIR")).join("coursier-cache"))
            .env("SCPM_WORKER_JAR", worker_jar())
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("SCPM_NO_DAEMON")
            .env("SCPM_DAEMON_DIR", self.path())
            .env("SCPM_DAEMON_FALLBACK_DIR", self.path())
            .env("SCPM_DAEMON_IDLE_SECS", "300");
        cmd
    }

    fn ls(&self) -> Value {
        daemon_ls(self.path())
    }
}

fn daemon_ls(dir: &Path) -> Value {
    let out = Command::cargo_bin("scpm")
        .unwrap()
        .args(["--json", "daemon", "ls"])
        .env("SCPM_DAEMON_DIR", dir)
        .env("SCPM_DAEMON_FALLBACK_DIR", dir)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)))
}

/// Waits up to `limit` for `cond`; returns whether it held.
fn wait_for(limit: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    cond()
}

fn active_requests(ls: &Value) -> u64 {
    ls.as_array().unwrap().iter().filter_map(|d| d["activeRequests"].as_u64()).sum()
}

fn alive(pid: u64) -> bool {
    StdCommand::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Files of [`add_slow_sources`]: a cold compile of them takes well over a minute.
const SLOW_FILES: usize = 150;

/// How long a cancelled compile may take to stop. The compiler checks for cancellation
/// between units and phases; while it is still loading (slow on a loaded machine) it can't.
const CANCEL_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Sources that keep util@jvm-3 compiling for a while (many units, so a cancel takes effect
/// between them).
fn add_slow_sources(ws: &Path) {
    let dir = ws.join("modules/util/src/main/scala-3/util");
    for o in 0..SLOW_FILES {
        let mut src = format!("package util\n\nobject Slow{o}:\n");
        for i in 0..250 {
            src.push_str(&format!(
                "  def f{i}(xs: List[Int]): Map[String, List[(Int, String)]] = xs.map(x => (x + {i}, s\"v$x-{i}\")).groupBy(_._2.take(2)).map((k, v) => k -> v.sortBy(_._1).reverse)\n"
            ));
        }
        fs::write(dir.join(format!("Slow{o}.scala")), src).unwrap();
    }
}

fn remove_slow_sources(ws: &Path) {
    for o in 0..SLOW_FILES {
        let _ = fs::remove_file(ws.join(format!("modules/util/src/main/scala-3/util/Slow{o}.scala")));
    }
}

#[test]
fn concurrent_daemon_clients_only_see_their_own_diagnostics() {
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let (a, b) = (workspace(), workspace());
    fs::write(
        a.path().join("modules/app/src/main/scala/app/BrokenA.scala"),
        "package app\n\nval brokenA: Int = \"a\"\n",
    )
    .unwrap();
    fs::write(
        b.path().join("modules/app/src/main/scala/app/BrokenB.scala"),
        "package app\n\nval brokenB: Int = \"b\"\n",
    )
    .unwrap();
    let run = |ws: &Path| {
        let out = daemons.scpm(ws).args(["--json", "compile", "app"]).output().unwrap();
        assert!(!String::from_utf8_lossy(&out.stderr).contains("without the shared daemon"));
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let (doc_a, doc_b) = std::thread::scope(|s| {
        let ta = s.spawn(|| run(a.path()));
        let tb = s.spawn(|| run(b.path()));
        (ta.join().unwrap(), tb.join().unwrap())
    });
    let files = |doc: &Value| -> Vec<String> {
        doc["diagnostics"].as_array().unwrap().iter().map(|d| d["file"].as_str().unwrap_or("").to_string()).collect()
    };
    assert_eq!(files(&doc_a), ["modules/app/src/main/scala/app/BrokenA.scala"], "{doc_a:#}");
    assert_eq!(files(&doc_b), ["modules/app/src/main/scala/app/BrokenB.scala"], "{doc_b:#}");
    let ls = daemons.ls();
    assert_eq!(ls.as_array().unwrap().len(), 1, "{ls:#}");
    assert_eq!(ls[0]["workspaces"].as_array().unwrap().len(), 2, "{ls:#}");
}

#[test]
fn a_client_killed_mid_compile_has_its_compile_cancelled() {
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let ws = workspace();
    assert!(daemons.scpm(ws.path()).args(["compile", "javalib"]).output().unwrap().status.success());
    add_slow_sources(ws.path());
    let mut client = daemons
        .std_scpm(ws.path())
        .args(["compile", "util@jvm-3"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_for(std::time::Duration::from_secs(60), || active_requests(&daemons.ls()) > 0), "never started");
    std::thread::sleep(std::time::Duration::from_secs(1));
    client.kill().unwrap();
    client.wait().unwrap();
    let killed = std::time::Instant::now();
    assert!(
        wait_for(CANCEL_LIMIT, || active_requests(&daemons.ls()) == 0),
        "the daemon kept compiling for a dead client: {:#}",
        daemons.ls()
    );
    eprintln!("cancelled in {:?}", killed.elapsed());
    assert_eq!(daemons.ls()[0]["clients"], 0);
}

#[test]
fn mcp_cancel_reaches_the_daemon_and_leaves_no_stale_diagnostics() {
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let ws = workspace();
    add_slow_sources(ws.path());
    let broken = ws.path().join("modules/util/src/main/scala-3/util/Broken.scala");
    fs::write(&broken, "package util\n\nval broken: Int = \"x\"\n").unwrap();
    let mut mcp = Mcp::spawn(daemons.std_scpm(ws.path()));
    let call = |id: u64, args: Value| serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "compile", "arguments": args}});
    mcp.send(
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
    );
    mcp.read_until(1);

    // A long compile, cancelled while the daemon runs it.
    mcp.send(call(2, serde_json::json!({"targets": ["util@jvm-3"]})));
    assert!(wait_for(std::time::Duration::from_secs(120), || active_requests(&daemons.ls()) > 0), "never started");
    let pid = daemons.ls()[0]["pid"].clone();
    std::thread::sleep(std::time::Duration::from_secs(2));
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 2}}));
    // The same daemon stays up with nothing running (an empty list would also count as zero).
    assert!(
        wait_for(CANCEL_LIMIT, || {
            let ls = daemons.ls();
            ls.as_array().unwrap().len() == 1 && ls[0]["pid"] == pid && ls[0]["activeRequests"] == 0
        }),
        "the cancel did not reach the daemon: {:#}",
        daemons.ls()
    );
    let requests = daemons.ls()[0]["requests"].as_u64().unwrap();

    // The next call reports only its own diagnostics.
    remove_slow_sources(ws.path());
    fs::remove_file(&broken).unwrap();
    mcp.send(call(3, serde_json::json!({"targets": ["util@jvm-3"]})));
    let r = mcp.read_until(3).0["result"]["structuredContent"].clone();
    assert_eq!(r["ok"], true, "{r:#}");
    assert_eq!(r["diagnostics"], serde_json::json!([]), "{r:#}");
    let ls = daemons.ls();
    assert_eq!(ls.as_array().unwrap().len(), 1);
    assert_eq!(ls[0]["pid"], pid, "the original daemon served the next call: {ls:#}");
    assert!(ls[0]["requests"].as_u64().unwrap() > requests, "{ls:#}");

    assert!(mcp.finish().success());
}

#[test]
fn a_too_long_daemon_dir_falls_back_to_a_short_one() {
    if !daemon_prerequisites() {
        return;
    }
    // A private stand-in for /tmp/scpm-<uid>, short enough for socket paths.
    let short = tempfile::Builder::new().prefix("scpm-t").tempdir_in("/tmp").unwrap();
    let base = tempfile::tempdir().unwrap();
    let long = base.path().join("d".repeat(60)).join("e".repeat(40));
    struct Stop(PathBuf, PathBuf);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = Command::cargo_bin("scpm")
                .unwrap()
                .args(["daemon", "stop"])
                .env("SCPM_DAEMON_DIR", &self.0)
                .env("SCPM_DAEMON_FALLBACK_DIR", &self.1)
                .output();
        }
    }
    let _stop = Stop(long.clone(), short.path().to_path_buf());
    let ws = workspace();
    let started = std::time::Instant::now();
    let out = scpm(ws.path())
        .args(["compile", "javalib"])
        .env_remove("SCPM_NO_DAEMON")
        .env("SCPM_DAEMON_DIR", &long)
        .env("SCPM_DAEMON_FALLBACK_DIR", short.path())
        .env("SCPM_DAEMON_IDLE_SECS", "300")
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("without the shared daemon"), "{stderr}");
    // Only a hang guard: a cold compile includes JVM start, which takes 25 s+ on a loaded CI
    // runner. Correctness is the daemon being used at the short path (no fallback warning).
    assert!(elapsed < std::time::Duration::from_secs(120), "took {elapsed:?}");
    let out = Command::cargo_bin("scpm")
        .unwrap()
        .args(["--json", "daemon", "ls"])
        .env("SCPM_DAEMON_DIR", &long)
        .env("SCPM_DAEMON_FALLBACK_DIR", short.path())
        .output()
        .unwrap();
    let ls: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ls.as_array().unwrap().len(), 1, "{ls:#}");
    assert!(Path::new(ls[0]["socket"].as_str().unwrap()).starts_with(short.path()), "{ls:#}");
    assert!(!long.exists(), "the long directory is not used");
}

#[test]
fn a_slow_terminal_does_not_get_a_client_disconnected() {
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let ws = workspace();
    assert!(daemons.scpm(ws.path()).args(["compile", "javalib"]).output().unwrap().status.success());
    let mut src = String::from("package util\n\nobject Noisy:\n");
    for i in 0..3000 {
        src.push_str(&format!("  def f{i}: Int = {{ {i}; {i} + 1 }}\n"));
    }
    fs::write(ws.path().join("modules/util/src/main/scala-3/util/Noisy.scala"), src).unwrap();
    // Nobody reads the output for longer than the daemon's write timeout (`| less`).
    let child = daemons
        .std_scpm(ws.path())
        .args(["compile", "util@jvm-3"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(15));
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{}", &stderr[stderr.len().saturating_sub(2000)..]);
    assert!(stderr.matches("A pure expression does nothing").count() >= 3000, "warnings were lost");
}

#[test]
fn a_compile_during_a_graceful_stop_does_not_disable_the_daemon() {
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let (busy, other) = (workspace(), workspace());
    assert!(daemons.scpm(busy.path()).args(["compile", "javalib"]).output().unwrap().status.success());
    add_slow_sources(busy.path());
    let mut compile = daemons
        .std_scpm(busy.path())
        .args(["compile", "util@jvm-3"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_for(std::time::Duration::from_secs(60), || active_requests(&daemons.ls()) > 0), "never started");
    // A graceful stop waits for the active compile; meanwhile another workspace compiles.
    let mut stop = StdCommand::new(assert_cmd::cargo::cargo_bin("scpm"))
        .args(["daemon", "stop"])
        .env("SCPM_DAEMON_DIR", daemons.path())
        .env("SCPM_DAEMON_FALLBACK_DIR", daemons.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        wait_for(std::time::Duration::from_secs(10), || daemons
            .ls()
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["state"] != "running")),
        "{:#}",
        daemons.ls()
    );
    let out = daemons.scpm(other.path()).args(["compile", "javalib"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let failed =
        |d: &Path| fs::read_dir(d).unwrap().flatten().any(|e| e.path().extension().is_some_and(|x| x == "failed"));
    assert!(!failed(daemons.path()), "a stopping daemon is not a failed start");
    stop.wait().unwrap();
    compile.wait().unwrap();

    // The daemon is used again right away.
    fs::write(
        other.path().join("modules/javalib/src/main/java/javalib/Extra.java"),
        "package javalib;\npublic class Extra {}\n",
    )
    .unwrap();
    let out = daemons.scpm(other.path()).args(["compile", "javalib"]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(daemons.ls()[0]["state"], "running");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("without the shared daemon"));
    assert!(!failed(daemons.path()));
}

#[test]
#[cfg(unix)]
fn a_worker_slower_to_start_than_the_rpc_timeout_still_gets_the_daemon() {
    if !daemon_prerequisites() {
        return;
    }
    // A JDK whose `java` takes 7 s to start the worker (but answers `-version` at once).
    let real = scpm::worker::java().unwrap().path;
    let real = if real.components().count() > 1 {
        real
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap()).map(|d| d.join(&real)).find(|p| p.is_file()).unwrap()
    };
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("bin")).unwrap();
    let java = home.path().join("bin/java");
    fs::write(
        &java,
        format!("#!/bin/sh\ncase \"$1\" in -version) ;; *) sleep 7 ;; esac\nexec '{}' \"$@\"\n", real.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&java, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let daemons = Daemons::new();
    let ws = workspace();
    let out = daemons.scpm(ws.path()).args(["compile", "javalib"]).env("SCPM_JAVA_HOME", home.path()).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("without the shared daemon"), "{stderr}");
    let ls = daemons.ls();
    assert_eq!(ls.as_array().unwrap().len(), 1, "{ls:#}");
    assert_eq!(ls[0]["state"], "running", "{ls:#}");
}

#[cfg(unix)]
#[test]
fn a_java_symlinked_to_a_differently_named_launcher_gets_the_daemon() {
    if !daemon_prerequisites() {
        return;
    }
    // `<home>/bin/java` -> `<custom>/bin/java.real`: there is no `<custom>/bin/java`.
    let real = scpm::worker::java().unwrap().path;
    let real = if real.components().count() > 1 {
        real
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap()).map(|d| d.join(&real)).find(|p| p.is_file()).unwrap()
    };
    let custom = tempfile::tempdir().unwrap();
    fs::create_dir_all(custom.path().join("bin")).unwrap();
    let launcher = custom.path().join("bin/java.real");
    fs::write(&launcher, format!("#!/bin/sh\nexec '{}' \"$@\"\n", real.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("bin")).unwrap();
    std::os::unix::fs::symlink(&launcher, home.path().join("bin/java")).unwrap();
    let daemons = Daemons::new();
    let ws = workspace();
    let out = daemons.scpm(ws.path()).args(["compile", "javalib"]).env("SCPM_JAVA_HOME", home.path()).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("without the shared daemon"), "{stderr}");
    assert_eq!(daemons.ls()[0]["state"], "running");
}

#[test]
#[cfg(unix)]
fn a_non_executable_java_earlier_on_path_is_skipped() {
    if !daemon_prerequisites() {
        return;
    }
    // `java` found on PATH (no JAVA_HOME), behind a non-executable file of that name.
    let real = scpm::worker::java().unwrap().path;
    let real = scpm::worker::find_on_path(&real).unwrap_or(real).canonicalize().unwrap();
    let decoy = tempfile::tempdir().unwrap();
    fs::write(decoy.path().join("java"), "not a program").unwrap();
    let mut dirs = vec![decoy.path().to_path_buf(), real.parent().unwrap().to_path_buf()];
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(dirs).unwrap();
    let daemons = Daemons::new();
    let ws = workspace();
    let out = daemons
        .scpm(ws.path())
        .args(["compile", "javalib"])
        .env("PATH", &path)
        .env_remove("JAVA_HOME")
        .env_remove("SCPM_JAVA_HOME")
        .env_remove("SCPM_JAVA")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("without the shared daemon"), "{stderr}");
    assert_eq!(daemons.ls().as_array().unwrap().len(), 1);
}

#[test]
fn parallel_cold_starts_share_one_daemon() {
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let workspaces: Vec<_> = (0..4).map(|_| workspace()).collect();
    std::thread::scope(|s| {
        for ws in &workspaces {
            let daemons = &daemons;
            s.spawn(move || {
                let out = daemons.scpm(ws.path()).args(["compile", "javalib"]).output().unwrap();
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(out.status.success(), "{stderr}");
                assert!(!stderr.contains("without the shared daemon"), "{stderr}");
            });
        }
    });
    let ls = daemons.ls();
    assert_eq!(ls.as_array().unwrap().len(), 1, "{ls:#}");
    assert_eq!(ls[0]["workspaces"].as_array().unwrap().len(), 4, "{ls:#}");

    // The race's losers exited at once: after a stop, no lingering `serve` replaces it.
    let out = daemons.std_scpm(workspaces[0].path()).args(["daemon", "stop"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert_eq!(daemons.ls(), serde_json::json!([]), "a daemon came back after stop");
}

#[cfg(unix)]
#[test]
fn the_daemon_rejects_an_unsupported_protocol() {
    use std::io::{BufRead, BufReader, Write};
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let ws = workspace();
    let out = daemons.scpm(ws.path()).args(["compile", "javalib"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let ls = daemons.ls();
    let sock = ls[0]["socket"].as_str().unwrap().to_string();
    let mut conn = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    conn.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    for (id, version) in [(1, serde_json::json!(99)), (2, serde_json::Value::Null)] {
        writeln!(
            conn,
            "{}",
            serde_json::json!({"id": id, "method": "initialize", "params": {"protocolVersion": version}})
        )
        .unwrap();
    }
    writeln!(conn, "{}", serde_json::json!({"id": 3, "method": "initialize", "params": {"protocolVersion": 1}}))
        .unwrap();
    let mut reader = BufReader::new(conn);
    let mut replies = Vec::new();
    while replies.len() < 3 {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        if v.get("id").is_some() {
            replies.push(v);
        }
    }
    assert_eq!(replies[0]["error"]["code"], "unsupported_protocol", "{replies:#?}");
    assert_eq!(replies[1]["error"]["code"], "unsupported_protocol", "{replies:#?}");
    assert_eq!(replies[2]["result"]["protocolVersion"], 1, "{replies:#?}");
}

#[test]
fn an_idle_daemon_exits() {
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let ws = workspace();
    let out = daemons.scpm(ws.path()).args(["compile", "javalib"]).env("SCPM_DAEMON_IDLE_SECS", "1").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(wait_for(std::time::Duration::from_secs(15), || daemons.ls() == serde_json::json!([])), "still running");
    let sockets =
        fs::read_dir(daemons.path()).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "sock"));
    assert_eq!(sockets.count(), 0);
}

#[test]
fn daemon_stop_terminates_a_daemon_that_does_not_answer() {
    if !daemon_prerequisites() {
        return;
    }
    let daemons = Daemons::new();
    let ws = workspace();
    assert!(daemons.scpm(ws.path()).args(["compile", "javalib"]).output().unwrap().status.success());
    let ls = daemons.ls();
    let (pid, worker) = (ls[0]["pid"].as_u64().unwrap(), ls[0]["workerPid"].as_u64().unwrap());
    assert!(StdCommand::new("kill").args(["-STOP", &pid.to_string()]).status().unwrap().success());
    let started = std::time::Instant::now();
    let out = Command::cargo_bin("scpm")
        .unwrap()
        .args(["daemon", "stop"])
        .env("SCPM_DAEMON_DIR", daemons.path())
        .env("SCPM_DAEMON_FALLBACK_DIR", daemons.path())
        .timeout(std::time::Duration::from_secs(60))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "stopped 1 daemon", "{stderr}");
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "took {:?}", started.elapsed());
    assert!(wait_for(std::time::Duration::from_secs(5), || !alive(pid) && !alive(worker)), "daemon or worker survived");
    assert_eq!(daemons.ls(), serde_json::json!([]));

    // A command afterwards starts a fresh daemon (a changed source, so it compiles).
    fs::write(
        ws.path().join("modules/javalib/src/main/java/javalib/Extra.java"),
        "package javalib;\npublic class Extra {}\n",
    )
    .unwrap();
    let out = daemons.scpm(ws.path()).args(["compile", "javalib"]).output().unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("without the shared daemon"));
    let ls = daemons.ls();
    assert_eq!(ls.as_array().unwrap().len(), 1, "{ls:#}");
    assert_ne!(ls[0]["pid"], pid, "{ls:#}");
    assert!(ls[0]["requests"].as_u64().unwrap() >= 1, "{ls:#}");
}

#[cfg(unix)]
#[test]
fn switching_scpm_java_invalidates_the_fingerprint() {
    if !prerequisites() {
        return;
    }
    let real = scpm::worker::java().unwrap().path;
    let real = if real.components().count() > 1 {
        real
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap()).map(|d| d.join(&real)).find(|p| p.is_file()).unwrap()
    };
    // Two launchers for the same JDK at different paths: different JDK identities.
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let launchers: Vec<PathBuf> = dirs
        .iter()
        .map(|d| {
            let l = d.path().join("java");
            fs::write(&l, format!("#!/bin/sh\nexec '{}' \"$@\"\n", real.display())).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&l, fs::Permissions::from_mode(0o755)).unwrap();
            l
        })
        .collect();
    let ws = workspace();
    let compile = |java: &Path| -> Vec<(String, String)> {
        let out = cold_scpm(ws.path()).args(["--json", "compile", "javalib"]).env("SCPM_JAVA", java).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        statuses(&serde_json::from_slice(&out.stdout).unwrap())
    };
    compile(&launchers[0]);
    assert!(compile(&launchers[0]).iter().all(|(_, s)| s == "upToDate"));
    let after_switch = compile(&launchers[1]);
    assert!(after_switch.iter().any(|(_, s)| s != "upToDate"), "{after_switch:?}");

    // A bare `SCPM_JAVA=java` is identified by what PATH resolves it to.
    let path = std::env::var_os("PATH").unwrap();
    let bare = |dir: &Path| -> Vec<(String, String)> {
        let mut dirs = vec![dir.to_path_buf()];
        dirs.extend(std::env::split_paths(&path));
        let out = cold_scpm(ws.path())
            .args(["--json", "compile", "javalib"])
            .env("SCPM_JAVA", "java")
            .env("PATH", std::env::join_paths(dirs).unwrap())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        statuses(&serde_json::from_slice(&out.stdout).unwrap())
    };
    bare(dirs[0].path());
    assert!(bare(dirs[0].path()).iter().all(|(_, s)| s == "upToDate"));
    let after_path_switch = bare(dirs[1].path());
    assert!(after_path_switch.iter().any(|(_, s)| s != "upToDate"), "{after_path_switch:?}");
}
