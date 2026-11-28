//! The resolver sidecar (`SCPM_RESOLVER=sidecar`, design §5.2.1) against local fixture
//! repositories. These need its jars built in this checkout (`sbt resolver/stage` in worker/)
//! and a JDK; they are skipped with a message otherwise. The compatibility test also needs `cs`.

mod support;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value;
use support::Server;

/// Skips a test whose tools are missing, except in CI, where they must be there.
fn skip(why: &str) {
    if std::env::var_os("CI").is_some() {
        panic!("{why}, and CI must run this test");
    }
    eprintln!("skipping: {why}");
}

fn stage() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../worker/target/out/jvm/resolver-stage");
    if dir.join("classpath.txt").is_file() {
        Some(dir)
    } else {
        skip("the resolver's jars are not built (run `sbt resolver/stage` in worker/)");
        None
    }
}

fn cs_available() -> bool {
    let bin = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    let ok = StdCommand::new(bin).arg("version").output().is_ok_and(|o| o.status.success());
    if !ok {
        skip("the coursier CLI `cs` is not available");
    }
    ok
}

/// `scpm` with the sidecar, no boot entries (the fixture repositories can't serve them), an
/// empty user config and a private cache home.
fn scpm(dir: &Path, cache: &Path) -> Command {
    let mut cmd = Command::cargo_bin("scpm").unwrap();
    let config = dir.join(".user-config.yaml");
    if !config.exists() {
        fs::write(&config, "").unwrap();
    }
    cmd.arg("-C")
        .arg(dir)
        .env("COURSIER_CACHE", cache)
        .env("SCPM_RESOLVER", "sidecar")
        .env("SCPM_TEST_SKIP_BOOT", "1")
        .env("SCPM_CONFIG", &config)
        .env_remove("CI")
        .env_remove("COURSIER_CREDENTIALS")
        .env_remove("SCPM_RESOLVER_CLASSPATH");
    cmd
}

fn json_install(dir: &Path, cache: &Path) -> Value {
    let out = scpm(dir, cache).args(["--json", "install"]).assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap()
}

fn lock(dir: &Path) -> Value {
    serde_yaml_ng::from_str(&fs::read_to_string(dir.join("scpm-lock.yaml")).unwrap()).unwrap()
}

/// Entries as text: `key` or `key [systems]`.
fn entries(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| match e {
            Value::String(s) => s.clone(),
            other => match other.get("artifact") {
                Some(a) => format!(
                    "{} [{}]",
                    a.as_str().unwrap(),
                    other["systems"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|s| s.as_str().unwrap())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                None => format!("module:{}", other["module"].as_str().unwrap()),
            },
        })
        .collect()
}

/// A Maven repository in memory: POMs (with dependencies and OS-family profiles) and jars.
#[derive(Default)]
struct Repo {
    files: HashMap<String, Vec<u8>>,
}

impl Repo {
    fn module(&mut self, coord: &str, deps: &[&str], windows_only: &[&str], classifiers: &[&str]) -> &mut Self {
        let [g, a, v] = coord.split(':').collect::<Vec<_>>()[..] else { panic!("{coord}") };
        let dep = |d: &&str| {
            let [g, a, v] = d.split(':').collect::<Vec<_>>()[..] else { panic!("{d}") };
            format!("<dependency><groupId>{g}</groupId><artifactId>{a}</artifactId><version>{v}</version></dependency>")
        };
        let profile = if windows_only.is_empty() {
            String::new()
        } else {
            format!(
                "<profiles><profile><id>win</id><activation><os><family>windows</family></os></activation><dependencies>{}</dependencies></profile></profiles>",
                windows_only.iter().map(dep).collect::<String>()
            )
        };
        let pom = format!(
            "<?xml version=\"1.0\"?><project><modelVersion>4.0.0</modelVersion><groupId>{g}</groupId><artifactId>{a}</artifactId><version>{v}</version><dependencies>{}</dependencies>{profile}</project>",
            deps.iter().map(dep).collect::<String>()
        );
        let base = format!("/maven/{}/{a}/{v}/{a}-{v}", g.replace('.', "/"));
        self.files.insert(format!("{base}.pom"), pom.into_bytes());
        self.files.insert(format!("{base}.jar"), format!("{coord} jar").into_bytes());
        for c in classifiers {
            self.files.insert(format!("{base}-{c}.jar"), format!("{coord} {c}").into_bytes());
        }
        self
    }
}

const EPOLL: &str = "io.netty:netty-transport-native-epoll:4.1.115.Final";

/// app (Windows adds winonly) → base, nat; nat publishes per-system classifier jars; netty's
/// epoll transport, whose natives the built-in table maps.
fn fixture() -> Server {
    fixture_with(&["linux-x86_64", "linux-aarch_64"])
}

/// [`fixture`] with only these classifier jars of the epoll transport.
fn fixture_with(epoll_classifiers: &[&str]) -> Server {
    let mut r = Repo::default();
    r.module("org.fix:base:1.0", &[], &[], &[])
        .module("org.fix:winonly:1.0", &[], &[], &[])
        .module("org.fix:nat:1.0", &[], &[], &["linux-x86_64", "osx-aarch_64"])
        .module("org.fix:app:1.0", &["org.fix:base:1.0", "org.fix:nat:1.0"], &["org.fix:winonly:1.0"], &[])
        .module(EPOLL, &[], &[], epoll_classifiers)
        .module("com.example:hello:1.0", &[], &[], &[]);
    Server::maven(r.files, None)
}

/// A mapped native jar the repository doesn't serve: a warning under scpm's own mapping (its
/// table follows Maven Central, so a proxy or mirror is missing it), an error under one the
/// user wrote (most likely a typo in the classifier).
#[test]
fn a_mapped_native_jar_the_repository_lacks() {
    if stage().is_none() {
        return;
    }
    // Built-in row: epoll 4.1.115 maps linux-aarch64 to `linux-aarch_64`, which isn't served.
    let server = fixture_with(&["linux-x86_64"]);
    let ws = tempfile::tempdir().unwrap();
    // The host's own system too, so the restore after the re-lock has its projection.
    workspace(ws.path(), &server, &format!("  systems: [linux-aarch64, linux-x86_64, {}]\n", scpm::system::own()));
    let cache = tempfile::tempdir().unwrap();
    let summary = json_install(ws.path(), cache.path());
    let warnings = summary["warnings"].to_string();
    assert!(
        warnings.contains("no native jar on linux-aarch64 (`linux-aarch_64`)") && warnings.contains("404 or 403"),
        "{summary}"
    );
    // On a linux-aarch64 host, every later install says so again, frozen ones too: it is read
    // from the lock, not only from the run that re-locked.
    let arm = fake_jdk(ws.path(), "Linux", "aarch64");
    for args in [&["--json", "install"][..], &["--json", "install", "--frozen"]] {
        let out = scpm(ws.path(), cache.path())
            .env_remove("SCPM_JAVA")
            .env("SCPM_JAVA_HOME", &arm)
            .args(args)
            .assert()
            .success();
        let summary: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
        assert_eq!(summary["relocked"], serde_json::json!([]), "{summary}");
        let warnings = summary["warnings"].to_string();
        assert!(
            warnings.contains("io.netty:netty-transport-native-epoll:4.1.115.Final has no native jar for linux-aarch64 (`linux-aarch_64`)"),
            "{args:?}: {summary}"
        );
    }
    // A mapping the user wrote, with a typo: an error, and no lock is written.
    let ws = tempfile::tempdir().unwrap();
    workspace(
        ws.path(),
        &server,
        "  systems: [linux-x86_64]\n  nativeClassifiers:\n    - {module: 'org.fix:nat', natives: {linux-x86_64: linux-x86-64}}\n",
    );
    let out = scpm(ws.path(), cache.path()).arg("install").assert().code(1);
    let err = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        err.contains("org.fix:nat:1.0: the repository has no native jar for linux-x86_64 (`linux-x86-64`)"),
        "{err}"
    );
    assert!(!ws.path().join("scpm-lock.yaml").exists());
}

/// Java modules `a` (app, and the epoll transport) and `b` (hello only).
fn workspace(dir: &Path, server: &Server, lock_settings: &str) {
    fs::write(
        dir.join("scpm-workspace.yaml"),
        format!(
            "modules: ['modules/*']\nrepositories:\n  - {{id: local, url: '{}'}}\nlock:\n{lock_settings}",
            server.url("/maven")
        ),
    )
    .unwrap();
    for (name, deps) in [
        ("a", format!("  org.fix:app: '1.0'\n  {}: '4.1.115.Final'\n", EPOLL.rsplit_once(':').unwrap().0)),
        ("b", "  com.example:hello: '1.0'\n".to_string()),
    ] {
        fs::create_dir_all(dir.join("modules").join(name)).unwrap();
        fs::write(dir.join("modules").join(name).join("scpm.yaml"), format!("scala: none\ndependencies:\n{deps}"))
            .unwrap();
    }
}

const THREE: &str = "  systems: [windows-x86_64, linux-x86_64, macos-aarch64]\n  nativeClassifiers:\n    - {module: 'org.fix:nat', natives: {linux-x86_64: linux-x86_64, macos-aarch64: osx-aarch_64}}\n";

fn fake_jdk(dir: &Path, os: &str, arch: &str) -> PathBuf {
    let home = dir.join(format!("jdk-{os}-{arch}"));
    fs::create_dir_all(home.join("bin")).unwrap();
    fs::write(home.join("bin").join(if cfg!(windows) { "java.exe" } else { "java" }), "").unwrap();
    fs::write(home.join("release"), format!("JAVA_VERSION=\"25\"\nOS_NAME=\"{os}\"\nOS_ARCH=\"{arch}\"\n")).unwrap();
    home
}

#[test]
fn one_lock_holds_every_systems_classpath() {
    if stage().is_none() {
        return;
    }
    let server = fixture();
    let ws = tempfile::tempdir().unwrap();
    workspace(ws.path(), &server, THREE);
    let cache = tempfile::tempdir().unwrap();
    let summary = json_install(ws.path(), cache.path());
    assert_eq!(summary["relocked"], serde_json::json!(["a@jvm", "b@jvm"]), "{summary}");
    let l = lock(ws.path());
    assert_eq!(l["systems"], serde_json::json!(["linux-x86_64", "macos-aarch64", "windows-x86_64"]));
    assert_eq!(l["nativeMappings"], 1);
    assert!(l.get("defaultSystems").is_none());
    let a = &l["variants"]["a@jvm"];
    assert_eq!(a["fullContext"], true);
    assert_eq!(
        entries(&a["runtimeClasspath"]),
        // Coursier's order (roots first), natives next to their module; a Windows profile's
        // dependency where Windows' own order puts it.
        [
            "org.fix:app:1.0",
            "io.netty:netty-transport-native-epoll:4.1.115.Final",
            "io.netty:netty-transport-native-epoll:4.1.115.Final:linux-x86_64 [linux-x86_64]",
            "org.fix:winonly:1.0 [windows-x86_64]",
            "org.fix:base:1.0",
            "org.fix:nat:1.0",
            "org.fix:nat:1.0:linux-x86_64 [linux-x86_64]",
            "org.fix:nat:1.0:osx-aarch_64 [macos-aarch64]",
        ]
    );
    // A jar-only classpath has no `systems` anywhere.
    assert_eq!(entries(&l["variants"]["b@jvm"]["runtimeClasspath"]), ["com.example:hello:1.0"]);
    for key in ["org.fix:nat:1.0:osx-aarch_64", "org.fix:winonly:1.0", EPOLL] {
        assert_eq!(l["artifacts"][key]["repo"], "local", "{key}");
    }

    // A frozen restore downloads only the host's files.
    let linux = fake_jdk(ws.path(), "Linux", "amd64");
    let empty = tempfile::tempdir().unwrap();
    let requests_before = server.requests.lock().unwrap().len();
    scpm(ws.path(), empty.path())
        .env_remove("SCPM_JAVA")
        .env("SCPM_JAVA_HOME", &linux)
        .args(["install", "--frozen"])
        .assert()
        .success();
    let paths: Vec<String> =
        server.requests.lock().unwrap()[requests_before..].iter().map(|r| r.path.clone()).collect();
    assert!(paths.iter().any(|p| p.ends_with("nat-1.0-linux-x86_64.jar")), "{paths:?}");
    assert!(paths.iter().any(|p| p.ends_with("epoll-4.1.115.Final-linux-x86_64.jar")), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains("osx") || p.contains("winonly") || p.ends_with(".pom")), "{paths:?}");
    // And it never starts the sidecar: an unusable resolver classpath doesn't matter.
    scpm(ws.path(), empty.path())
        .env_remove("SCPM_JAVA")
        .env("SCPM_JAVA_HOME", &linux)
        .env("SCPM_RESOLVER_CLASSPATH", "/nonexistent/resolver.jar")
        .args(["install", "--frozen"])
        .assert()
        .success();

    // Unchanged, the lock is fresh.
    let again = json_install(ws.path(), cache.path());
    assert_eq!(again["relocked"], serde_json::json!([]), "{again}");
    assert_eq!(again["lockfileWritten"], false);
}

#[test]
fn mapping_table_changes_relock_only_affected_variants() {
    if stage().is_none() {
        return;
    }
    let server = fixture();
    let ws = tempfile::tempdir().unwrap();
    workspace(ws.path(), &server, "  systems: [linux-x86_64, macos-aarch64]\n");
    let cache = tempfile::tempdir().unwrap();
    json_install(ws.path(), cache.path());
    let path = ws.path().join("scpm-lock.yaml");
    let edit_header = |n: u32| {
        let mut l: serde_yaml_ng::Value = serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        l["nativeMappings"] = n.into();
        fs::write(&path, serde_yaml_ng::to_string(&l).unwrap()).unwrap();
    };
    // M > N: only `a` holds a module whose mapping changed since table 0; the header becomes M.
    edit_header(0);
    let summary = json_install(ws.path(), cache.path());
    assert_eq!(summary["relocked"], serde_json::json!(["a@jvm"]), "{summary}");
    assert_eq!(lock(ws.path())["nativeMappings"], 1);
    // The same with the header edited as text: the re-lock gives the lock it had, and the
    // header is raised anyway, so the next run re-locks nothing.
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("\nnativeMappings: 1\n"), "{text}");
    fs::write(&path, text.replace("\nnativeMappings: 1\n", "\nnativeMappings: 0\n")).unwrap();
    assert_eq!(json_install(ws.path(), cache.path())["relocked"], serde_json::json!(["a@jvm"]));
    assert_eq!(fs::read_to_string(&path).unwrap(), text, "header raised; nothing else changed");
    assert_eq!(json_install(ws.path(), cache.path())["relocked"], serde_json::json!([]));
    // M < N: nothing to re-lock only restores; anything to re-lock stops.
    edit_header(99);
    let summary = json_install(ws.path(), cache.path());
    assert_eq!(summary["relocked"], serde_json::json!([]));
    assert_eq!(lock(ws.path())["nativeMappings"], 99, "never downgraded");
    fs::write(
        ws.path().join("modules/b/scpm.yaml"),
        "scala: none\ndependencies:\n  com.example:hello: {version: '1.0', transitive: false}\n",
    )
    .unwrap();
    let out = scpm(ws.path(), cache.path()).arg("install").assert().code(1);
    let err = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(err.contains("native mapping table 99") && err.contains("upgrade scpm"), "{err}");
    // A frozen run only notes the difference.
    edit_header(1);
    let out = scpm(ws.path(), cache.path()).args(["install"]).assert().success();
    let _ = out;
    edit_header(7);
    let out = scpm(ws.path(), cache.path()).args(["--json", "install", "--frozen"]).assert().success();
    let summary: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert!(summary["notes"].to_string().contains("mapping table 7, this scpm has 1"), "{summary}");
}

#[test]
fn the_sidecar_relocks_only_variants_cs_wrote() {
    if stage().is_none() || !cs_available() {
        return;
    }
    let server = fixture();
    let ws = tempfile::tempdir().unwrap();
    workspace(ws.path(), &server, "  jdk: 25\n");
    let cache = tempfile::tempdir().unwrap();
    scpm(ws.path(), cache.path()).env("SCPM_RESOLVER", "cs").arg("install").assert().success();
    let cs_lock = fs::read_to_string(ws.path().join("scpm-lock.yaml")).unwrap();
    assert!(!cs_lock.contains("fullContext"));
    // Unmark a after the sidecar marked both: only a re-locks.
    let summary = json_install(ws.path(), cache.path());
    assert_eq!(summary["relocked"], serde_json::json!(["a@jvm", "b@jvm"]), "{summary}");
    let path = ws.path().join("scpm-lock.yaml");
    let mut l: serde_yaml_ng::Value = serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    l["variants"]["a@jvm"].as_mapping_mut().unwrap().remove("fullContext");
    fs::write(&path, serde_yaml_ng::to_string(&l).unwrap()).unwrap();
    let summary = json_install(ws.path(), cache.path());
    assert_eq!(summary["relocked"], serde_json::json!(["a@jvm"]), "{summary}");
    assert_eq!(lock(ws.path())["variants"]["a@jvm"]["fullContext"], true);
}

/// Every golden single-system fixture gives the same lock from `cs` and the sidecar, but for
/// what only the sidecar writes (design §5.2.1, "Compatibility test"). The fixture is resolved
/// for the machine's own system, where `cs` applies the same profiles, and maps no natives.
#[test]
fn cs_and_the_sidecar_write_the_same_single_system_lock() {
    if stage().is_none() || !cs_available() {
        return;
    }
    let server = fixture();
    let own = scpm::system::own();
    let strip = |text: String| -> String {
        text.lines()
            .filter(|l| {
                !l.contains("fullContext:") && !l.starts_with("nativeMappings:") && !l.starts_with("bootFullContext:")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut locks = Vec::new();
    for resolver in ["cs", "sidecar"] {
        let ws = tempfile::tempdir().unwrap();
        workspace(ws.path(), &server, &format!("  systems: [{own}]\n"));
        // No mapped module: `cs` maps none.
        fs::write(ws.path().join("modules/a/scpm.yaml"), "scala: none\ndependencies:\n  org.fix:app: '1.0'\n").unwrap();
        let cache = tempfile::tempdir().unwrap();
        scpm(ws.path(), cache.path()).env("SCPM_RESOLVER", resolver).arg("install").assert().success();
        locks.push(strip(fs::read_to_string(ws.path().join("scpm-lock.yaml")).unwrap()));
    }
    assert_eq!(locks[0], locks[1]);
}

/// The sidecar starts with a cleared environment (design §5.2.1): JVM option variables and
/// coursier's own variables never reach it, so this resolves exactly as without them.
#[test]
fn a_hostile_environment_does_not_reach_the_sidecar() {
    if stage().is_none() {
        return;
    }
    let server = fixture();
    let ws = tempfile::tempdir().unwrap();
    workspace(ws.path(), &server, "  systems: [linux-x86_64, macos-aarch64]\n");
    let cache = tempfile::tempdir().unwrap();
    let mirrors = ws.path().join("mirrors.properties");
    fs::write(&mirrors, format!("m.from={}\nm.to=https://127.0.0.1:1/nowhere\n", server.url("/maven"))).unwrap();
    scpm(ws.path(), cache.path())
        .env("JAVA_TOOL_OPTIONS", "-Dcoursier.mode=offline -Dhttp.proxyHost=127.0.0.1 -Dhttp.proxyPort=1")
        .env("_JAVA_OPTIONS", "-Dcoursier.repositories=https://127.0.0.1:1/x")
        .env("JDK_JAVA_OPTIONS", "-Djava.net.useSystemProxies=true")
        .env("COURSIER_MODE", "offline")
        .env("COURSIER_REPOSITORIES", "https://127.0.0.1:1/x")
        .env("COURSIER_MIRRORS", &mirrors)
        .env("COURSIER_CACHE_SERVER", "https://127.0.0.1:1/")
        .arg("install")
        .assert()
        .success();
    assert_eq!(lock(ws.path())["artifacts"]["org.fix:app:1.0"]["repo"], "local");
}

/// The bootstrap list: jars restored from `resolverMirror`, each checked against its sha256,
/// into the cache, without credentials; and `SCPM_RESOLVER_CLASSPATH` with its notice.
#[test]
fn the_resolvers_jars_come_from_the_pinned_list() {
    let Some(stage) = stage() else { return };
    let names: Vec<String> = fs::read_to_string(stage.join("classpath.txt"))
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    // Assets are named by their sha256.
    let mut files = HashMap::new();
    let mut list = String::from("release none\n");
    for n in &names {
        let bytes = fs::read(stage.join(n)).unwrap();
        let sha = scpm::fetch::hex(&sha2_digest(&bytes));
        list.push_str(&format!("{sha} {} {n}\n", bytes.len()));
        files.insert(format!("/jars/{sha}.jar"), bytes);
    }
    let jars = Server::maven(files.clone(), None);
    let fixture = fixture();
    let ws = tempfile::tempdir().unwrap();
    workspace(ws.path(), &fixture, "  jdk: 25\n");
    fs::write(ws.path().join("list.txt"), &list).unwrap();
    // Credentials that match the mirror's origin, from the user config and from coursier's own
    // file: neither may be sent there.
    let origin = jars.url("/");
    let user_config = |mirror: &str| {
        format!("resolverMirror: {mirror}\ncredentials:\n  - {{url: '{origin}', user: alice, password: s3cret}}\n")
    };
    fs::write(ws.path().join(".user-config.yaml"), user_config(&jars.url("/jars/"))).unwrap();
    let coursier_credentials = ws.path().join("credentials.properties");
    fs::write(
        &coursier_credentials,
        "m.host=127.0.0.1\nm.username=bob\nm.password=hunter2\nm.auto=true\nm.https-only=false\n",
    )
    .unwrap();
    // A private cache home for `<cache>/scpm/resolver`.
    let home = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let in_home = |cmd: &mut Command, home: &Path| {
        cmd.env("HOME", home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("COURSIER_CREDENTIALS", &coursier_credentials)
            .env("SCPM_TEST_RESOLVER_LIST", ws.path().join("list.txt"));
    };
    let mut cmd = scpm(ws.path(), cache.path());
    in_home(&mut cmd, home.path());
    cmd.arg("install").assert().success();
    let reqs = jars.requests.lock().unwrap().clone();
    assert_eq!(reqs.iter().filter(|r| r.method == "GET").count(), names.len(), "{reqs:?}");
    assert!(reqs.iter().all(|r| r.authorization.is_none()), "no credentials on the resolver's jars: {reqs:?}");
    let resolver_cache = walk(home.path());
    assert!(resolver_cache.iter().filter(|p| p.to_string_lossy().contains("scpm/resolver/")).count() >= names.len());

    // A served jar that doesn't match its pin is refused.
    let mut tampered = files;
    let (first, bytes) = tampered.iter_mut().next().unwrap();
    bytes.reverse();
    let first = first.clone();
    let tampered = Server::maven(tampered, None);
    fs::write(ws.path().join(".user-config.yaml"), user_config(&tampered.url("/jars/"))).unwrap();
    fs::remove_file(ws.path().join("scpm-lock.yaml")).unwrap();
    let fresh = tempfile::tempdir().unwrap();
    let mut cmd = scpm(ws.path(), cache.path());
    in_home(&mut cmd, fresh.path());
    let out = cmd.arg("install").assert().failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(err.contains("sha256") || err.contains("checksum") || err.contains(&first[6..20]), "{err}");
    assert!(!walk(fresh.path()).iter().any(|p| p.ends_with(&first[6..])), "a mismatching jar is never kept");

    // SCPM_RESOLVER_CLASSPATH replaces the pins, with a notice.
    let cp = std::env::join_paths(names.iter().map(|n| stage.join(n))).unwrap();
    let out = scpm(ws.path(), cache.path()).env("SCPM_RESOLVER_CLASSPATH", cp).arg("install").assert().success();
    let err = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(err.contains("SCPM_RESOLVER_CLASSPATH replaces the resolver's jars"), "{err}");
}

/// A stand-in `scpm.resolver.Main` compiled from `body` (Java), on a classpath of its own.
fn fake_resolver(dir: &Path, body: &str) -> Option<PathBuf> {
    let Some(home) = scpm::worker::detected_java_home() else {
        skip("no JDK home to compile a fake resolver with");
        return None;
    };
    let src = dir.join("src/scpm/resolver/Main.java");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(
        &src,
        format!(
            "package scpm.resolver;\nimport java.io.*;\npublic class Main {{ public static void main(String[] a) throws Exception {{ {body} }} }}\n"
        ),
    )
    .unwrap();
    let classes = dir.join("classes");
    let javac = home.join("bin").join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let ok = StdCommand::new(javac).arg("-d").arg(&classes).arg(&src).status().is_ok_and(|s| s.success());
    assert!(ok, "javac failed");
    Some(classes)
}

/// A sidecar that doesn't answer is stopped after its timeout, and the command fails saying
/// so: at start-up (`initialize`), and in a request (`resolve`).
#[test]
fn a_sidecar_that_does_not_answer_is_stopped() {
    let server = fixture();
    let ws = tempfile::tempdir().unwrap();
    workspace(ws.path(), &server, "  jdk: 25\n");
    let cache = tempfile::tempdir().unwrap();
    let silent = "Thread.sleep(Long.MAX_VALUE);";
    // Answers `initialize` (id 1), then nothing.
    let init_only = "BufferedReader in = new BufferedReader(new InputStreamReader(System.in)); in.readLine(); \
        System.out.println(\"{\\\"id\\\":1,\\\"result\\\":{\\\"protocolVersion\\\":1}}\"); System.out.flush(); \
        Thread.sleep(Long.MAX_VALUE);";
    for (name, body, method) in [("silent", silent, "initialize"), ("init-only", init_only, "resolve")] {
        let Some(classes) = fake_resolver(&ws.path().join(name), body) else { return };
        let started = std::time::Instant::now();
        let out = scpm(ws.path(), cache.path())
            .env("SCPM_RESOLVER_CLASSPATH", &classes)
            .env("SCPM_TEST_RESOLVER_TIMEOUT_MS", "1500")
            .arg("install")
            .assert()
            .failure();
        let err = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
        assert!(err.contains(&format!("did not answer `{method}`")), "{name}: {err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(60), "{name}: stopped in time");
    }
}

fn sha2_digest(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).to_vec()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
