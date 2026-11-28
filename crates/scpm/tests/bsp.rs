//! `scpm bsp` end to end: a client speaking real `Content-Length`-framed JSON-RPC over the
//! server's stdio, against copies of the fixtures. Tests that resolve or compile need `cs`, a
//! JDK and the worker jars (`sbt package` in worker/), and are skipped with a message otherwise.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as StdCommand, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod shared_daemon;

/// Which compile daemon a test's server uses.
#[derive(Clone, Copy)]
enum Daemon<'a> {
    /// The warm daemon shared by the tests (see `shared_daemon`).
    Shared,
    /// The test's own daemon directory.
    Own(&'a Path),
    /// None: the server starts its own cold worker.
    Off,
}

fn prerequisites() -> bool {
    let cs = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    let has_cs = StdCommand::new(cs).arg("version").output().is_ok_and(|o| o.status.success());
    let has_worker = scpm::worker::helper_jar("SCPM_WORKER_JAR", "scpm-compile-worker").is_ok()
        && scpm::worker::helper_jar("SCPM_TEST_RUNNER_JAR", "scpm-test-runner").is_ok();
    let has_java = scpm::worker::java().is_ok();
    if !(has_cs && has_worker && has_java) {
        eprintln!("skipping: needs `cs`, a JDK and the worker jars (run `sbt package` in worker/)");
    }
    has_cs && has_worker && has_java
}

fn workspace(fixture: &str) -> tempfile::TempDir {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture);
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

/// How long one response may take (a cold resolve or compile included).
const RESPONSE_LIMIT: Duration = Duration::from_secs(300);

/// A BSP client driving `scpm bsp` in a workspace.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
    /// Notifications received so far (in order), drained by `take_notifications`.
    notifications: Vec<Value>,
    stderr: PathBuf,
    next_id: u64,
    root: PathBuf,
}

impl Client {
    /// Starts the server with the workspace as working directory (as BSP clients do) and
    /// initializes it.
    fn start(ws: &Path, daemons: Daemon) -> Client {
        Client::start_as(ws, daemons, "scpm-test")
    }

    /// [`Client::start`] with the client's `displayName` (Metals gets a SemanticDB hint).
    fn start_as(ws: &Path, daemons: Daemon, name: &str) -> Client {
        let root = ws.canonicalize().unwrap();
        let stderr = root.join(".bsp-test-stderr.log");
        let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin("scpm"));
        cmd.arg("bsp")
            .current_dir(&root)
            .env("COURSIER_CACHE", Path::new(env!("CARGO_TARGET_TMPDIR")).join("coursier-cache"))
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&stderr).unwrap());
        if let Ok(jar) = scpm::worker::helper_jar("SCPM_WORKER_JAR", "scpm-compile-worker") {
            cmd.env("SCPM_WORKER_JAR", jar);
        }
        match daemons {
            Daemon::Shared => cmd.envs(shared_daemon::env()),
            Daemon::Own(dir) => {
                cmd.env_remove("SCPM_NO_DAEMON")
                    .env("SCPM_DAEMON_DIR", dir)
                    // Never the user's own `/tmp/scpm-<uid>` when the temp path is too long for sockets.
                    .env("SCPM_DAEMON_FALLBACK_DIR", dir)
                    .env("SCPM_DAEMON_IDLE_SECS", "300")
            }
            Daemon::Off => cmd.env("SCPM_NO_DAEMON", "1"),
        };
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = channel();
        // Every byte on stdout must be a framed JSON-RPC message.
        std::thread::spawn(move || {
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if stdout.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap_or_else(|| panic!("not a header: {line:?}"));
                    assert_eq!(name, "Content-Length", "unexpected header {line:?}");
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
                let mut body = vec![0; length.expect("Content-Length")];
                stdout.read_exact(&mut body).unwrap();
                let msg: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(msg["jsonrpc"], "2.0", "{msg}");
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });
        let mut client = Client { child, stdin, rx, notifications: Vec::new(), stderr, next_id: 0, root };
        let root_uri = format!("file://{}/", client.root.display());
        let init = client.request(
            "build/initialize",
            json!({"displayName": name, "version": "1", "bspVersion": "2.1.0", "rootUri": root_uri,
                   "capabilities": {"languageIds": ["scala", "java"]}}),
        );
        assert_eq!(init["result"]["displayName"], "scpm", "{init}");
        client.notify("build/initialized", json!({}));
        client
    }

    fn send(&mut self, msg: &Value) {
        let body = msg.to_string();
        let stdin = self.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Sends a request without waiting; returns its id.
    fn send_request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Waits for the response to `id`, keeping notifications.
    fn response(&mut self, id: u64) -> Value {
        let started = Instant::now();
        loop {
            let left = RESPONSE_LIMIT.saturating_sub(started.elapsed());
            match self.rx.recv_timeout(left) {
                Ok(msg) if msg["id"] == id && msg.get("method").is_none() => return msg,
                Ok(msg) if msg.get("method").is_some() => self.notifications.push(msg),
                Ok(other) => panic!("unexpected message {other}"),
                Err(RecvTimeoutError::Timeout) => panic!("no response to {id}; stderr:\n{}", self.stderr_text()),
                Err(RecvTimeoutError::Disconnected) => panic!("server exited; stderr:\n{}", self.stderr_text()),
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        self.response(id)
    }

    /// The result of a request that must succeed.
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.request(method, params);
        assert!(r.get("error").is_none(), "{method}: {r}\nstderr:\n{}", self.stderr_text());
        r["result"].clone()
    }

    /// Waits until a notification matching `pred` arrives (or was already received).
    fn wait_notification(&mut self, limit: Duration, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(n) = self.notifications.iter().find(|n| pred(n)) {
            return n.clone();
        }
        let started = Instant::now();
        loop {
            let left = limit.saturating_sub(started.elapsed());
            match self.rx.recv_timeout(left) {
                Ok(msg) => {
                    let hit = pred(&msg);
                    self.notifications.push(msg.clone());
                    if hit {
                        return msg;
                    }
                }
                Err(_) => panic!("notification never came; stderr:\n{}", self.stderr_text()),
            }
        }
    }

    fn take_notifications(&mut self) -> Vec<Value> {
        while let Ok(msg) = self.rx.try_recv() {
            self.notifications.push(msg);
        }
        std::mem::take(&mut self.notifications)
    }

    fn stderr_text(&self) -> String {
        fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    fn target(&self, id: &str) -> Value {
        let dir = match id.split_once('@').map(|(m, _)| m) {
            Some("javadeps") => self.root.clone(),
            Some(m) => self.root.join("modules").join(m),
            None => unreachable!(),
        };
        json!({"uri": format!("{}?id={id}", scpm::ide::file_uri(&dir) + "/")})
    }

    fn uri(&self, rel: &str) -> String {
        scpm::ide::file_uri(&self.root.join(rel))
    }

    /// `build/shutdown` then `build/exit`; the server must exit 0.
    fn shutdown(mut self) {
        assert_eq!(self.ok("build/shutdown", Value::Null), Value::Null);
        self.notify("build/exit", Value::Null);
        drop(self.stdin.take());
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "exit code {status:?}; stderr:\n{}", self.stderr_text());
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "the server did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Stops a test's private daemons even if an assertion fails.
struct DaemonGuard(tempfile::TempDir);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = StdCommand::new(assert_cmd::cargo::cargo_bin("scpm"))
            .args(["daemon", "stop", "--force"])
            .env("SCPM_DAEMON_DIR", self.0.path())
            .env("SCPM_DAEMON_FALLBACK_DIR", self.0.path())
            .output();
    }
}

fn items(result: &Value) -> &Vec<Value> {
    result["items"].as_array().unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

#[test]
fn lifecycle_framing_and_setup_without_resolution() {
    // No `cs` needed: nothing here loads the workspace.
    let ws = workspace("app");
    let mut c = Client::start(ws.path(), Daemon::Shared);
    let caps = c.ok("build/initialize", json!({"displayName": "again", "version": "1", "bspVersion": "2.1.0", "rootUri": "file:///", "capabilities": {}}))["capabilities"].clone();
    for key in [
        "jvmRunEnvironmentProvider",
        "jvmTestEnvironmentProvider",
        "dependencySourcesProvider",
        "resourcesProvider",
        "outputPathsProvider",
        "canReload",
    ] {
        assert_eq!(caps[key], true, "{key}: {caps}");
    }
    assert_eq!(caps["debugProvider"]["languageIds"], json!([]));
    let unknown = c.request("buildTarget/nope", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    c.shutdown();

    // `--setup` writes the connection file once, pointing at this binary.
    let bin = assert_cmd::cargo::cargo_bin("scpm");
    let setup = || StdCommand::new(&bin).args(["bsp", "--setup"]).current_dir(ws.path()).output().unwrap();
    let out = setup();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("Wrote .bsp/scpm.json"));
    let conn: Value = serde_json::from_str(&fs::read_to_string(ws.path().join(".bsp/scpm.json")).unwrap()).unwrap();
    assert_eq!(conn["name"], "scpm");
    assert_eq!(conn["argv"][1], "bsp");
    assert_eq!(Path::new(conn["argv"][0].as_str().unwrap()), bin.canonicalize().unwrap());
    assert!(String::from_utf8_lossy(&setup().stdout).starts_with("Up to date"));
    fs::write(ws.path().join(".bsp/scpm.json"), "{}").unwrap();
    assert!(!setup().status.success(), "someone else's file is left alone");
}

#[test]
fn targets_options_and_environments() {
    if !prerequisites() {
        return;
    }
    let ws = workspace("app");
    let mut c = Client::start(ws.path(), Daemon::Shared);
    let targets = c.ok("workspace/buildTargets", json!({}))["targets"].as_array().unwrap().clone();
    let find = |name: &str| {
        targets.iter().find(|t| t["displayName"] == name).unwrap_or_else(|| panic!("{name}: {targets:#?}")).clone()
    };
    let names: Vec<&str> = targets.iter().map(|t| t["displayName"].as_str().unwrap()).collect();
    assert_eq!(names, ["app@jvm-3", "app@jvm-3:test", "javalib@jvm", "util@jvm-3", "util@jvm-2.13"], "{targets:#?}");

    let app = find("app@jvm-3");
    assert_eq!(app["id"], c.target("app@jvm-3"));
    assert_eq!(app["baseDirectory"], format!("{}/", c.uri("modules/app")));
    assert_eq!(app["tags"], json!(["application"]));
    assert_eq!(app["capabilities"], json!({"canCompile": true, "canTest": false, "canRun": true, "canDebug": false}));
    assert_eq!(app["dataKind"], "scala");
    assert_eq!(app["data"]["scalaVersion"], "3.9.0");
    assert_eq!(app["data"]["scalaBinaryVersion"], "3");
    assert!(strings(&app["data"]["jars"]).iter().any(|j| j.contains("scala3-compiler_3-3.9.0.jar")), "{app:#}");
    assert!(app["data"]["jvmBuildTarget"]["javaHome"].as_str().unwrap().starts_with("file:///"));
    let deps = app["dependencies"].as_array().unwrap();
    assert!(deps.contains(&c.target("util@jvm-3")) && deps.contains(&c.target("javalib@jvm")), "{deps:?}");

    let app_test = find("app@jvm-3:test");
    assert_eq!(app_test["tags"], json!(["test"]));
    assert_eq!(app_test["baseDirectory"], app["baseDirectory"], "IntelliJ merges them into one module");
    assert_eq!(app_test["dependencies"][0], c.target("app@jvm-3"));
    assert_eq!(app_test["capabilities"]["canTest"], true);

    let javalib = find("javalib@jvm");
    assert_eq!((javalib["dataKind"].as_str(), &javalib["languageIds"]), (Some("jvm"), &json!(["java"])));
    // Cross-built: the primary variant (the first Scala version) is the IDE's.
    assert_eq!(find("util@jvm-3")["tags"], json!(["library"]));
    assert_eq!(find("util@jvm-2.13")["tags"], json!(["library", "no-ide"]));
    assert_eq!(find("util@jvm-2.13")["data"]["scalaVersion"], "2.13.18");

    let (t_app, t_test, t_javalib, t_util) =
        (c.target("app@jvm-3"), c.target("app@jvm-3:test"), c.target("javalib@jvm"), c.target("util@jvm-3"));
    let sources = c.ok("buildTarget/sources", json!({"targets": [t_app, t_test]}));
    assert_eq!(
        items(&sources)[0]["sources"][0],
        json!({"uri": format!("{}/", c.uri("modules/app/src/main/scala")), "kind": 2, "generated": false})
    );
    assert_eq!(items(&sources)[1]["sources"][0]["uri"], format!("{}/", c.uri("modules/app/src/test/scala")));
    let util_sources = c.ok("buildTarget/sources", json!({"targets": [t_util]}));
    let util_dirs: Vec<&str> =
        items(&util_sources)[0]["sources"].as_array().unwrap().iter().map(|s| s["uri"].as_str().unwrap()).collect();
    assert!(util_dirs.iter().any(|u| u.ends_with("/src/main/scala-3/")), "{util_dirs:?}");
    assert!(!util_dirs.iter().any(|u| u.ends_with("/src/main/scala-2.13/")), "{util_dirs:?}");
    let resources = c.ok("buildTarget/resources", json!({"targets": [t_app]}));
    assert_eq!(items(&resources)[0]["resources"][0], format!("{}/", c.uri("modules/app/src/main/resources")));
    let inverse = c.ok(
        "buildTarget/inverseSources",
        json!({"textDocument": {"uri": c.uri("modules/app/src/main/scala/app/Main.scala")}}),
    );
    assert_eq!(inverse["targets"], json!([t_app]));
    let outputs = c.ok("buildTarget/outputPaths", json!({"targets": [t_app]}));
    assert_eq!(items(&outputs)[0]["outputPaths"][0]["uri"], format!("{}/", c.uri(".scpm/out/app@jvm-3")));

    // scalacOptions: what `scpm compile` uses; Java-only targets have none.
    let scalac = c.ok("buildTarget/scalacOptions", json!({"targets": [t_app, t_test, t_javalib]}));
    assert_eq!(items(&scalac).len(), 2, "{scalac:#}");
    let main = &items(&scalac)[0];
    assert_eq!(main["classDirectory"], format!("{}/", c.uri(".scpm/out/app@jvm-3/classes")));
    let cp = strings(&main["classpath"]);
    assert!(cp.contains(&format!("{}/", c.uri(".scpm/out/util@jvm-3/classes"))), "{cp:?}");
    assert!(cp.iter().any(|e| e.ends_with(".jar") && e.contains("cats-core_3")), "{cp:?}");
    assert!(!cp.iter().any(|e| e.contains("munit")), "test dependencies stay off the main classpath");
    let test = &items(&scalac)[1];
    assert_eq!(test["classDirectory"], format!("{}/", c.uri(".scpm/out/app@jvm-3/test-classes")));
    let test_cp = strings(&test["classpath"]);
    assert_eq!(test_cp[0], format!("{}/", c.uri(".scpm/out/app@jvm-3/classes")));
    assert!(test_cp.iter().any(|e| e.contains("munit_3")), "{test_cp:?}");
    let javac = c.ok("buildTarget/javacOptions", json!({"targets": [t_javalib]}));
    assert_eq!(items(&javac)[0]["classDirectory"], format!("{}/", c.uri(".scpm/out/javalib@jvm/classes")));

    let modules = c.ok("buildTarget/dependencyModules", json!({"targets": [t_util]}));
    let cats = items(&modules)[0]["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "org.typelevel:cats-core_3")
        .cloned();
    let cats = cats.unwrap_or_else(|| panic!("{modules:#}"));
    assert_eq!(cats["version"], "2.13.0");
    assert_eq!(cats["dataKind"], "maven");
    assert!(cats["data"]["artifacts"][0]["uri"].as_str().unwrap().ends_with("cats-core_3-2.13.0.jar"));
    // Sources jars may be unavailable (offline, rate-limited): only the shape is checked.
    let dep_sources = c.ok("buildTarget/dependencySources", json!({"targets": [t_util]}));
    assert!(items(&dep_sources)[0]["sources"].is_array(), "{dep_sources:#}");

    // Run and test environments: runtime classpaths, before anything is compiled.
    let run_env = c.ok("buildTarget/jvmRunEnvironment", json!({"targets": [t_app]}));
    let env = &items(&run_env)[0];
    let cp = strings(&env["classpath"]);
    assert_eq!(cp[0], format!("{}/", c.uri(".scpm/out/app@jvm-3/classes")));
    assert!(cp.contains(&format!("{}/", c.uri(".scpm/out/javalib@jvm/classes"))), "{cp:?}");
    assert_eq!(env["workingDirectory"], scpm::ide::file_uri(&c.root));
    assert_eq!(env["mainClasses"][0]["className"], "app.Main");
    let test_env = c.ok("buildTarget/jvmTestEnvironment", json!({"targets": [t_test]}));
    let env = &items(&test_env)[0];
    let cp = strings(&env["classpath"]);
    assert_eq!(cp[0], format!("{}/", c.uri(".scpm/out/app@jvm-3/test-classes")));
    assert!(cp.iter().any(|e| e.contains("munit_3")), "{cp:?}");
    assert_eq!(env["workingDirectory"], c.uri("modules/app"), "tests run in the module directory");

    // Compile the tests: compile-task / compile-report notifications, then discovery.
    let compiled = c.ok("buildTarget/compile", json!({"targets": [t_test], "originId": "o1"}));
    assert_eq!(compiled, json!({"originId": "o1", "statusCode": 1}));
    let notes = c.take_notifications();
    let report = notes
        .iter()
        .find(|n| n["method"] == "build/taskFinish" && n["params"]["data"]["target"] == t_test)
        .unwrap_or_else(|| panic!("{notes:#?}"));
    assert_eq!(report["params"]["dataKind"], "compile-report");
    assert_eq!(report["params"]["data"]["errors"], 0);
    assert_eq!(report["params"]["originId"], "o1");
    assert!(notes.iter().any(|n| n["method"] == "build/taskStart" && n["params"]["dataKind"] == "compile-task"));
    assert!(notes.iter().all(|n| n["method"] != "build/taskStart" || n["params"]["eventTime"].is_u64()));
    let mains = c.ok("buildTarget/scalaMainClasses", json!({"targets": [t_app]}));
    assert_eq!(items(&mains)[0]["classes"][0]["class"], "app.Main", "{mains:#}");
    let tests = c.ok("buildTarget/scalaTestClasses", json!({"targets": [t_app, t_test]}));
    assert_eq!(tests["items"], json!([{"target": t_test, "framework": "munit", "classes": ["app.MainSuite"]}]));

    // A config change is noticed on the next request: buildTarget/didChange.
    let app_yaml = ws.path().join("modules/app/scpm.yaml");
    fs::write(&app_yaml, fs::read_to_string(&app_yaml).unwrap() + "scalacOptions: [-deprecation]\n").unwrap();
    let scalac = c.ok("buildTarget/scalacOptions", json!({"targets": [t_app]}));
    assert!(strings(&items(&scalac)[0]["options"]).contains(&"-deprecation".to_string()), "{scalac:#}");
    let changed = c.wait_notification(Duration::from_secs(5), |n| n["method"] == "buildTarget/didChange");
    let changes = changed["params"]["changes"].as_array().unwrap();
    assert!(changes.contains(&json!({"target": t_app, "kind": 2})), "{changes:?}");
    assert!(!changes.iter().any(|ch| ch["target"] == t_javalib), "{changes:?}");
    // An explicit reload with nothing changed sends no didChange (IntelliJ re-imports on it).
    c.take_notifications();
    assert_eq!(c.ok("workspace/reload", json!({})), Value::Null);
    assert!(!c.take_notifications().iter().any(|n| n["method"] == "buildTarget/didChange"));

    // A version-specific source directory created mid-session is picked up. First let the
    // config edits above age past the reload stamp's 1 s guard, and settle the model.
    std::thread::sleep(Duration::from_millis(1500));
    c.ok("workspace/buildTargets", json!({}));
    c.ok("workspace/buildTargets", json!({}));
    c.take_notifications();
    let scala3 = ws.path().join("modules/app/src/main/scala-3");
    fs::create_dir_all(&scala3).unwrap();
    let sources = c.ok("buildTarget/sources", json!({"targets": [t_app]}));
    let uris: Vec<String> = items(&sources)[0]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(uris.iter().any(|u| u.trim_end_matches('/').ends_with("src/main/scala-3")), "{uris:?}");
    fs::remove_dir_all(&scala3).unwrap();
    c.ok("workspace/reload", json!({}));
    c.take_notifications();

    // A workspace-level setting that only changes compiler options still notifies.
    let ws_yaml = ws.path().join("scpm-workspace.yaml");
    fs::write(&ws_yaml, fs::read_to_string(&ws_yaml).unwrap() + "ide:\n  semanticdb: true\n").unwrap();
    let scalac = c.ok("buildTarget/scalacOptions", json!({"targets": [t_app]}));
    assert!(strings(&items(&scalac)[0]["options"]).contains(&"-Xsemanticdb".to_string()), "{scalac:#}");
    let changed = c.wait_notification(Duration::from_secs(5), |n| n["method"] == "buildTarget/didChange");
    let changes = changed["params"]["changes"].as_array().unwrap();
    assert!(changes.contains(&json!({"target": t_app, "kind": 2})), "{changes:?}");
    fs::write(&ws_yaml, fs::read_to_string(&ws_yaml).unwrap().replace("ide:\n  semanticdb: true\n", "")).unwrap();
    c.ok("workspace/reload", json!({}));
    c.take_notifications();

    // Every target in one request: each task starts and finishes exactly once (two compile
    // passes run, mains after tests, and the second sees the first's targets up to date).
    c.take_notifications();
    let r = c.ok("buildTarget/compile", json!({"targets": [t_app, t_test, t_javalib, t_util]}));
    assert_eq!(r["statusCode"], 1);
    let notes = c.take_notifications();
    let ids = |method: &str| -> Vec<String> {
        let mut ids: Vec<String> = notes
            .iter()
            .filter(|n| n["method"] == method && n["params"]["taskId"]["parents"].is_array())
            .map(|n| n["params"]["taskId"]["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    };
    let (starts, finishes) = (ids("build/taskStart"), ids("build/taskFinish"));
    assert_eq!(starts, finishes, "{notes:#?}");
    let mut deduped = finishes.clone();
    deduped.dedup();
    assert_eq!(deduped, finishes, "a task finished twice: {notes:#?}");
    assert_eq!(finishes.len(), 4, "{notes:#?}");

    let cleaned = c.ok("buildTarget/cleanCache", json!({"targets": [t_test]}));
    assert_eq!(cleaned["cleaned"], true);
    assert!(!ws.path().join(".scpm/out/app@jvm-3/test-classes").exists());
    assert!(ws.path().join(".scpm/out/app@jvm-3/classes").exists());
    let unknown = c.request("buildTarget/compile", json!({"targets": [{"uri": "file:///nope?id=x"}]}));
    assert_eq!(unknown["error"]["code"], -32602);
    c.shutdown();
}

#[test]
fn compile_publishes_and_clears_diagnostics_through_the_daemon() {
    if !prerequisites() {
        return;
    }
    let ws = workspace("app");
    let daemons = DaemonGuard(tempfile::tempdir().unwrap());
    let broken = ws.path().join("modules/util/src/main/scala-3/util/Broken.scala");
    fs::write(&broken, "package util\n\nval broken: Int = \"nope\"\n").unwrap();
    let mut c = Client::start(ws.path(), Daemon::Own(daemons.0.path()));
    let (t_app, t_util) = (c.target("app@jvm-3"), c.target("util@jvm-3"));
    let file = c.uri("modules/util/src/main/scala-3/util/Broken.scala");

    let r = c.ok("buildTarget/compile", json!({"targets": [t_app], "originId": "c1"}));
    assert_eq!(r["statusCode"], 2, "{r}");
    let notes = c.take_notifications();
    let published: Vec<&Value> = notes.iter().filter(|n| n["method"] == "build/publishDiagnostics").collect();
    assert_eq!(published.len(), 1, "{published:#?}");
    let p = &published[0]["params"];
    assert_eq!(p["textDocument"]["uri"], file);
    assert_eq!(p["buildTarget"], t_util);
    assert_eq!(p["reset"], true);
    assert_eq!(p["originId"], "c1");
    let d = &p["diagnostics"][0];
    assert_eq!(d["severity"], 1);
    assert_eq!(d["code"], "E007");
    assert_eq!(d["range"]["start"], json!({"line": 2, "character": 18}), "0-based");
    let util_report = notes
        .iter()
        .find(|n| n["params"]["dataKind"] == "compile-report" && n["params"]["data"]["target"] == t_util)
        .unwrap();
    assert_eq!(
        (util_report["params"]["status"].as_u64(), util_report["params"]["data"]["errors"].as_u64()),
        (Some(2), Some(1))
    );
    let app_report = notes
        .iter()
        .find(|n| n["params"]["dataKind"] == "compile-report" && n["params"]["data"]["target"] == t_app)
        .unwrap();
    assert_eq!(app_report["params"]["status"], 2, "app is skipped after util failed");
    assert!(!c.stderr_text().contains("without the shared daemon"), "{}", c.stderr_text());

    // Fixed: the file's diagnostics are cleared with an empty reset.
    fs::write(&broken, "package util\n\nval broken: Int = 1\n").unwrap();
    let r = c.ok("buildTarget/compile", json!({"targets": [t_app], "originId": "c2"}));
    assert_eq!(r["statusCode"], 1, "{r}\n{}", c.stderr_text());
    let notes = c.take_notifications();
    let cleared: Vec<&Value> = notes.iter().filter(|n| n["method"] == "build/publishDiagnostics").collect();
    assert_eq!(cleared.len(), 1, "{cleared:#?}");
    assert_eq!(cleared[0]["params"]["textDocument"]["uri"], file);
    assert_eq!(cleared[0]["params"]["diagnostics"], json!([]));
    assert_eq!(cleared[0]["params"]["reset"], true);

    // Unchanged: up to date (noOp), nothing republished.
    let r = c.ok("buildTarget/compile", json!({"targets": [t_app]}));
    assert_eq!(r["statusCode"], 1);
    let notes = c.take_notifications();
    assert!(!notes.iter().any(|n| n["method"] == "build/publishDiagnostics"), "{notes:#?}");
    assert!(
        notes
            .iter()
            .filter(|n| n["params"]["dataKind"] == "compile-report")
            .all(|n| n["params"]["data"]["noOp"] == true)
    );
    c.shutdown();
}

#[test]
fn runs_tests_and_programs_and_cancels_them() {
    if !prerequisites() {
        return;
    }
    let ws = workspace("app");
    let tests_dir = ws.path().join("modules/app/src/test/scala/app");
    fs::write(
        tests_dir.join("FailSuite.scala"),
        "package app\n\nclass FailSuite extends munit.FunSuite:\n  test(\"fails\") {\n    println(\"from a test\")\n    assertEquals(1 + 1, 3)\n  }\n",
    )
    .unwrap();
    let main_dir = ws.path().join("modules/app/src/main/scala/app");
    fs::write(main_dir.join("Exit.scala"), "package app\n\n@main def Exit(): Unit = sys.exit(7)\n").unwrap();
    fs::write(
        main_dir.join("Bytes.scala"),
        "package app\n\n@main def Bytes(): Unit =\n  System.out.write(Array[Byte](0xff.toByte, 0xfe.toByte, 10))\n  System.out.flush()\n  println(\"after bad bytes\")\n",
    )
    .unwrap();
    // Leaves a background process (another JVM, so it works everywhere) holding its stdout and
    // stderr open.
    fs::write(
        main_dir.join("Spawn.scala"),
        "package app\n\n@main def Spawn(): Unit =\n  val java = ProcessHandle.current().info().command().get()\n  new ProcessBuilder(java, \"-cp\", System.getProperty(\"java.class.path\"), \"app.Linger\").inheritIO().start()\n  println(\"spawned\")\n",
    )
    .unwrap();
    fs::write(main_dir.join("Linger.scala"), "package app\n\n@main def Linger(): Unit = Thread.sleep(600000)\n")
        .unwrap();
    fs::write(
        main_dir.join("Sleep.scala"),
        "package app\n\n@main def Sleep(): Unit =\n  println(\"sleeping\")\n  Thread.sleep(600000)\n",
    )
    .unwrap();
    let mut c = Client::start(ws.path(), Daemon::Shared);
    let (t_app, t_test) = (c.target("app@jvm-3"), c.target("app@jvm-3:test"));

    // A stale selection (its target is gone) runs nothing rather than every suite.
    let stale = c.request(
        "buildTarget/test",
        json!({"targets": [t_test], "dataKind": "scala-test",
               "data": {"testClasses": [{"target": {"uri": "file:///gone/?id=gone@jvm-3:test"}, "classes": ["app.MainSuite"]}]}}),
    );
    assert_eq!(stale["error"]["code"], -32602, "{stale}");

    // Only MainSuite (IntelliJ's BSP test runner sends `scala-test` with explicit classes).
    let r = c.ok(
        "buildTarget/test",
        json!({"targets": [t_test], "originId": "t1", "dataKind": "scala-test",
               "data": {"testClasses": [{"target": t_test, "classes": ["app.MainSuite"]}]}}),
    );
    assert_eq!(r, json!({"originId": "t1", "statusCode": 1}));
    let notes = c.take_notifications();
    let report =
        notes.iter().find(|n| n["params"]["dataKind"] == "test-report").unwrap_or_else(|| panic!("{notes:#?}"));
    assert_eq!(report["params"]["data"]["passed"], 2);
    assert_eq!(report["params"]["data"]["failed"], 0);
    assert_eq!(report["params"]["data"]["target"], t_test);
    assert_eq!(notes.iter().filter(|n| n["params"]["dataKind"] == "test-finish").count(), 2);

    // Cases chosen per suite: one runner request each, and each run gets its own report.
    let r = c.ok(
        "buildTarget/test",
        json!({"targets": [t_test], "originId": "t3", "dataKind": "scala-test-suites-selection",
               "data": {"suites": [{"className": "app.MainSuite", "tests": ["greets"]},
                                   {"className": "app.FailSuite", "tests": ["fails"]}]}}),
    );
    assert_eq!(r["statusCode"], 2);
    let notes = c.take_notifications();
    let reports: Vec<&Value> = notes.iter().filter(|n| n["params"]["dataKind"] == "test-report").collect();
    assert_eq!(reports.len(), 2, "{notes:#?}");
    let sum = |k: &str| reports.iter().map(|r| r["params"]["data"][k].as_u64().unwrap_or(0)).sum::<u64>();
    assert_eq!((sum("passed"), sum("failed")), (1, 1), "{reports:#?}");
    // Each run keeps its own JUnit report, so FailSuite's failure isn't overwritten.
    let reports_dir = ws.path().join(".scpm/out/app@jvm-3/test-reports");
    let junit: Vec<String> = ["junit.xml", "junit-2.xml"]
        .iter()
        .map(|f| fs::read_to_string(reports_dir.join(f)).unwrap_or_else(|e| panic!("{f}: {e}")))
        .collect();
    assert!(junit.iter().any(|x| x.contains("<failure")), "{junit:#?}");

    // Everything: FailSuite fails, with its output as log messages and a location.
    let r = c.ok("buildTarget/test", json!({"targets": [t_test], "originId": "t2"}));
    assert_eq!(r["statusCode"], 2);
    let notes = c.take_notifications();
    let report = notes.iter().find(|n| n["params"]["dataKind"] == "test-report").unwrap();
    assert_eq!(
        (report["params"]["data"]["passed"].as_u64(), report["params"]["data"]["failed"].as_u64()),
        (Some(2), Some(1))
    );
    let failed = notes
        .iter()
        .find(|n| n["params"]["dataKind"] == "test-finish" && n["params"]["data"]["status"] == 2)
        .unwrap_or_else(|| panic!("{notes:#?}"));
    assert_eq!(failed["params"]["data"]["displayName"], "app.FailSuite.fails");
    assert_eq!(failed["params"]["data"]["location"]["uri"], c.uri("modules/app/src/test/scala/app/FailSuite.scala"));
    assert_eq!(failed["params"]["data"]["location"]["range"]["start"]["line"], 5);
    assert!(
        notes
            .iter()
            .any(|n| n["method"] == "build/logMessage"
                && n["params"]["message"].as_str().unwrap().contains("from a test")),
        "{notes:#?}"
    );

    // Programs: output as log messages, exit codes as status codes.
    let r = c.ok("buildTarget/run", json!({"target": t_app, "originId": "r1", "arguments": ["Ann"]}));
    assert_eq!(r, json!({"originId": "r1", "statusCode": 1}));
    let notes = c.take_notifications();
    assert!(
        notes.iter().any(|n| n["method"] == "build/logMessage" && n["params"]["message"] == "HELLO, ANN!"),
        "{notes:#?}"
    );
    let r = c.ok("buildTarget/run", json!({"target": t_app, "dataKind": "scala-main-class", "data": {"class": "app.Exit", "arguments": [], "jvmOptions": []}}));
    assert_eq!(r["statusCode"], 2);
    // The run ends with the JVM, not with a background process that keeps its pipes open.
    let spawn_at = Instant::now();
    let r = c.ok(
        "buildTarget/run",
        json!({"target": t_app, "dataKind": "scala-main-class", "data": {"class": "app.Spawn", "arguments": []}}),
    );
    assert_eq!(r["statusCode"], 1);
    assert!(spawn_at.elapsed() < Duration::from_secs(60), "took {:?}", spawn_at.elapsed());
    assert!(c.take_notifications().iter().any(|n| n["params"]["message"] == "spawned"));

    // Invalid UTF-8 output doesn't stop forwarding the lines after it.
    let r = c.ok(
        "buildTarget/run",
        json!({"target": t_app, "dataKind": "scala-main-class", "data": {"class": "app.Bytes", "arguments": []}}),
    );
    assert_eq!(r["statusCode"], 1);
    let notes = c.take_notifications();
    assert!(notes.iter().any(|n| n["params"]["message"] == "after bad bytes"), "{notes:#?}");

    // Cancelling a running program kills it and answers -32800.
    let id = c.send_request(
        "buildTarget/run",
        json!({"target": t_app, "dataKind": "scala-main-class", "data": {"class": "app.Sleep", "arguments": []}}),
    );
    c.wait_notification(Duration::from_secs(120), |n| n["params"]["message"] == "sleeping");
    // Meanwhile, Metals' empty-compile ping and a reload don't wait for the running build.
    let ping_at = Instant::now();
    assert_eq!(c.ok("buildTarget/compile", json!({"targets": []}))["statusCode"], 1);
    assert_eq!(c.ok("workspace/reload", json!({})), Value::Null);
    assert!(ping_at.elapsed() < Duration::from_secs(10), "took {:?}", ping_at.elapsed());
    let cancelled_at = Instant::now();
    c.notify("$/cancelRequest", json!({"id": id}));
    let r = c.response(id);
    assert_eq!(r["error"]["code"], -32800, "{r}");
    assert!(cancelled_at.elapsed() < Duration::from_secs(15), "took {:?}", cancelled_at.elapsed());

    // A cancelled test run: the next request still works.
    let id = c.send_request("buildTarget/test", json!({"targets": [t_test]}));
    c.notify("$/cancelRequest", json!({"id": id}));
    assert_eq!(c.response(id)["error"]["code"], -32800);
    let r = c.ok("buildTarget/compile", json!({"targets": [t_app]}));
    assert_eq!(r["statusCode"], 1);
    c.shutdown();
}

#[test]
fn java_only_and_preset_workspaces() {
    if !prerequisites() {
        return;
    }
    let ws = workspace("java-deps");
    let mut c = Client::start(ws.path(), Daemon::Shared);
    let targets = c.ok("workspace/buildTargets", json!({}))["targets"].clone();
    assert_eq!(targets.as_array().unwrap().len(), 1, "no test sources, no test target: {targets:#}");
    let t = &targets[0];
    assert_eq!(t["displayName"], "javadeps@jvm");
    assert_eq!(t["dataKind"], "jvm");
    assert!(t["data"]["javaHome"].as_str().unwrap().starts_with("file:///"));
    let id = t["id"].clone();
    assert_eq!(c.ok("buildTarget/scalacOptions", json!({"targets": [id]}))["items"], json!([]));
    // A module at the workspace root: IntelliJ excludes all of `.scpm/` through it.
    let outputs = c.ok("buildTarget/outputPaths", json!({"targets": [id]}));
    assert_eq!(items(&outputs)[0]["outputPaths"][0]["uri"], format!("{}/", c.uri(".scpm")));
    let javac = c.ok("buildTarget/javacOptions", json!({"targets": [id]}));
    let cp = strings(&items(&javac)[0]["classpath"]);
    assert!(cp.iter().any(|e| e.contains("guava-33.4.0-jre.jar")), "{cp:?}");
    assert!(!cp.iter().any(|e| e.contains("jsr305")), "excluded");
    fs::create_dir_all(ws.path().join("src/main/java/demo")).unwrap();
    fs::write(
        ws.path().join("src/main/java/demo/Hi.java"),
        "package demo;\npublic class Hi { public static void main(String[] a) { System.out.println(com.google.common.base.Strings.repeat(\"hi\", 2)); } }\n",
    )
    .unwrap();
    assert_eq!(c.ok("buildTarget/compile", json!({"targets": [id]}))["statusCode"], 1);
    let mains = c.ok("buildTarget/scalaMainClasses", json!({"targets": [id]}));
    assert_eq!(items(&mains)[0]["classes"][0]["class"], "demo.Hi", "found in the class files");
    let r = c.ok(
        "buildTarget/run",
        json!({"target": id, "dataKind": "scala-main-class", "data": {"class": "demo.Hi", "arguments": []}}),
    );
    assert_eq!(r["statusCode"], 1);
    assert!(c.take_notifications().iter().any(|n| n["params"]["message"] == "hihi"));
    c.shutdown();

    let ws = workspace("basic");
    let mut c = Client::start(ws.path(), Daemon::Shared);
    let targets = c.ok("workspace/buildTargets", json!({}))["targets"].clone();
    let core = targets
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["displayName"].as_str().unwrap().starts_with("core@"))
        .unwrap_or_else(|| panic!("{targets:#}"))
        .clone();
    let scalac = c.ok("buildTarget/scalacOptions", json!({"targets": [core["id"]]}));
    assert!(strings(&items(&scalac)[0]["options"]).contains(&"-deprecation".to_string()), "preset options: {scalac:#}");
    c.shutdown();
}

/// Sources that keep util@jvm-3 compiling for a while.
fn add_slow_sources(ws: &Path, files: usize) {
    let dir = ws.join("modules/util/src/main/scala-3/util");
    for o in 0..files {
        let mut src = format!("package util\n\nobject Slow{o}:\n");
        for i in 0..250 {
            src.push_str(&format!(
                "  def f{i}(xs: List[Int]): Map[String, List[(Int, String)]] = xs.map(x => (x + {i}, s\"v$x-{i}\")).groupBy(_._2.take(2)).map((k, v) => k -> v.sortBy(_._1).reverse)\n"
            ));
        }
        fs::write(dir.join(format!("Slow{o}.scala")), src).unwrap();
    }
}

#[test]
fn a_cancelled_compile_reports_cancelled_tasks() {
    if !prerequisites() {
        return;
    }
    let ws = workspace("app");
    add_slow_sources(ws.path(), 60);
    // Its own worker, which the cancel kills (the daemon's cancel is tested in compile.rs).
    let mut c = Client::start(ws.path(), Daemon::Off);
    let (t_app, t_util) = (c.target("app@jvm-3"), c.target("util@jvm-3"));
    let id = c.send_request("buildTarget/compile", json!({"targets": [t_app], "originId": "slow"}));
    c.wait_notification(RESPONSE_LIMIT, |n| {
        n["method"] == "build/taskStart"
            && n["params"]["dataKind"] == "compile-task"
            && n["params"]["data"]["target"] == t_util
    });
    std::thread::sleep(Duration::from_millis(1500));
    c.notify("$/cancelRequest", json!({"id": id}));
    assert_eq!(c.response(id)["error"]["code"], -32800);
    let notes = c.take_notifications();
    let report = |t: &Value| {
        notes
            .iter()
            .find(|n| n["params"]["dataKind"] == "compile-report" && n["params"]["data"]["target"] == *t)
            .unwrap_or_else(|| panic!("no report for {t}: {notes:#?}"))
            .clone()
    };
    assert_eq!(report(&t_util)["params"]["status"], 3, "cancelled, not an error");
    assert_eq!(report(&t_app)["params"]["status"], 3, "skipped because of the cancel");
    assert!(
        !notes.iter().any(|n| n["method"] == "build/logMessage" && n["params"]["type"] == 1),
        "no error logs for a cancel: {notes:#?}"
    );
    c.shutdown();
}

#[test]
fn semanticdb_for_metals() {
    if !prerequisites() {
        return;
    }
    // Without `ide.semanticdb`, a Metals client is told how to turn it on.
    let ws = workspace("app");
    let mut c = Client::start_as(ws.path(), Daemon::Shared, "Metals");
    c.ok("workspace/buildTargets", json!({}));
    let hint = c.wait_notification(Duration::from_secs(5), |n| n["method"] == "build/showMessage");
    assert!(hint["params"]["message"].as_str().unwrap().contains("semanticdb: true"), "{hint}");
    c.shutdown();

    let ws_yaml = ws.path().join("scpm-workspace.yaml");
    fs::write(&ws_yaml, fs::read_to_string(&ws_yaml).unwrap() + "ide:\n  semanticdb: true\n").unwrap();
    let mut c = Client::start_as(ws.path(), Daemon::Shared, "Metals");
    let (t_app, t_util2) = (c.target("app@jvm-3"), c.target("util@jvm-2.13"));
    let scalac = c.ok("buildTarget/scalacOptions", json!({"targets": [t_app, t_util2]}));
    let root = c.root.display().to_string();
    let app = strings(&items(&scalac)[0]["options"]);
    let classes = format!("{root}/.scpm/out/app@jvm-3/classes");
    assert!(app.windows(2).any(|w| w == ["-sourceroot", root.as_str()]), "{app:?}");
    assert!(app.windows(2).any(|w| w == ["-semanticdb-target".to_string(), classes.clone()]), "{app:?}");
    assert!(app.contains(&"-Xsemanticdb".to_string()), "{app:?}");
    let util2 = strings(&items(&scalac)[1]["options"]);
    assert!(util2.iter().any(|o| o.starts_with("-Xplugin:") && o.contains("semanticdb-scalac_2.13.18")), "{util2:?}");
    assert!(util2.contains(&format!("-P:semanticdb:sourceroot:{root}")), "{util2:?}");
    assert!(!c.take_notifications().iter().any(|n| n["method"] == "build/showMessage"), "no hint when it is on");

    assert_eq!(
        c.ok("buildTarget/compile", json!({"targets": [t_app, t_util2]}))["statusCode"],
        1,
        "{}",
        c.stderr_text()
    );
    let db = |variant: &str, file: &str| {
        ws.path().join(format!(".scpm/out/{variant}/classes/META-INF/semanticdb/{file}.semanticdb"))
    };
    assert!(db("app@jvm-3", "modules/app/src/main/scala/app/Main.scala").is_file());
    assert!(db("util@jvm-2.13", "modules/util/src/main/scala/util/Greeting.scala").is_file());
    c.shutdown();

    // `scpm compile` uses the same options: everything is up to date.
    let out = StdCommand::new(assert_cmd::cargo::cargo_bin("scpm"))
        .args(["--json", "compile", "app", "util@jvm-2.13"])
        .current_dir(ws.path())
        .env("COURSIER_CACHE", Path::new(env!("CARGO_TARGET_TMPDIR")).join("coursier-cache"))
        .env("SCPM_WORKER_JAR", scpm::worker::helper_jar("SCPM_WORKER_JAR", "scpm-compile-worker").unwrap())
        .env("SCPM_NO_DAEMON", "1")
        .env_remove("CI")
        .output()
        .unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["tasks"].as_array().unwrap().iter().all(|t| t["status"] == "upToDate"), "{doc:#}");
}

#[test]
fn a_broken_config_keeps_the_last_good_model() {
    if !prerequisites() {
        return;
    }
    let ws = workspace("app");
    let mut c = Client::start(ws.path(), Daemon::Shared);
    let t_app = c.target("app@jvm-3");
    c.ok("workspace/buildTargets", json!({}));
    let app_yaml = ws.path().join("modules/app/scpm.yaml");
    let good = fs::read_to_string(&app_yaml).unwrap();
    fs::write(&app_yaml, good.clone() + "scalacOptions: {not: a list}\n").unwrap();
    // Still served, and the user is told once.
    let sources = c.ok("buildTarget/sources", json!({"targets": [t_app]}));
    assert_eq!(items(&sources).len(), 1);
    c.ok("buildTarget/sources", json!({"targets": [t_app]}));
    let notes = c.take_notifications();
    let shown: Vec<&Value> = notes.iter().filter(|n| n["method"] == "build/showMessage").collect();
    assert_eq!(shown.len(), 1, "{notes:#?}");
    assert_eq!(shown[0]["params"]["type"], 1);
    assert!(shown[0]["params"]["message"].as_str().unwrap().contains("scalacOptions"), "{shown:?}");
    // An explicit reload reports the error.
    assert_eq!(c.request("workspace/reload", json!({}))["error"]["code"], -32603);
    // Fixed: picked up on the next request, with the new options.
    fs::write(&app_yaml, good + "scalacOptions: [-deprecation]\n").unwrap();
    let scalac = c.ok("buildTarget/scalacOptions", json!({"targets": [t_app]}));
    assert!(strings(&items(&scalac)[0]["options"]).contains(&"-deprecation".to_string()));
    c.shutdown();
}

#[test]
fn new_modules_and_symlinked_sources_are_seen() {
    if !prerequisites() {
        return;
    }
    let ws = workspace("app");
    // app's main sources live elsewhere, behind a symlink.
    let real = ws.path().join("elsewhere/app-sources");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::rename(ws.path().join("modules/app/src/main/scala"), &real).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, ws.path().join("modules/app/src/main/scala")).unwrap();
    let mut c = Client::start(ws.path(), Daemon::Shared);
    let names = |c: &mut Client| -> Vec<String> {
        c.ok("workspace/buildTargets", json!({}))["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["displayName"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(!names(&mut c).contains(&"extra@jvm".to_string()));
    #[cfg(unix)]
    {
        // Editors report the file by its real path.
        let file = scpm::ide::file_uri(&real.canonicalize().unwrap().join("app/Main.scala"));
        let inverse = c.ok("buildTarget/inverseSources", json!({"textDocument": {"uri": file}}));
        assert_eq!(inverse["targets"], json!([c.target("app@jvm-3")]), "{inverse}");
    }
    // A new module matching the `modules` glob shows up without an explicit reload.
    let extra = ws.path().join("modules/extra");
    fs::create_dir_all(extra.join("src/main/java")).unwrap();
    fs::write(extra.join("scpm.yaml"), "scala: none\n").unwrap();
    assert!(names(&mut c).contains(&"extra@jvm".to_string()));
    let changed = c.wait_notification(Duration::from_secs(5), |n| {
        n["method"] == "buildTarget/didChange"
            && n["params"]["changes"].as_array().unwrap().iter().any(|ch| ch["kind"] == 1)
    });
    assert!(
        changed["params"]["changes"].as_array().unwrap().contains(&json!({"target": c.target("extra@jvm"), "kind": 1}))
    );
    c.shutdown();
}
