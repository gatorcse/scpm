//! `scpm install` against real Maven Central (and local HTTP servers). These tests need network
//! access and the native coursier CLI (`cs`, or `SCPM_CS`); they are skipped with a message when
//! `cs` is missing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value;

fn cs_available() -> bool {
    let bin = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    let ok = StdCommand::new(bin).arg("version").output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: coursier CLI `cs` not found");
    }
    ok
}

/// A coursier cache shared by tests that only need resolution to be fast, under target/.
fn shared_cache() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("coursier-cache")
}

fn copy_fixture(name: &str) -> tempfile::TempDir {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
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

fn scpm(dir: &Path, cache: &Path) -> Command {
    let mut cmd = Command::cargo_bin("scpm").unwrap();
    cmd.arg("-C").arg(dir).env("COURSIER_CACHE", cache).env_remove("CI").env_remove("COURSIER_CREDENTIALS");
    // Never the developer's own machine settings: an empty user config.
    let config = Path::new(env!("CARGO_TARGET_TMPDIR")).join("empty-user-config.yaml");
    if !config.exists() {
        fs::write(&config, "").unwrap();
    }
    cmd.env("SCPM_CONFIG", config);
    cmd
}

fn lock(dir: &Path) -> Value {
    let text = fs::read_to_string(dir.join("scpm-lock.yaml")).unwrap();
    serde_yaml_ng::from_str(&text).unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| match e {
            Value::String(s) => s.clone(),
            other => format!("module:{}", other["module"].as_str().unwrap()),
        })
        .collect()
}

#[test]
fn install_locks_joint_classpaths() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let l = lock(ws.path());

    let variants: Vec<&String> = l["variants"].as_object().unwrap().keys().collect();
    assert_eq!(variants, ["core@jvm-3", "util@jvm-2.13", "util@jvm-3"]);

    // The catalog range is selected once per (platform, Scala binary version).
    let selection = &l["catalogs"]["default/org.typelevel::cats-core@jvm-3"];
    assert_eq!(selection["range"], "^2.12.0");
    let cats = selection["version"].as_str().unwrap();
    assert_eq!(l["variants"]["util@jvm-3"]["specifiers"]["org.typelevel::cats-core"]["version"], cats);

    let core = &l["variants"]["core@jvm-3"];
    let compile = strings(&core["compileClasspath"]);
    assert_eq!(compile[0], "module:util@jvm-3", "upstream modules come first");
    assert_eq!(
        compile[1], "org.scala-lang:scala3-library_3:3.9.0",
        "the Scala library is forced to the variant's version"
    );
    assert!(
        compile.contains(&format!("org.typelevel:cats-core_3:{cats}")),
        "joint resolution includes util's deps: {compile:?}"
    );
    assert!(
        !compile.iter().any(|k| k.contains("jsr305") || k.contains("checker-qual")),
        "exclusions apply: {compile:?}"
    );
    assert!(!compile.iter().any(|k| k.contains("munit")), "test deps stay out of compile");
    assert!(strings(&core["testCompileClasspath"]).iter().any(|k| k.starts_with("org.scalameta:munit_3:1.2.1")));
    assert!(core["specifiers"]["io.circe::circe-core"]["version"].as_str().unwrap().starts_with("0.14."));

    for (name, first) in [
        ("scala-compiler@3.9.0", "org.scala-lang:scala3-compiler_3:3.9.0"),
        ("compiler-bridge@2.13.18+zinc-2.1.0", "org.scala-sbt:compiler-bridge_2.13:2.1.0"),
    ] {
        assert_eq!(l["boot"][name][0], first, "boot {name}");
    }
    let artifact = &l["artifacts"][format!("org.typelevel:cats-core_3:{cats}")];
    assert_eq!(artifact["repo"], "central");
    assert_eq!(artifact["sha256"].as_str().unwrap().len(), 64);

    // A second install changes nothing.
    let before = fs::read_to_string(ws.path().join("scpm-lock.yaml")).unwrap();
    let out = scpm(ws.path(), &shared_cache()).arg("install").assert().success().get_output().stdout.clone();
    assert!(String::from_utf8_lossy(&out).contains("Lockfile up to date (3 variants)"));
    assert_eq!(fs::read_to_string(ws.path().join("scpm-lock.yaml")).unwrap(), before);
}

#[test]
fn stale_lock_is_an_error_when_frozen_and_relocks_only_affected_variants() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let util = ws.path().join("modules/util/scpm.yaml");
    let text = fs::read_to_string(&util).unwrap();
    fs::write(&util, format!("{text}runtimeDependencies:\n  org.slf4j:slf4j-api: 2.0.17\n")).unwrap();

    let err =
        scpm(ws.path(), &shared_cache()).args(["install", "--frozen"]).assert().code(1).get_output().stderr.clone();
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("scpm-lock.yaml is out of date for: core@jvm-3, util@jvm-2.13, util@jvm-3"), "{err}");

    // CI=true implies --frozen.
    scpm(ws.path(), &shared_cache()).env("CI", "true").arg("install").assert().code(1);

    let out =
        scpm(ws.path(), &shared_cache()).args(["--json", "install"]).assert().success().get_output().stdout.clone();
    let summary: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(summary["relocked"], serde_json::json!(["core@jvm-3", "util@jvm-2.13", "util@jvm-3"]));
    let l = lock(ws.path());
    assert!(
        strings(&l["variants"]["core@jvm-3"]["runtimeClasspath"]).contains(&"org.slf4j:slf4j-api:2.0.17".to_string())
    );
    assert!(
        !strings(&l["variants"]["core@jvm-3"]["compileClasspath"]).contains(&"org.slf4j:slf4j-api:2.0.17".to_string())
    );
}

#[test]
fn frozen_install_restores_into_an_empty_cache_without_poms() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("java-deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();

    let empty = tempfile::tempdir().unwrap();
    let out = scpm(ws.path(), empty.path())
        .args(["--json", "install", "--frozen"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: Value = serde_json::from_slice(&out).unwrap();
    assert!(summary["fetch"]["downloaded"].as_u64().unwrap() >= 3, "{summary}");
    let files: Vec<PathBuf> = walk(empty.path());
    assert!(files.iter().any(|f| f.ends_with("guava-33.4.0-jre.jar")));
    assert!(!files.iter().any(|f| f.extension().is_some_and(|e| e == "pom")), "no POMs: {files:?}");
    assert!(!files.iter().any(|f| f.to_string_lossy().ends_with(".part") || f.to_string_lossy().ends_with(".lock")));

    // coursier can use (and extend) a cache that scpm populated.
    let cs = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    let ok = StdCommand::new(cs)
        .args(["fetch", "--cache"])
        .arg(empty.path())
        .arg("com.google.guava:guava:33.4.0-jre")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok);
}

#[test]
fn tampered_checksum_is_rejected() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("java-deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let path = ws.path().join("scpm-lock.yaml");
    let text = fs::read_to_string(&path).unwrap();
    let mut l: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).unwrap();
    l["artifacts"]["org.slf4j:slf4j-api:2.0.17"]["sha256"] = "0".repeat(64).into();
    fs::write(&path, serde_yaml_ng::to_string(&l).unwrap()).unwrap();

    let empty = tempfile::tempdir().unwrap();
    let err = scpm(ws.path(), empty.path()).args(["install", "--frozen"]).assert().code(1).get_output().stderr.clone();
    assert!(String::from_utf8_lossy(&err).contains("checksum mismatch"), "{}", String::from_utf8_lossy(&err));
    assert!(
        !walk(empty.path()).iter().any(|f| f.ends_with("slf4j-api-2.0.17.jar")),
        "a bad download must not land in the cache"
    );
}

#[test]
fn why_and_classpath() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    let why = scpm(ws.path(), &shared_cache())
        .args(["why", "cats-kernel", "-F", "util@jvm-3"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8_lossy(&why).starts_with("util@jvm-3: org.typelevel:cats-core_3:"),
        "{}",
        String::from_utf8_lossy(&why)
    );
    // Coursier explains the chains, at the locked versions, including upstream declarations.
    let out = scpm(ws.path(), &shared_cache())
        .args(["--json", "why", "org.typelevel::cats-kernel", "-F", "core@jvm-3"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&out).unwrap();
    let paths = report["paths"].as_array().unwrap();
    assert!(
        paths.iter().any(|p| p["declaredBy"] == "util@jvm-3"
            && p["chain"][0].as_str().unwrap().starts_with("org.typelevel:cats-core_3:")),
        "{report}"
    );
    assert!(
        paths.iter().any(|p| p["declaredBy"] == "core@jvm-3"
            && p["chain"][0].as_str().unwrap().starts_with("io.circe:circe-core_3:")),
        "{report}"
    );
    assert!(
        paths.iter().all(|p| p["chain"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("cats-kernel_3"))
    );
    assert_eq!(report["unexplained"], serde_json::json!([]));
    assert!(report.get("warnings").is_none(), "{report}");
    // Machine-settings warnings reach `why --json` too.
    let missing = ws.path().join("no-such-config.yaml");
    let out = scpm(ws.path(), &shared_cache())
        .env("SCPM_CONFIG", &missing)
        .args(["--json", "why", "cats-kernel", "-F", "util@jvm-3"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&out).unwrap();
    assert!(report["warnings"][0].as_str().unwrap().contains("no-such-config.yaml"), "{report}");

    let out = scpm(ws.path(), &shared_cache())
        .args(["--json", "classpath", "core", "--kind", "test-runtime"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let files: Vec<String> = serde_json::from_slice(&out).unwrap();
    assert!(files[0].ends_with(".scpm/out/util@jvm-3/classes"), "{files:?}");
    assert!(files.iter().any(|f| f.ends_with("munit_3-1.2.1.jar")));
    assert!(files.iter().skip(1).all(|f| Path::new(f).is_file()), "every jar is in the cache");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

mod support;

use support::{HELLO_JAR_PATH, Server, hello_repo_files};

/// Writes a workspace with one Java-only module `lib` (variant `lib@jvm`).
fn java_workspace(dir: &Path, workspace_extra: &str, lib: &str) {
    fs::write(dir.join("scpm-workspace.yaml"), format!("modules: ['modules/*']\n{workspace_extra}")).unwrap();
    fs::create_dir_all(dir.join("modules/lib")).unwrap();
    fs::write(dir.join("modules/lib/scpm.yaml"), format!("scala: none\n{lib}")).unwrap();
}

fn json_install(dir: &Path, cache: &Path, extra: &[&str]) -> Value {
    let out = scpm(dir, cache).args(["--json", "install"]).args(extra).assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap()
}

fn stderr_of(cmd: &mut Command) -> String {
    String::from_utf8_lossy(&cmd.assert().code(1).get_output().stderr).into_owned()
}

#[test]
fn changing_repositories_rederives_repo_ids() {
    if !cs_available() {
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let lib = "dependencies:\n  org.slf4j:slf4j-api: 2.0.17\n";
    java_workspace(ws.path(), "repositories:\n  - {id: apache, url: 'https://repo.maven.apache.org/maven2'}\n", lib);
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    assert_eq!(lock(ws.path())["artifacts"]["org.slf4j:slf4j-api:2.0.17"]["repo"], "apache");

    java_workspace(ws.path(), "repositories: [central]\n", lib);
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    assert_eq!(lock(ws.path())["artifacts"]["org.slf4j:slf4j-api:2.0.17"]["repo"], "central");
    let empty = tempfile::tempdir().unwrap();
    scpm(ws.path(), empty.path()).args(["install", "--frozen"]).assert().success();
    assert!(
        walk(empty.path())
            .iter()
            .any(|f| f.ends_with("repo1.maven.org/maven2/org/slf4j/slf4j-api/2.0.17/slf4j-api-2.0.17.jar"))
    );
}

#[test]
fn locked_ranges_are_not_reselected_when_a_variant_relocks() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    // Pretend the range was locked when 0.14.10 was the latest.
    let path = ws.path().join("scpm-lock.yaml");
    let mut l: serde_yaml_ng::Value = serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    l["variants"]["core@jvm-3"]["specifiers"]["io.circe::circe-core"]["version"] = "0.14.10".into();
    fs::write(&path, serde_yaml_ng::to_string(&l).unwrap()).unwrap();

    // An unrelated edit re-locks core@jvm-3, but its unchanged range keeps its version.
    let core = ws.path().join("modules/core/scpm.yaml");
    fs::write(&core, format!("{}  org.slf4j:slf4j-api: 2.0.17\n", fs::read_to_string(&core).unwrap())).unwrap();
    let summary = json_install(ws.path(), &shared_cache(), &[]);
    assert_eq!(summary["relocked"], serde_json::json!(["core@jvm-3"]));
    let l = lock(ws.path());
    assert_eq!(l["variants"]["core@jvm-3"]["specifiers"]["io.circe::circe-core"]["version"], "0.14.10");
    assert!(
        strings(&l["variants"]["core@jvm-3"]["compileClasspath"]).contains(&"io.circe:circe-core_3:0.14.10".into())
    );

    // Editing the spec selects again.
    let text = fs::read_to_string(&core).unwrap().replace("^0.14.10", "~0.14.11");
    fs::write(&core, text).unwrap();
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let selected = lock(ws.path())["variants"]["core@jvm-3"]["specifiers"]["io.circe::circe-core"]["version"].clone();
    assert_ne!(selected, "0.14.10");
}

#[test]
fn declared_classifiers_and_types_are_locked() {
    if !cs_available() {
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    java_workspace(
        ws.path(),
        "",
        "dependencies:\n  \
           org.slf4j:slf4j-api: {version: 2.0.17, classifier: sources}\n  \
           com.jakewharton.timber:timber: {version: 4.7.1, type: aar, transitive: false}\n",
    );
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let compile = strings(&lock(ws.path())["variants"]["lib@jvm"]["compileClasspath"]);
    assert!(compile.contains(&"org.slf4j:slf4j-api:2.0.17:sources".to_string()), "{compile:?}");
    assert!(compile.contains(&"com.jakewharton.timber:timber:4.7.1@aar".to_string()), "{compile:?}");

    let empty = tempfile::tempdir().unwrap();
    scpm(ws.path(), empty.path()).args(["install", "--frozen"]).assert().success();
    let files = walk(empty.path());
    assert!(files.iter().any(|f| f.ends_with("slf4j-api-2.0.17-sources.jar")), "{files:?}");
    assert!(files.iter().any(|f| f.ends_with("timber-4.7.1.aar")), "{files:?}");

    // A direct dependency that yields no file is an error naming it, its variant and classpath.
    java_workspace(ws.path(), "", "dependencies:\n  org.junit:junit-bom: 5.11.4\n");
    // A plain coordinate with `<packaging>pom</packaging>` (here a BOM) is allowed to have no
    // jar of its own, as in Maven and sbt.
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    // A declared classifier that doesn't exist is an error.
    java_workspace(ws.path(), "", "dependencies:\n  org.slf4j:slf4j-api: {version: '2.0.17', classifier: nope}\n");
    let err = stderr_of(scpm(ws.path(), &shared_cache()).arg("install"));
    assert!(err.contains("slf4j-api-2.0.17-nope.jar: not found"), "{err}");
    assert!(err.contains("classpath of lib@jvm"), "{err}");
}

/// A `cs` wrapper that logs its arguments, to check that no secret reaches argv.
fn logging_cs(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("cs-args.log");
    let script = dir.join("cs-wrapper.sh");
    let cs = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\nexec '{cs}' \"$@\"\n", log.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (script, log)
}

#[test]
fn configured_credentials_reach_cs_through_a_private_file() {
    if !cs_available() || cfg!(windows) {
        return;
    }
    let server = Server::maven(hello_repo_files(b"hello jar"), Some(("alice", "s3cret-pass")));
    let ws = tempfile::tempdir().unwrap();
    let manifest = |opt_in: &str| {
        format!(
            "repositories:\n  - id: local\n    url: '{}'\n    credentials: {{user: 'env:REPO_USER', password: 'env:REPO_PASS'}}\n{opt_in}",
            server.url("/maven")
        )
    };
    java_workspace(ws.path(), &manifest(""), "dependencies:\n  com.example:hello: '1.0'\n");
    let (wrapper, log) = logging_cs(ws.path());
    let cache = tempfile::tempdir().unwrap();
    let with_creds = |cache: &Path| {
        let mut cmd = scpm(ws.path(), cache);
        cmd.env("SCPM_CS", &wrapper).env("REPO_USER", "alice").env("REPO_PASS", "s3cret-pass");
        cmd
    };
    // Credentials for a plain-HTTP repository are a config error without the opt-in.
    let err = stderr_of(with_creds(cache.path()).arg("install"));
    assert!(err.contains("plain HTTP") && err.contains("allowInsecureCredentials"), "{err}");
    java_workspace(
        ws.path(),
        &manifest("    allowInsecureCredentials: true\n"),
        "dependencies:\n  com.example:hello: '1.0'\n",
    );
    with_creds(cache.path()).arg("install").assert().success();
    let args = fs::read_to_string(&log).unwrap();
    assert!(!args.contains("s3cret-pass") && !args.contains("--credential"), "{args}");

    // The Rust fetcher uses the same credentials and coursier's `user@host` cache layout.
    let empty = tempfile::tempdir().unwrap();
    with_creds(empty.path()).args(["install", "--frozen"]).assert().success();
    let jar = empty
        .path()
        .join(format!("http/alice%40127.0.0.1%3A{}/maven/com/example/hello/1.0/hello-1.0.jar", server.port));
    assert_eq!(fs::read(&jar).unwrap(), b"hello jar");

    // Missing variables only matter when the repository is contacted: everything is cached.
    scpm(ws.path(), empty.path()).args(["install", "--frozen"]).assert().success();
    let out = scpm(ws.path(), empty.path()).args(["classpath", "lib", "--frozen"]).assert().success();
    assert!(String::from_utf8_lossy(&out.get_output().stdout).contains("alice%40127.0.0.1"));
    let other = tempfile::tempdir().unwrap();
    let err = stderr_of(scpm(ws.path(), other.path()).args(["install", "--frozen"]));
    assert!(err.contains("environment variable `REPO_USER` (user) is not set"), "{err}");
    let err = stderr_of(
        scpm(ws.path(), other.path()).env("REPO_USER", "alice").env("REPO_PASS", "").args(["install", "--frozen"]),
    );
    assert!(err.contains("`REPO_PASS` (password) is empty"), "{err}");
}

#[test]
fn coursier_credentials_are_used_for_repositories_without_configured_ones() {
    if !cs_available() {
        return;
    }
    let server = Server::maven(hello_repo_files(b"hello jar"), Some(("alice", "s3cret")));
    let ws = tempfile::tempdir().unwrap();
    let props = ws.path().join("coursier-credentials.properties");
    fs::write(&props, "local.host=127.0.0.1\nlocal.username=alice\nlocal.password=s3cret\nlocal.https-only=false\n")
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    // Without the repository's opt-in, not even `cs` gets them over plain HTTP.
    java_workspace(
        ws.path(),
        &format!("repositories:\n  - {{id: local, url: '{}'}}\n", server.url("/maven")),
        "dependencies:\n  com.example:hello: '1.0'\n",
    );
    let err = stderr_of(scpm(ws.path(), cache.path()).env("COURSIER_CREDENTIALS", &props).arg("install"));
    assert!(err.contains("401") || err.contains("nauthorized"), "{err}");
    assert!(server.requests.lock().unwrap().iter().all(|r| r.authorization.is_none()));

    java_workspace(
        ws.path(),
        &format!("repositories:\n  - {{id: local, url: '{}', allowInsecureCredentials: true}}\n", server.url("/maven")),
        "dependencies:\n  com.example:hello: '1.0'\n",
    );
    scpm(ws.path(), cache.path()).env("COURSIER_CREDENTIALS", &props).arg("install").assert().success();

    let empty = tempfile::tempdir().unwrap();
    scpm(ws.path(), empty.path()).env("COURSIER_CREDENTIALS", &props).args(["install", "--frozen"]).assert().success();
    let jar = empty
        .path()
        .join(format!("http/alice%40127.0.0.1%3A{}/maven/com/example/hello/1.0/hello-1.0.jar", server.port));
    assert_eq!(fs::read(&jar).unwrap(), b"hello jar");
    assert!(server.count(HELLO_JAR_PATH) >= 2);
}

#[test]
fn user_config_credentials_reach_cs_and_the_fetcher() {
    if !cs_available() || cfg!(windows) {
        return;
    }
    let server = Server::maven(hello_repo_files(b"hello jar"), Some(("alice", "s3cret-pass")));
    let ws = tempfile::tempdir().unwrap();
    let config = ws.path().join("user-config.yaml");
    fs::write(
        &config,
        format!("credentials:\n  - {{url: '{}', user: alice, password: 'env:NEXUS_PASS'}}\n", server.url("/")),
    )
    .unwrap();
    let workspace = |opt_in: &str| {
        java_workspace(
            ws.path(),
            &format!("repositories:\n  - {{id: local, url: '{}'{opt_in}}}\n", server.url("/maven")),
            "dependencies:\n  com.example:hello: '1.0'\n",
        )
    };
    let (wrapper, log) = logging_cs(ws.path());
    let with_config = |cache: &Path| {
        let mut cmd = scpm(ws.path(), cache);
        cmd.env("SCPM_CONFIG", &config).env("NEXUS_PASS", "s3cret-pass").env("SCPM_CS", &wrapper);
        cmd
    };
    // Over plain HTTP they need the repository's opt-in, like coursier's own.
    workspace("");
    let cache = tempfile::tempdir().unwrap();
    let err = stderr_of(with_config(cache.path()).arg("install"));
    assert!(err.contains("401") || err.contains("nauthorized"), "{err}");
    assert!(server.requests.lock().unwrap().iter().all(|r| r.authorization.is_none()));

    workspace(", allowInsecureCredentials: true");
    with_config(cache.path()).arg("install").assert().success();
    assert!(!fs::read_to_string(&log).unwrap().contains("s3cret-pass"));
    let empty = tempfile::tempdir().unwrap();
    with_config(empty.path()).args(["install", "--frozen"]).assert().success();
    let jar = empty
        .path()
        .join(format!("http/alice%40127.0.0.1%3A{}/maven/com/example/hello/1.0/hello-1.0.jar", server.port));
    assert_eq!(fs::read(&jar).unwrap(), b"hello jar");

    // A missing variable names the user config.
    let other = tempfile::tempdir().unwrap();
    let err = stderr_of(scpm(ws.path(), other.path()).env("SCPM_CONFIG", &config).args(["install", "--frozen"]));
    assert!(err.contains("`NEXUS_PASS`") && err.contains("user config"), "{err}");
    // The file is parsed strictly: an unknown key is a configuration error.
    fs::write(&config, "mirror: []\n").unwrap();
    let out = scpm(ws.path(), other.path()).env("SCPM_CONFIG", &config).arg("install").assert().code(2);
    let err = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(err.contains("user-config.yaml") && err.contains("mirrors"), "{err}");
}

/// `files` with every path's `/maven/` replaced by `/{dir}/`.
fn moved(files: std::collections::HashMap<String, Vec<u8>>, dir: &str) -> std::collections::HashMap<String, Vec<u8>> {
    files.into_iter().map(|(p, b)| (p.replace("/maven/", &format!("/{dir}/")), b)).collect()
}

#[test]
fn one_origin_shares_its_credentials_on_both_paths() {
    if !cs_available() || cfg!(windows) {
        return;
    }
    // Only the mirror path holds the artifact, behind the same login as the private repository.
    let server = Server::maven(moved(hello_repo_files(b"hello jar"), "remote"), Some(("alice", "s3cret-pass")));
    let ws = tempfile::tempdir().unwrap();
    let private = format!(
        "  - {{id: private, url: '{}', allowInsecureCredentials: true, credentials: {{user: 'env:A_USER', password: 'env:A_PASS'}}}}\n",
        server.url("/maven")
    );
    let deps = "dependencies:\n  com.example:hello: '1.0'\n";
    let env = |cmd: &mut Command| {
        cmd.env("A_USER", "alice").env("A_PASS", "s3cret-pass").env("B_USER", "bob").env("B_PASS", "other-pass");
    };

    // Different credentials on one origin: a configuration error naming both repositories.
    let other = format!(
        "  - {{id: other, url: '{}', allowInsecureCredentials: true, credentials: {{user: 'env:B_USER', password: 'env:B_PASS'}}}}\n",
        server.url("/other")
    );
    java_workspace(ws.path(), &format!("repositories:\n{private}{other}"), deps);
    let mut cmd = scpm(ws.path(), tempfile::tempdir().unwrap().path());
    env(&mut cmd);
    let out = cmd.arg("install").assert().code(2);
    let err = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(err.contains(&server.url("/maven/")) && err.contains(&server.url("/other/")), "{err}");
    assert!(!err.contains("pass"), "{err}");

    // An anonymous mirror on that origin gets the private repository's credentials, from `cs`
    // when re-locking and from the fetcher when restoring.
    java_workspace(ws.path(), &format!("repositories:\n{private}  - central\n"), deps);
    let config = ws.path().join("user-config.yaml");
    fs::write(&config, format!("mirrors:\n  - {{url: '{}', mirrorOf: [central]}}\n", server.url("/remote"))).unwrap();
    let with_config = |cache: &Path| {
        let mut cmd = scpm(ws.path(), cache);
        env(&mut cmd);
        cmd.env("SCPM_CONFIG", &config);
        cmd
    };
    with_config(tempfile::tempdir().unwrap().path()).arg("install").assert().success();
    assert_eq!(lock(ws.path())["artifacts"]["com.example:hello:1.0"]["repo"], "central");
    let empty = tempfile::tempdir().unwrap();
    with_config(empty.path()).args(["install", "--frozen"]).assert().success();
    let jar = empty
        .path()
        .join(format!("http/alice%40127.0.0.1%3A{}/remote/com/example/hello/1.0/hello-1.0.jar", server.port));
    assert_eq!(fs::read(&jar).unwrap(), b"hello jar");
}

#[test]
fn relocking_through_a_catch_all_mirror_keeps_repository_ids() {
    if !cs_available() || cfg!(windows) {
        return;
    }
    let world: std::collections::HashMap<String, Vec<u8>> = hello_repo_files(b"world jar")
        .into_iter()
        .map(|(p, b)| {
            (p.replace("hello", "world"), String::from_utf8(b).unwrap().replace("hello", "world").into_bytes())
        })
        .collect();
    let main = Server::maven(Default::default(), None);
    let acme = Server::maven(hello_repo_files(b"hello jar"), None);
    let mut mirrored = moved(hello_repo_files(b"hello jar"), "mirror");
    mirrored.extend(moved(world, "mirror"));
    let mirror = Server::maven(mirrored, None);
    let ws = tempfile::tempdir().unwrap();
    let repos = format!(
        "repositories:\n  - {{id: main, url: '{}'}}\n  - {{id: acme, url: '{}'}}\n",
        main.url("/maven"),
        acme.url("/maven")
    );
    java_workspace(ws.path(), &repos, "dependencies:\n  com.example:hello: '1.0'\n");
    let cache = tempfile::tempdir().unwrap();
    scpm(ws.path(), cache.path()).arg("install").assert().success();
    assert_eq!(lock(ws.path())["artifacts"]["com.example:hello:1.0"]["repo"], "acme");

    // Re-locked through one mirror for both repositories: hello stays acme's, so a restore
    // without the mirror still finds it; only a new artifact gets the first repository.
    java_workspace(ws.path(), &repos, "dependencies:\n  com.example:hello: '1.0'\n  com.example:world: '1.0'\n");
    let config = ws.path().join("user-config.yaml");
    fs::write(&config, format!("mirrors:\n  - {{url: '{}', mirrorOf: ['*']}}\n", mirror.url("/mirror"))).unwrap();
    scpm(ws.path(), cache.path()).env("SCPM_CONFIG", &config).arg("install").assert().success();
    assert!(mirror.count("/mirror/com/example/world/1.0/world-1.0.pom") > 0, "re-locked through the mirror");
    let locked = lock(ws.path());
    assert_eq!(locked["artifacts"]["com.example:hello:1.0"]["repo"], "acme");
    assert_eq!(locked["artifacts"]["com.example:world:1.0"]["repo"], "main");
}

#[test]
fn mirrors_serve_both_paths_and_never_reach_the_lock() {
    if !cs_available() || cfg!(windows) {
        return;
    }
    let real = Server::maven(hello_repo_files(b"hello jar"), None);
    let mirrored: std::collections::HashMap<String, Vec<u8>> =
        hello_repo_files(b"hello jar").into_iter().map(|(p, b)| (p.replace("/maven/", "/mirror/"), b)).collect();
    let mirror = Server::maven(mirrored, Some(("alice", "mirror-pass")));
    let manifest =
        format!("repositories:\n  - {{id: local, url: '{}', allowInsecureCredentials: true}}\n", real.url("/maven"));
    let deps = "dependencies:\n  com.example:hello: '1.0'\n";
    let plain = tempfile::tempdir().unwrap();
    java_workspace(plain.path(), &manifest, deps);
    scpm(plain.path(), tempfile::tempdir().unwrap().path()).arg("install").assert().success();
    let real_requests = real.requests.lock().unwrap().len();
    assert!(real_requests > 0);

    // The same workspace through a mirror; its credentials are matched against the mirror's URL.
    let ws = tempfile::tempdir().unwrap();
    java_workspace(ws.path(), &manifest, deps);
    let config = ws.path().join("user-config.yaml");
    fs::write(
        &config,
        format!(
            "mirrors:\n  - {{url: '{m}', mirrorOf: [local]}}\ncredentials:\n  - {{url: '{m}', user: alice, password: 'env:MIRROR_PASS'}}\n",
            m = mirror.url("/mirror")
        ),
    )
    .unwrap();
    let with_mirror = |cache: &Path| {
        let mut cmd = scpm(ws.path(), cache);
        cmd.env("SCPM_CONFIG", &config).env("MIRROR_PASS", "mirror-pass");
        cmd
    };
    with_mirror(tempfile::tempdir().unwrap().path()).arg("install").assert().success();
    assert_eq!(
        fs::read(ws.path().join("scpm-lock.yaml")).unwrap(),
        fs::read(plain.path().join("scpm-lock.yaml")).unwrap(),
        "the lock is byte-identical with and without the mirror"
    );
    // A restore downloads from the mirror too, into the mirror's place in coursier's cache.
    let empty = tempfile::tempdir().unwrap();
    with_mirror(empty.path()).args(["install", "--frozen"]).assert().success();
    let jar = empty
        .path()
        .join(format!("http/alice%40127.0.0.1%3A{}/mirror/com/example/hello/1.0/hello-1.0.jar", mirror.port));
    assert_eq!(fs::read(&jar).unwrap(), b"hello jar");
    assert!(mirror.count("/mirror/com/example/hello/1.0/hello-1.0.jar") >= 2, "both paths used the mirror");
    assert_eq!(real.requests.lock().unwrap().len(), real_requests, "the repository itself is never asked");
}

#[test]
fn both_paths_use_exactly_the_resolved_proxy() {
    if !cs_available() || cfg!(windows) {
        return;
    }
    // A forward proxy for a repository whose host doesn't resolve: requests arrive in absolute form.
    let repo = "http://repo.scpm-test.invalid/maven";
    let files: std::collections::HashMap<String, Vec<u8>> = hello_repo_files(b"hello jar")
        .into_iter()
        .map(|(p, b)| (format!("http://repo.scpm-test.invalid{p}"), b))
        .collect();
    let proxy = Server::maven(files, None);
    let ws = tempfile::tempdir().unwrap();
    java_workspace(
        ws.path(),
        &format!("repositories:\n  - {{id: remote, url: '{repo}'}}\n"),
        "dependencies:\n  com.example:hello: '1.0'\n",
    );
    let config = ws.path().join("user-config.yaml");
    fs::write(&config, format!("proxy:\n  http: '127.0.0.1:{}'\n", proxy.port)).unwrap();
    let dead = "http://127.0.0.1:1";
    let jar = |cache: &Path| cache.join("http/repo.scpm-test.invalid/maven/com/example/hello/1.0/hello-1.0.jar");

    // The user config's proxy wins over the environment's, for `cs` and for the fetcher.
    let cache = tempfile::tempdir().unwrap();
    scpm(ws.path(), cache.path()).env("SCPM_CONFIG", &config).env("HTTP_PROXY", dead).arg("install").assert().success();
    let empty = tempfile::tempdir().unwrap();
    scpm(ws.path(), empty.path())
        .env("SCPM_CONFIG", &config)
        .env("http_proxy", dead)
        .args(["install", "--frozen"])
        .assert()
        .success();
    assert_eq!(fs::read(jar(empty.path())).unwrap(), b"hello jar");
    assert!(proxy.count(&format!("{repo}/com/example/hello/1.0/hello-1.0.jar")) >= 2);

    // Without a user config proxy, `HTTP_PROXY` applies to both paths.
    fs::remove_file(ws.path().join("scpm-lock.yaml")).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let proxy_url = format!("http://127.0.0.1:{}", proxy.port);
    scpm(ws.path(), cache.path()).env("HTTP_PROXY", &proxy_url).arg("install").assert().success();
    let empty = tempfile::tempdir().unwrap();
    scpm(ws.path(), empty.path()).env("HTTP_PROXY", &proxy_url).args(["install", "--frozen"]).assert().success();
    assert_eq!(fs::read(jar(empty.path())).unwrap(), b"hello jar");

    // Proxy authentication is not supported: a clear error that never repeats the password,
    // and only when something must be downloaded.
    let with_user = format!("http://alice:s3cret@127.0.0.1:{}", proxy.port);
    let err = stderr_of(
        scpm(ws.path(), tempfile::tempdir().unwrap().path())
            .env("HTTP_PROXY", &with_user)
            .args(["install", "--frozen"]),
    );
    assert!(err.contains("HTTP_PROXY") && err.contains("proxy authentication is not supported"), "{err}");
    assert!(!err.contains("s3cret"), "{err}");
    scpm(ws.path(), empty.path()).env("HTTP_PROXY", &with_user).args(["install", "--frozen"]).assert().success();
    // A `#` in the password used to end the authority early and land the value in the error.
    let hashed = format!("http://alice:pa#ss@127.0.0.1:{}", proxy.port);
    let err = stderr_of(
        scpm(ws.path(), tempfile::tempdir().unwrap().path()).env("HTTP_PROXY", &hashed).args(["install", "--frozen"]),
    );
    assert!(err.contains("proxy authentication is not supported") && !err.contains("pa#ss"), "{err}");

    // An unusable proxy for a scheme no repository uses blocks nothing, on either path.
    fs::remove_file(ws.path().join("scpm-lock.yaml")).unwrap();
    let with_socks = |cache: &Path| {
        let mut cmd = scpm(ws.path(), cache);
        cmd.env("HTTPS_PROXY", "socks5://127.0.0.1:1080").env("HTTP_PROXY", &proxy_url);
        cmd
    };
    with_socks(tempfile::tempdir().unwrap().path()).arg("install").assert().success();
    let empty = tempfile::tempdir().unwrap();
    with_socks(empty.path()).args(["install", "--frozen"]).assert().success();
    assert_eq!(fs::read(jar(empty.path())).unwrap(), b"hello jar");
}

#[test]
fn concurrent_installs_all_succeed() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("java-deps");
    let children: Vec<_> = (0..4)
        .map(|_| {
            StdCommand::new(env!("CARGO_BIN_EXE_scpm"))
                .arg("-C")
                .arg(ws.path())
                .arg("install")
                .env("COURSIER_CACHE", shared_cache())
                .env_remove("CI")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert!(lock(ws.path())["artifacts"]["org.slf4j:slf4j-api:2.0.17"]["sha256"].is_string());
    let leftovers: Vec<_> = fs::read_dir(ws.path()).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(!leftovers.iter().any(|n| n.to_string_lossy().ends_with(".tmp")), "{leftovers:?}");
}

#[test]
fn upstream_test_dependencies_do_not_relock_downstream_variants() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let util = ws.path().join("modules/util/scpm.yaml");
    let text = fs::read_to_string(&util).unwrap();
    fs::write(&util, format!("{text}testDependencies:\n  org.slf4j:slf4j-api: 2.0.17\n")).unwrap();
    let summary = json_install(ws.path(), &shared_cache(), &[]);
    assert_eq!(summary["relocked"], serde_json::json!(["util@jvm-2.13", "util@jvm-3"]));
}

#[test]
fn corrupt_or_conflicted_locks() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("java-deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let path = ws.path().join("scpm-lock.yaml");
    let good = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("<<<<<<< HEAD\n{good}=======\n{good}>>>>>>> other\n")).unwrap();

    let err = stderr_of(scpm(ws.path(), &shared_cache()).args(["install", "--frozen"]));
    assert!(err.contains("merge conflict markers"), "{err}");
    assert!(err.contains("help: resolve the merge conflict, or re-create scpm-lock.yaml locally"), "{err}");

    let out = scpm(ws.path(), &shared_cache()).arg("install").assert().success().get_output().stderr.clone();
    assert!(String::from_utf8_lossy(&out).contains("re-locking from scratch"));
    assert_eq!(fs::read_to_string(&path).unwrap(), good);

    fs::write(&path, "lockfileVersion: [1\n").unwrap();
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    assert_eq!(fs::read_to_string(&path).unwrap(), good);
}

#[test]
fn boot_entries_follow_their_inputs() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let full = strings(&lock(ws.path())["boot"]["scala-compiler@3.9.0"]);
    assert!(full.len() > 1);
    let before = lock(ws.path())["bootInputs"].clone();
    assert!(before.as_str().unwrap().starts_with("sha256:"));

    let path = ws.path().join("scpm-lock.yaml");
    let mut l: serde_yaml_ng::Value = serde_yaml_ng::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    l["boot"]["scala-compiler@3.9.0"] = serde_yaml_ng::to_value(&full[..1]).unwrap();
    fs::write(&path, serde_yaml_ng::to_string(&l).unwrap()).unwrap();
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    assert_eq!(strings(&lock(ws.path())["boot"]["scala-compiler@3.9.0"]).len(), 1, "unchanged inputs: kept");

    // A new resolution context changes bootInputs, so every boot entry is resolved again.
    let wsfile = ws.path().join("scpm-workspace.yaml");
    let text = fs::read_to_string(&wsfile).unwrap();
    fs::write(&wsfile, format!("{text}lock:\n  resolutionContext: {{jdk: '21'}}\n")).unwrap();
    let summary = json_install(ws.path(), &shared_cache(), &[]);
    assert_eq!(summary["bootRelocked"].as_array().unwrap().len(), 4, "{summary}");
    let l = lock(ws.path());
    assert_ne!(l["bootInputs"], before);
    assert_eq!(strings(&l["boot"]["scala-compiler@3.9.0"]), full);
}

#[test]
fn removed_variants_and_resolved_on() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    let first = json_install(ws.path(), &shared_cache(), &[]);
    assert!(first["resolvedOn"]["os"].is_string(), "reported when something was resolved: {first}");
    assert!(lock(ws.path()).get("resolvedOn").is_none(), "not part of the lock");
    let again = json_install(ws.path(), &shared_cache(), &[]);
    assert!(again.get("resolvedOn").is_none() && again["warnings"] == serde_json::json!([]), "{again}");

    let util = ws.path().join("modules/util/scpm.yaml");
    let text = fs::read_to_string(&util).unwrap().replace("[catalog, catalog:scala213]", "[catalog]");
    fs::write(&util, text).unwrap();
    let out = scpm(ws.path(), &shared_cache()).arg("install").assert().success().get_output().stdout.clone();
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("Lockfile updated (removed util@jvm-2.13)"), "{out}");
    let l = lock(ws.path());
    assert!(l["variants"].get("util@jvm-2.13").is_none());
    assert!(l["boot"].get("scala-compiler@2.13.18").is_none());
}

#[test]
fn verify_repairs_corrupted_cached_jars() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("java-deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let cache = tempfile::tempdir().unwrap();
    scpm(ws.path(), cache.path()).args(["install", "--frozen"]).assert().success();
    let jar = walk(cache.path()).into_iter().find(|f| f.ends_with("slf4j-api-2.0.17.jar")).unwrap();
    let mut bytes = fs::read(&jar).unwrap();
    bytes[100] ^= 0xff;
    fs::write(&jar, &bytes).unwrap();

    let summary = json_install(ws.path(), cache.path(), &["--frozen", "--verify"]);
    assert_eq!(summary["fetch"]["repaired"], 1, "{summary}");
    let sha = lock(ws.path())["artifacts"]["org.slf4j:slf4j-api:2.0.17"]["sha256"].as_str().unwrap().to_string();
    assert_eq!(scpm::fetch::sha256_file(&jar).unwrap(), sha);
}

#[test]
fn lock_is_identical_whether_built_incrementally_or_from_scratch() {
    if !cs_available() {
        return;
    }
    let scratch = copy_fixture("deps");
    scpm(scratch.path(), &shared_cache()).arg("install").assert().success();

    let incremental = copy_fixture("deps");
    let core = incremental.path().join("modules/core");
    let saved = fs::read_to_string(core.join("scpm.yaml")).unwrap();
    fs::remove_dir_all(&core).unwrap();
    scpm(incremental.path(), &shared_cache()).arg("install").assert().success();
    fs::create_dir_all(&core).unwrap();
    fs::write(core.join("scpm.yaml"), saved).unwrap();
    scpm(incremental.path(), &shared_cache()).arg("install").assert().success();

    assert_eq!(
        fs::read_to_string(incremental.path().join("scpm-lock.yaml")).unwrap(),
        fs::read_to_string(scratch.path().join("scpm-lock.yaml")).unwrap()
    );
}

#[test]
fn cs_and_scpm_share_an_empty_cache_concurrently() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("java-deps");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let empty = tempfile::tempdir().unwrap();
    let cs = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    let cs_child = StdCommand::new(cs)
        .args(["fetch", "--cache"])
        .arg(empty.path())
        .args(["org.slf4j:slf4j-api:2.0.17", "com.google.guava:guava:33.4.0-jre"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let scpm_out = StdCommand::new(env!("CARGO_BIN_EXE_scpm"))
        .arg("-C")
        .arg(ws.path())
        .args(["install", "--frozen"])
        .env("COURSIER_CACHE", empty.path())
        .env_remove("CI")
        .output()
        .unwrap();
    let cs_out = cs_child.wait_with_output().unwrap();
    assert!(scpm_out.status.success(), "{}", String::from_utf8_lossy(&scpm_out.stderr));
    assert!(cs_out.status.success(), "{}", String::from_utf8_lossy(&cs_out.stderr));

    let l = lock(ws.path());
    let files = walk(empty.path());
    for (key, name) in [
        ("org.slf4j:slf4j-api:2.0.17", "slf4j-api-2.0.17.jar"),
        ("com.google.guava:guava:33.4.0-jre", "guava-33.4.0-jre.jar"),
    ] {
        let file = files.iter().find(|f| f.ends_with(name)).unwrap();
        assert_eq!(scpm::fetch::sha256_file(file).unwrap(), l["artifacts"][key]["sha256"].as_str().unwrap());
    }
    assert!(!files.iter().any(|f| f.to_string_lossy().ends_with(".part")), "{files:?}");
}

#[test]
fn editing_an_exact_catalog_version_relocks_to_it() {
    if !cs_available() {
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let lib = "dependencies:\n  org.slf4j:slf4j-api: catalog\n";
    java_workspace(ws.path(), "catalog:\n  org.slf4j:slf4j-api: 2.0.17\n", lib);
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    assert!(
        strings(&lock(ws.path())["variants"]["lib@jvm"]["compileClasspath"])
            .contains(&"org.slf4j:slf4j-api:2.0.17".to_string())
    );

    java_workspace(ws.path(), "catalog:\n  org.slf4j:slf4j-api: 2.0.16\n", lib);
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let l = lock(ws.path());
    assert!(
        strings(&l["variants"]["lib@jvm"]["compileClasspath"]).contains(&"org.slf4j:slf4j-api:2.0.16".to_string()),
        "{l:#}"
    );
    assert_eq!(l["variants"]["lib@jvm"]["specifiers"]["org.slf4j:slf4j-api"]["version"], "2.0.16");
}

#[test]
fn pom_packaged_dependencies_resolve_to_their_dependencies() {
    if !cs_available() {
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    // groovy-all is a <packaging>pom</packaging> aggregator: no jar of its own.
    java_workspace(ws.path(), "", "dependencies:\n  org.apache.groovy:groovy-all: 4.0.24\n");
    scpm(ws.path(), &shared_cache()).arg("install").assert().success();
    let cp = strings(&lock(ws.path())["variants"]["lib@jvm"]["compileClasspath"]);
    assert!(cp.iter().any(|k| k.starts_with("org.apache.groovy:groovy:4.0.24")), "{cp:?}");
}

/// A file:// Maven repository holding `com.example:hello:1.0` with the given jar bytes.
fn file_repo(dir: &Path, jar: &[u8]) -> String {
    for (path, body) in hello_repo_files(jar) {
        let file = dir.join(path.trim_start_matches('/'));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, body).unwrap();
    }
    format!("file://{}/maven", dir.display())
}

/// Every mirror source coursier reads by itself, each sending `from` to `to` (design §5.9).
fn hostile_coursier_env(dir: &Path, from: &str, to: &str) -> Vec<(&'static str, PathBuf)> {
    let mirrors = dir.join("hostile-mirrors.properties");
    fs::write(&mirrors, format!("m.from={from}\nm.to={to}\n")).unwrap();
    let config_dir = dir.join("hostile-coursier-config");
    fs::create_dir_all(&config_dir).unwrap();
    fs::copy(&mirrors, config_dir.join("mirror.properties")).unwrap();
    let maven = dir.join("hostile-m2");
    fs::create_dir_all(&maven).unwrap();
    fs::write(
        maven.join("settings.xml"),
        format!(
            "<settings><mirrors><mirror><id>corp</id><mirrorOf>*</mirrorOf><url>{to}</url></mirror></mirrors></settings>"
        ),
    )
    .unwrap();
    vec![
        ("COURSIER_MIRRORS", mirrors.clone()),
        ("COURSIER_CONFIG_DIR", config_dir),
        ("COURSIER_EXTRA_MIRRORS", mirrors),
        ("MAVEN_HOME", maven.clone()),
        ("CS_MAVEN_HOME", maven),
    ]
}

#[test]
fn foreign_mirror_settings_do_not_redirect_cs() {
    if !cs_available() || cfg!(windows) {
        return;
    }
    let server = Server::maven(hello_repo_files(b"the real jar"), None);
    let ws = tempfile::tempdir().unwrap();
    let elsewhere = file_repo(&ws.path().join("elsewhere"), b"a hostile jar");
    let hostile = hostile_coursier_env(ws.path(), &server.url("/maven"), &elsewhere);
    // Each source on its own redirects a plain `cs`, so this test notices if one stops working.
    let cs = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    for (var, value) in &hostile {
        let cache = tempfile::tempdir().unwrap();
        let out = StdCommand::new(&cs)
            .args(["fetch", "--no-default", "-r", &server.url("/maven"), "com.example:hello:1.0", "--cache"])
            .arg(cache.path())
            .env_remove("COURSIER_MIRRORS")
            .env_remove("COURSIER_MAVEN_SETTINGS")
            .env(var, value)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("/elsewhere/"), "{var} should redirect a plain cs: {stdout}");
    }

    java_workspace(
        ws.path(),
        &format!("repositories:\n  - {{id: local, url: '{}'}}\n", server.url("/maven")),
        "dependencies:\n  com.example:hello: '1.0'\n",
    );
    let cache = tempfile::tempdir().unwrap();
    let hostile_scpm = || {
        let mut cmd = scpm(ws.path(), cache.path());
        cmd.envs(hostile.iter().map(|(k, v)| (*k, v.as_os_str())))
            .env("COURSIER_MODE", "offline")
            .env("COURSIER_TTL", "0")
            .env("COURSIER_REPOSITORIES", &elsewhere)
            // Not the developer's own ~/.m2.
            .env("HOME", ws.path());
        cmd
    };
    let out = hostile_scpm().arg("install").assert().success();
    assert_eq!(lock(ws.path())["artifacts"]["com.example:hello:1.0"]["repo"], "local");
    let jar = cache.path().join(format!("http/127.0.0.1%3A{}/maven/com/example/hello/1.0/hello-1.0.jar", server.port));
    assert_eq!(fs::read(jar).unwrap(), b"the real jar");
    assert!(server.count(HELLO_JAR_PATH) >= 1);

    // One notice names an ignored setting that touched the repository, and how to see more.
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert_eq!(stderr.matches("note:").count(), 1, "{stderr}");
    assert!(stderr.contains("~/hostile-m2/settings.xml (CS_MAVEN_HOME): mirror \"corp\" for *"), "{stderr}");
    assert!(stderr.contains("is ignored for local (and 2 more settings)"), "{stderr}");
    assert!(stderr.contains("scpm config explain"), "{stderr}");
    // Nothing contacted, no notice.
    let out = hostile_scpm().arg("install").assert().success();
    assert!(!String::from_utf8_lossy(&out.get_output().stderr).contains("note:"));
    // A failure that may be caused by an ignored setting carries the notice too.
    java_workspace(
        ws.path(),
        "repositories:\n  - {id: local, url: 'http://127.0.0.1:1/maven'}\n",
        "dependencies:\n  com.example:hello: '1.0'\n",
    );
    let err = stderr_of(hostile_scpm().arg("install"));
    assert!(err.contains("note: ") && err.contains("scpm config explain"), "{err}");
}
