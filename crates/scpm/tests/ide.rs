//! `scpm ide metals` (`.metals/mbt.json`) and `scpm idea` (IntelliJ project files) on the
//! fixture workspaces. They resolve with the native coursier CLI and fetch sources jars, so they
//! need `cs` and network access, and are skipped with a message when `cs` is missing.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::Value;

mod shared_daemon;

fn cs_available() -> bool {
    let bin = std::env::var("SCPM_CS").unwrap_or_else(|_| "cs".into());
    let ok = StdCommand::new(bin).arg("version").output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: coursier CLI `cs` not found");
    }
    ok
}

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

fn scpm(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("scpm").unwrap();
    cmd.arg("-C").arg(dir).env("COURSIER_CACHE", shared_cache()).env_remove("CI").env_remove("COURSIER_CREDENTIALS");
    cmd
}

fn run_json(dir: &Path, args: &[&str]) -> Value {
    let out = scpm(dir).arg("--json").args(args).assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap()
}

fn stderr(dir: &Path, args: &[&str]) -> String {
    let out = scpm(dir).args(args).assert().success().get_output().stderr.clone();
    String::from_utf8(out).unwrap()
}

fn mbt(dir: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(dir.join(".metals/mbt.json")).unwrap()).unwrap()
}

fn sorted(v: &Value) -> Vec<String> {
    let mut s = strings(v);
    s.sort();
    s
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect()).unwrap_or_default()
}

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The shape every generated mbt.json must have (Metals v2 `MbtBuild`): each namespace has the
/// schema's required keys, references only known dependency modules and namespaces, and every
/// jar (and sources jar) is a `file:` URI of an existing file.
fn check_mbt_shape(m: &Value) {
    let modules = m["dependencyModules"].as_array().unwrap();
    let ids: BTreeSet<&str> = modules.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), modules.len(), "dependency module ids are unique");
    for d in modules {
        for key in ["jar", "sources"] {
            if let Some(uri) = d[key].as_str() {
                let path = uri.strip_prefix("file://").unwrap_or_else(|| panic!("{key} is a file URI: {uri}"));
                assert!(Path::new(path).is_file(), "{uri} exists");
            }
        }
        assert!(d.as_object().unwrap().keys().all(|k| ["id", "jar", "sources"].contains(&k.as_str())));
    }
    let namespaces = m["namespaces"].as_object().unwrap();
    for (name, ns) in namespaces {
        for key in ["sources", "dependencyModules", "dependsOn"] {
            assert!(ns[key].is_array(), "{name}.{key} is required");
        }
        for id in strings(&ns["dependencyModules"]) {
            assert!(ids.contains(id.as_str()), "{name} references unknown module {id}");
        }
        for dep in strings(&ns["dependsOn"]) {
            assert!(namespaces.contains_key(&dep), "{name} depends on unknown namespace {dep}");
        }
        for s in strings(&ns["sources"]) {
            assert!(!s.starts_with('/') && !s.contains('\\'), "sources are workspace-relative: {s}");
        }
    }
}

#[test]
fn mbt_json_for_the_app_fixture() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("app");
    let summary = run_json(ws.path(), &["ide", "metals"]);
    assert_eq!(summary["path"], ".metals/mbt.json");
    assert_eq!(summary["written"], true);
    let m = mbt(ws.path());
    check_mbt_shape(&m);
    let ns = &m["namespaces"];
    let names: Vec<&String> = ns.as_object().unwrap().keys().collect();
    assert_eq!(
        names,
        [
            "app@jvm-3",
            "app@jvm-3:test",
            "javalib@jvm",
            "javalib@jvm:test",
            "util@jvm-2.13",
            "util@jvm-2.13:test",
            "util@jvm-3",
            "util@jvm-3:test"
        ],
        "a namespace per variant plus a `:test` one, as Metals' Maven importer does"
    );

    let app = &ns["app@jvm-3"];
    assert_eq!(app["scalaVersion"], "3.9.0");
    assert_eq!(sorted(&app["dependsOn"]), ["javalib@jvm", "util@jvm-3"]);
    assert_eq!(strings(&app["sources"]), ["modules/app/src/main/scala", "modules/app/src/main/java"]);
    assert_eq!(app["mainClasses"], serde_json::json!([{"className": "app.Main"}]));
    assert_eq!(strings(&app["classDirectories"]), [".scpm/out/app@jvm-3/classes"]);
    assert!(app["javaHome"].as_str().is_some_and(|h| Path::new(h).join("bin").exists()), "{app}");
    let app_deps = strings(&app["dependencyModules"]);
    assert!(app_deps.contains(&"org.typelevel:cats-core_3:2.13.0".to_string()), "{app_deps:?}");
    assert!(!app_deps.iter().any(|d| d.contains("munit")), "test dependencies stay out of the main namespace");

    let app_test = &ns["app@jvm-3:test"];
    assert_eq!(strings(&app_test["dependsOn"])[0], "app@jvm-3", "a test namespace depends on its main one");
    assert_eq!(sorted(&app_test["dependsOn"]), ["app@jvm-3", "javalib@jvm", "util@jvm-3"]);
    assert!(strings(&app_test["dependencyModules"]).contains(&"org.scalameta:munit_3:1.2.1".to_string()));
    assert!(app_test.get("mainClasses").is_none());

    let util213 = &ns["util@jvm-2.13"];
    assert_eq!(util213["scalaVersion"], "2.13.18");
    assert!(strings(&util213["sources"]).contains(&"modules/util/src/main/scala-2.13".to_string()));
    assert!(strings(&util213["dependencyModules"]).contains(&"org.typelevel:cats-core_2.13:2.13.0".to_string()));
    let javalib = &ns["javalib@jvm"];
    assert!(javalib.get("scalaVersion").is_none(), "Java-only variants have no Scala version");
    assert!(javalib.get("scalacOptions").is_none());

    // Sources jars are fetched on demand and are not recorded in the lock.
    let cats =
        m["dependencyModules"].as_array().unwrap().iter().find(|d| d["id"] == "org.typelevel:cats-core_3:2.13.0");
    assert!(cats.unwrap()["sources"].as_str().unwrap().ends_with("cats-core_3-2.13.0-sources.jar"));
    assert!(!read(ws.path(), "scpm-lock.yaml").contains("sources"), "the lock has no sources jars");
}

#[test]
fn mbt_json_options_and_classpaths_for_basic_deps_and_java_deps() {
    if !cs_available() {
        return;
    }
    let basic = copy_fixture("basic");
    scpm(basic.path()).args(["ide", "metals"]).assert().success();
    let m = mbt(basic.path());
    check_mbt_shape(&m);
    let core213 = &m["namespaces"]["core@jvm-2.13"];
    assert_eq!(strings(&core213["scalacOptions"]), ["-deprecation", "-feature", "-Werror", "-Xsource:3"]);
    assert_eq!(strings(&core213["dependsOn"]), ["util@jvm-2.13"]);
    let javalib = &m["namespaces"]["javalib@jvm"];
    assert_eq!(strings(&javalib["javacOptions"]), ["-Xlint:all"]);
    assert!(strings(&javalib["dependencyModules"]).contains(&"org.slf4j:slf4j-api:2.0.17".to_string()));
    // Upstream variants are transitive, as on the locked classpath.
    assert_eq!(sorted(&m["namespaces"]["cli@jvm-3"]["dependsOn"]), ["core@jvm-3", "javalib@jvm", "util@jvm-3"]);

    let deps = copy_fixture("deps");
    scpm(deps.path()).args(["ide", "metals"]).assert().success();
    let m = mbt(deps.path());
    check_mbt_shape(&m);
    let core = strings(&m["namespaces"]["core@jvm-3"]["dependencyModules"]);
    assert!(core.iter().any(|d| d.starts_with("com.google.guava:guava:33.4.0-jre")), "{core:?}");
    assert!(!core.iter().any(|d| d.starts_with("com.google.code.findbugs:jsr305")), "exclusions hold");
    // The classpath is the lock's: every locked compile artifact is there, in order.
    let lock: serde_yaml_ng::Value = serde_yaml_ng::from_str(&read(deps.path(), "scpm-lock.yaml")).unwrap();
    let locked: Vec<String> = lock["variants"]["core@jvm-3"]["compileClasspath"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|e| e.as_str().map(str::to_string))
        .collect();
    assert_eq!(core, locked);

    let java = copy_fixture("java-deps");
    scpm(java.path()).args(["ide", "metals"]).assert().success();
    let m = mbt(java.path());
    check_mbt_shape(&m);
    assert_eq!(strings(&m["namespaces"]["javadeps@jvm"]["sources"]), ["src/main/scala", "src/main/java"]);
}

#[test]
fn mbt_json_regeneration_is_a_noop_when_nothing_changed() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("app");
    scpm(ws.path()).args(["install"]).assert().success();
    let lock = read(ws.path(), "scpm-lock.yaml");
    scpm(ws.path()).args(["ide", "metals", "--no-sources"]).assert().success();
    let path = ws.path().join(".metals/mbt.json");
    let before = fs::read_to_string(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
    }
    std::thread::sleep(std::time::Duration::from_millis(20));

    assert_eq!(run_json(ws.path(), &["ide", "metals", "--no-sources"])["written"], false);
    // A non-frozen install regenerates scpm's mbt.json, but skips the write when unchanged.
    let err = stderr(ws.path(), &["install"]);
    assert!(!err.contains("mbt.json"), "{err}");
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified, "the file is not rewritten");
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    scpm(ws.path()).args(["idea", "--no-sources"]).assert().success();
    scpm(ws.path()).arg("install").assert().success();
    assert_eq!(read(ws.path(), "scpm-lock.yaml"), lock, "IDE generation never changes the lock");

    // A config change that doesn't re-lock anything still reaches mbt.json on install.
    let util = ws.path().join("modules/util/scpm.yaml");
    let mut text = fs::read_to_string(&util).unwrap();
    text.push_str("scalacOptions: [-deprecation]\n");
    fs::write(&util, text).unwrap();
    let err = stderr(ws.path(), &["install"]);
    assert!(err.contains("updated .metals/mbt.json"), "{err}");
    assert_eq!(strings(&mbt(ws.path())["namespaces"]["util@jvm-3"]["scalacOptions"]), ["-deprecation"]);
    // Frozen installs never touch IDE files.
    fs::remove_file(&path).unwrap();
    fs::write(&path, "{}").unwrap();
    scpm(ws.path()).args(["install", "--frozen"]).assert().success();
    assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
}

/// The wrapper logs every `cs` invocation to the returned file.
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
fn compile_refreshes_ide_files_without_starting_cs() {
    if !cs_available() || cfg!(windows) {
        return;
    }
    let ws = copy_fixture("app");
    let dir = ws.path();
    scpm(dir).args(["ide", "metals", "--no-sources"]).assert().success();
    scpm(dir).args(["idea", "--no-sources"]).assert().success();
    let lock = read(dir, "scpm-lock.yaml");
    // scalacOptions don't re-lock anything, but they reach the IDE files through `compile`.
    let util = dir.join("modules/util/scpm.yaml");
    fs::write(&util, format!("{}scalacOptions: [-deprecation]\n", read(dir, "modules/util/scpm.yaml"))).unwrap();
    let (wrapper, log) = logging_cs(dir);
    let out = scpm(dir)
        .env("SCPM_CS", &wrapper)
        .envs(shared_daemon::env())
        .args(["compile", "javalib"])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("updated"), "{err}");
    assert_eq!(strings(&mbt(dir)["namespaces"]["util@jvm-3"]["scalacOptions"]), ["-deprecation"]);
    assert!(read(dir, ".idea/scala_compiler.xml").contains("<parameter value=\"-deprecation\" />"));
    assert!(!log.exists(), "no cs process: {}", fs::read_to_string(&log).unwrap_or_default());
    assert_eq!(read(dir, "scpm-lock.yaml"), lock);
}

#[test]
fn install_leaves_an_mbt_json_it_did_not_write_alone() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    let dir = ws.path();
    let append = |rel: &str, text: &str| fs::write(dir.join(rel), format!("{}{text}", read(dir, rel))).unwrap();
    scpm(dir).args(["ide", "metals", "--no-sources"]).assert().success();
    // Metals merged several importers' output into the file: it isn't scpm's any more.
    let path = dir.join(".metals/mbt.json");
    fs::write(&path, "{\"namespaces\": {\"other\": {}}}\n").unwrap();
    append("modules/util/scpm.yaml", "javacOptions: [-Xlint:all]\n");
    scpm(dir).arg("install").assert().success();
    assert_eq!(read(dir, ".metals/mbt.json"), "{\"namespaces\": {\"other\": {}}}\n");
    // The explicit command takes it back, and then install maintains it again.
    scpm(dir).args(["ide", "metals", "--no-sources"]).assert().success();
    let ours = read(dir, ".metals/mbt.json");
    append("modules/util/scpm.yaml", "scalacOptions: [-deprecation]\n");
    scpm(dir).arg("install").assert().success();
    assert_ne!(read(dir, ".metals/mbt.json"), ours);

    // With a Metals importer script, Metals writes the file: install doesn't, even when enabled.
    fs::write(dir.join("scpm.mbt.sh"), "exec scpm ide metals --output \"$MBT_OUTPUT_FILE\"\n").unwrap();
    append("scpm-workspace.yaml", "ide: {metals: true}\n");
    let before = read(dir, ".metals/mbt.json");
    append("modules/core/scpm.yaml", "mainClass: x.Main\n");
    let err = stderr(dir, &["install"]);
    assert!(err.contains("Metals runs the importer `scpm.mbt.sh`"), "{err}");
    assert_eq!(read(dir, ".metals/mbt.json"), before);
}

#[test]
fn idea_leaves_intellij_files_it_did_not_write_alone() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("app");
    let dir = ws.path();
    fs::create_dir_all(dir.join(".idea/modules")).unwrap();
    let compiler = "<project version=\"4\">\n  <component name=\"ScalaCompilerConfiguration\">\n    <option name=\"incrementalityType\" value=\"IDEA\" />\n  </component>\n</project>\n";
    fs::write(dir.join(".idea/scala_compiler.xml"), compiler).unwrap();
    let util = "<module type=\"JAVA_MODULE\" version=\"4\" />\n";
    fs::write(dir.join(".idea/modules/util.iml"), util).unwrap();
    let out = scpm(dir).args(["idea", "--no-sources"]).assert().success().get_output().clone();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("left 2 IntelliJ files that scpm didn't generate alone"), "{err}");
    assert!(err.contains(".idea/modules/util.iml") && err.contains("--force"), "{err}");
    assert_eq!(read(dir, ".idea/scala_compiler.xml"), compiler);
    assert_eq!(read(dir, ".idea/modules/util.iml"), util);
    assert!(read(dir, ".idea/modules/app.iml").contains("Generated by scpm"));
    let manifest = read(dir, ".scpm/idea-out/generated");
    assert!(!manifest.contains("util.iml") && !manifest.contains("scala_compiler.xml"), "{manifest}");
    // The refresh after a no-op install rewrites nothing, so it doesn't repeat the warning.
    let out = scpm(dir).arg("install").assert().success().get_output().clone();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(!err.contains("didn't generate alone"), "{err}");

    // A file scpm wrote stays scpm's even after IntelliJ re-saves it without the comment.
    let app = dir.join(".idea/modules/app.iml");
    fs::write(&app, read(dir, ".idea/modules/app.iml").replace("Generated by scpm", "x")).unwrap();
    scpm(dir).args(["idea", "--no-sources"]).assert().success();
    assert!(read(dir, ".idea/modules/app.iml").contains("Generated by scpm"));

    scpm(dir).args(["idea", "--no-sources", "--force"]).assert().success();
    assert!(read(dir, ".idea/scala_compiler.xml").contains("Generated by scpm"));
    assert!(read(dir, ".idea/modules/util.iml").contains("Generated by scpm"));
}

#[test]
fn ide_metals_setting_generates_on_install() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("deps");
    let file = ws.path().join("scpm-workspace.yaml");
    let mut text = fs::read_to_string(&file).unwrap();
    scpm(ws.path()).arg("install").assert().success();
    assert!(!ws.path().join(".metals/mbt.json").exists(), "not generated unless asked");
    text.push_str("ide:\n  metals: true\n");
    fs::write(&file, text).unwrap();
    scpm(ws.path()).arg("install").assert().success();
    check_mbt_shape(&mbt(ws.path()));
}

#[test]
fn metals_output_flag_writes_elsewhere() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("java-deps");
    let out = ws.path().join("out/mbt-scpm.json");
    scpm(ws.path()).args(["ide", "metals", "--output"]).arg(&out).assert().success();
    assert!(out.is_file());
    assert!(!ws.path().join(".metals/mbt.json").exists());
}

#[test]
fn idea_project_files_for_the_app_fixture() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("app");
    let dir = ws.path();
    // Unrelated IntelliJ files and modules are left alone.
    fs::create_dir_all(dir.join(".idea")).unwrap();
    fs::write(dir.join(".idea/workspace.xml"), "<project version=\"4\" />\n").unwrap();
    run_json(dir, &["idea"]);
    // IntelliJ re-saves modules.xml with a module of its own (and without scpm's comment): the
    // file is still scpm's (it is in the manifest), and the extra module is kept.
    let saved = read(dir, ".idea/modules.xml").replace(
        "<modules>\n",
        "<modules>\n      <module fileurl=\"file://$PROJECT_DIR$/tools/tools.iml\" filepath=\"$PROJECT_DIR$/tools/tools.iml\" />\n",
    );
    fs::write(dir.join(".idea/modules.xml"), saved.replace("Generated by scpm", "")).unwrap();
    let summary = run_json(dir, &["idea"]);
    assert_eq!(
        summary["modules"],
        serde_json::json!({"app": "app@jvm-3", "javalib": "javalib@jvm", "util": "util@jvm-3"})
    );
    assert_eq!(read(dir, ".idea/workspace.xml"), "<project version=\"4\" />\n");

    let modules = read(dir, ".idea/modules.xml");
    for m in ["app", "javalib", "util"] {
        assert!(modules.contains(&format!("filepath=\"$PROJECT_DIR$/.idea/modules/{m}.iml\"")), "{modules}");
    }
    assert!(modules.contains("$PROJECT_DIR$/tools/tools.iml"), "other modules are kept: {modules}");
    assert!(modules.contains("Generated by scpm"));

    let app = read(dir, ".idea/modules/app.iml");
    assert!(app.contains("Generated by scpm"));
    assert!(app.contains("<output url=\"file://$MODULE_DIR$/../../.scpm/idea-out/app@jvm-3/classes\" />"), "{app}");
    assert!(app.contains("<output-test url=\"file://$MODULE_DIR$/../../.scpm/idea-out/app@jvm-3/test-classes\" />"));
    assert!(app.contains("<content url=\"file://$MODULE_DIR$/../../modules/app\">"));
    assert!(app.contains("url=\"file://$MODULE_DIR$/../../modules/app/src/main/scala\" isTestSource=\"false\""));
    assert!(app.contains("url=\"file://$MODULE_DIR$/../../modules/app/src/test/scala\" isTestSource=\"true\""));
    assert!(app.contains("<orderEntry type=\"library\" name=\"scpm: scala-sdk-3.9.0\" level=\"project\" />"));
    assert!(
        app.contains(
            "<orderEntry type=\"library\" name=\"scpm: org.typelevel:cats-core_3:2.13.0\" level=\"project\" />"
        )
    );
    assert!(
        app.contains("<orderEntry type=\"library\" scope=\"TEST\" name=\"scpm: org.scalameta:munit_3:1.2.1\""),
        "{app}"
    );
    assert!(app.contains("<orderEntry type=\"module\" module-name=\"util\" />"));
    assert!(app.contains("<orderEntry type=\"module\" module-name=\"javalib\" />"));

    let util = read(dir, ".idea/modules/util.iml");
    assert!(util.contains("modules/util/src/main/scala-3\""), "the primary variant (first Scala version)");
    assert!(!util.contains("scala-2.13\""));
    let javalib = read(dir, ".idea/modules/javalib.iml");
    assert!(!javalib.contains("scala-sdk"), "Java-only modules get no Scala SDK");

    let sdk = read(dir, ".idea/libraries/scpm__scala_sdk_3_9_0.xml");
    assert!(sdk.contains("<library name=\"scpm: scala-sdk-3.9.0\" type=\"Scala\">"));
    assert!(sdk.contains("<language-level>Scala_3_9</language-level>"));
    assert!(sdk.contains("<compiler-classpath>") && sdk.contains("scala3-compiler_3-3.9.0.jar\" />"), "{sdk}");
    let cats = read(dir, ".idea/libraries/scpm__org_typelevel_cats_core_3_2_13_0.xml");
    assert!(cats.contains("<library name=\"scpm: org.typelevel:cats-core_3:2.13.0\">"));
    assert!(cats.contains("cats-core_3-2.13.0.jar!/\" />") && cats.contains("cats-core_3-2.13.0-sources.jar!/\" />"));

    let misc = read(dir, ".idea/misc.xml");
    assert!(misc.contains("project-jdk-type=\"JavaSDK\"") && misc.contains("<component name=\"ProjectRootManager\""));
    assert!(read(dir, ".idea/scala_compiler.xml").contains("modules=\"app,util\""));

    // Unchanged inputs write nothing; a user's own misc.xml is never replaced.
    fs::write(dir.join(".idea/misc.xml"), "<project version=\"4\" />\n").unwrap();
    let again = run_json(dir, &["idea"]);
    assert_eq!(again["written"], serde_json::json!([]), "{again}");
    assert_eq!(read(dir, ".idea/misc.xml"), "<project version=\"4\" />\n");

    // A module that goes away loses its generated file, and install keeps the files fresh.
    fs::remove_dir_all(dir.join("modules/javalib")).unwrap();
    let app_yaml = dir.join("modules/app/scpm.yaml");
    fs::write(&app_yaml, read(dir, "modules/app/scpm.yaml").replace("  javalib: workspace:*\n", "")).unwrap();
    let err = stderr(dir, &["install"]);
    assert!(err.contains("updated"), "{err}");
    assert!(!dir.join(".idea/modules/javalib.iml").exists());
    assert!(!read(dir, ".idea/modules.xml").contains("javalib.iml"));
    assert!(read(dir, ".idea/modules.xml").contains("tools/tools.iml"));
}

#[test]
fn idea_primary_variant_and_scopes() {
    if !cs_available() {
        return;
    }
    let ws = copy_fixture("app");
    let dir = ws.path();
    let util = dir.join("modules/util/scpm.yaml");
    fs::write(&util, format!("{}ide:\n  primary: '2.13'\n", read(dir, "modules/util/scpm.yaml"))).unwrap();
    let javalib = dir.join("modules/javalib/scpm.yaml");
    fs::write(
        &javalib,
        "scala: none\nprovidedDependencies:\n  org.slf4j:slf4j-api: 2.0.17\nruntimeDependencies:\n  com.google.code.findbugs:jsr305: 3.0.2\n",
    )
    .unwrap();
    let out = scpm(dir).arg("idea").assert().success().get_output().clone();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("util@jvm-3 is not the IntelliJ module of `util`"), "{err}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("util: util@jvm-2.13"), "{stdout}");

    let util_iml = read(dir, ".idea/modules/util.iml");
    assert!(util_iml.contains("modules/util/src/main/scala-2.13\""));
    assert!(util_iml.contains("name=\"scpm: scala-sdk-2.13.18\""));
    let sdk = read(dir, ".idea/libraries/scpm__scala_sdk_2_13_18.xml");
    assert!(sdk.contains("<language-level>Scala_2_13</language-level>") && sdk.contains("scala-compiler-2.13.18.jar"));

    // app@jvm-3 needs util@jvm-3, which isn't an IntelliJ module: scpm's output stands in.
    let app = read(dir, ".idea/modules/app.iml");
    assert!(!app.contains("module-name=\"util\""), "{app}");
    assert!(app.contains("<orderEntry type=\"module-library\">"));
    assert!(app.contains("$MODULE_DIR$/../../.scpm/out/util@jvm-3/classes"));
    // Its resources are a separate root, as on scpm's runtime classpath.
    assert!(
        app.contains(
            "<root url=\"file://$MODULE_DIR$/../../.scpm/out/util@jvm-3/classes\" />\n          <root url=\"file://$MODULE_DIR$/../../modules/util/src/main/resources\" />"
        ),
        "{app}"
    );

    let j = read(dir, ".idea/modules/javalib.iml");
    assert!(j.contains("scope=\"PROVIDED\" name=\"scpm: org.slf4j:slf4j-api:2.0.17\""), "{j}");
    assert!(j.contains("scope=\"RUNTIME\" name=\"scpm: com.google.code.findbugs:jsr305:3.0.2\""), "{j}");
}
