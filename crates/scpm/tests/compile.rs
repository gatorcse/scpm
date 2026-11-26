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
