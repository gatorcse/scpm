//! `scpm compile` end to end: needs `cs`, a JDK, network access, and the worker jar built by
//! `sbt package` in worker/ (or SCPM_WORKER_JAR). Skipped with a message otherwise.

use std::fs;
use std::path::{Path, PathBuf};

use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value;

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
        .timeout(std::time::Duration::from_secs(300));
    cmd
}

fn compile_json(dir: &Path, args: &[&str]) -> (bool, Value) {
    let out = scpm(dir).args(["--json", "compile"]).args(args).output().unwrap();
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
    let ws = workspace();
    let (ok, doc) = compile_json(ws.path(), &["--tests"]);
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
    let (ok, doc) = compile_json(ws.path(), &["--tests"]);
    assert!(ok);
    assert!(statuses(&doc).iter().all(|(_, s)| s == "upToDate"), "{doc:#}");
    assert_eq!(fs::metadata(&log).map(|m| m.len()).unwrap_or(0), before, "the worker must not start");

    // Changing a Scala 3-only source recompiles util@jvm-3 (and checks app) but not 2.13.
    let platform = ws.path().join("modules/util/src/main/scala-3/util/Platform.scala");
    fs::write(&platform, "package util\n\nobject Platform:\n  def name: String = \"scala 3!\"\n").unwrap();
    let (ok, doc) = compile_json(ws.path(), &[]);
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
    let out = scpm(ws.path())
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
