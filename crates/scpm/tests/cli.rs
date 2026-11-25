//! End-to-end tests of the `scpm` binary against fixture workspaces.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn scpm(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("scpm").unwrap();
    cmd.arg("-C").arg(dir);
    cmd
}

fn stdout(dir: &Path, args: &[&str]) -> String {
    let out = scpm(dir).args(args).assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

fn json(dir: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    serde_json::from_str(&stdout(dir, &all)).unwrap()
}

/// Writes `files` (path, contents) into a fresh temporary directory.
fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    dir
}

/// Runs scpm expecting a configuration error; returns stderr.
fn config_error(dir: &Path, args: &[&str]) -> String {
    let out = scpm(dir).args(args).assert().code(2).get_output().stderr.clone();
    String::from_utf8(out).unwrap()
}

const WS: &str = "toolchain: {scala: '3.9.0'}\nmodules: ['modules/*']\n";

#[test]
fn ls_lists_variants() {
    let out = stdout(&fixture("basic"), &["ls"]);
    assert_eq!(
        out,
        "\
cli@jvm-3      apps/cli
core@jvm-3     modules/core
core@jvm-2.13  modules/core
core@js-3      modules/core
core@js-2.13   modules/core
javalib@jvm    modules/javalib
util@jvm-3     modules/util
util@jvm-2.13  modules/util
util@js-3      modules/util
util@js-2.13   modules/util
"
    );
}

#[test]
fn ls_works_from_a_module_subdirectory() {
    let out = stdout(&fixture("basic").join("modules/core/src"), &["ls", "-F", "javalib"]);
    assert_eq!(out, "javalib@jvm  modules/javalib\n");
}

#[test]
fn graph_text_is_dependency_ordered() {
    let out = stdout(&fixture("basic"), &["graph", "-F", "cli..."]);
    assert_eq!(
        out,
        "\
javalib@jvm
util@jvm-3
core@jvm-3 → util@jvm-3
cli@jvm-3 → core@jvm-3, javalib@jvm, util@jvm-3 (test)
"
    );
}

#[test]
fn graph_dot_marks_test_edges() {
    let out = stdout(&fixture("basic"), &["graph", "--format", "dot", "-F", "cli..."]);
    assert!(out.contains("\"cli@jvm-3\" -> \"util@jvm-3\" [style=dashed];"), "{out}");
}

#[test]
fn config_applies_presets_catalogs_and_variant_overrides() {
    let v = json(&fixture("basic"), &["config", "core@jvm-2.13"]);
    let core = &v[0];
    assert_eq!(core["id"], "core@jvm-2.13");
    assert_eq!(core["scala"]["full"], "2.13.18");
    // strict extends library: library's options first, then strict's, then the variant's.
    assert_eq!(core["scalacOptions"], serde_json::json!(["-deprecation", "-feature", "-Werror", "-Xsource:3"]));
    let deps = core["dependencies"].as_array().unwrap();
    let find = |coord: &str| deps.iter().find(|d| d["coord"] == coord).unwrap_or_else(|| panic!("{coord} missing"));
    assert_eq!(find("org.typelevel::cats-core")["version"], serde_json::json!({"kind": "exact", "version": "2.12.0"}));
    assert_eq!(find("org.typelevel::cats-core")["spec"], "catalog:scala213");
    assert_eq!(find("io.circe::circe-core")["version"], serde_json::json!({"kind": "range", "range": "^0.14.10"}));
    assert_eq!(find("com.google.guava:guava")["exclude"], serde_json::json!(["com.google.code.findbugs:jsr305"]));
    assert_eq!(find("org.scalameta::munit")["scope"], "test");
    assert_eq!(core["workspaceDependencies"], serde_json::json!([{"scope": "compile", "module": "util"}]));
    assert!(
        core["sources"].as_array().unwrap().contains(&Value::from("modules/core/src/main/scala-2.13")),
        "existing version-specific source dirs are added: {}",
        core["sources"]
    );
    assert_eq!(core["publish"], true);
    assert_eq!(core["versionScheme"], "early-semver");

    let scala3 = json(&fixture("basic"), &["config", "core@jvm-3"]);
    assert_eq!(scala3[0]["scalacOptions"], serde_json::json!(["-deprecation", "-feature", "-Werror"]));
}

#[test]
fn java_only_modules_have_no_scala() {
    let v = json(&fixture("basic"), &["config", "javalib"]);
    assert_eq!(v[0]["id"], "javalib@jvm");
    assert_eq!(v[0]["scala"], Value::Null);
    assert_eq!(v[0]["javacOptions"], serde_json::json!(["-Xlint:all"]));
}

#[test]
fn filters_follow_the_graph() {
    let ids = |args: &[&str]| -> Vec<String> {
        let mut all = vec!["ls"];
        all.extend_from_slice(args);
        json(&fixture("basic"), &all)
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&["-F", "core^..."]), vec!["util@jvm-3", "util@jvm-2.13", "util@js-3", "util@js-2.13"]);
    assert_eq!(ids(&["-F", "...^util@jvm-3"]), vec!["cli@jvm-3", "core@jvm-3"]);
    assert_eq!(ids(&["-F", "{apps/cli}"]), vec!["cli@jvm-3"]);
    assert_eq!(
        ids(&["-F", "./modules/**", "-F", "!*@js-*", "-F", "!util"]),
        vec!["core@jvm-3", "core@jvm-2.13", "javalib@jvm"]
    );
    assert_eq!(ids(&["-F", "*@*-2.13"]).len(), 4);
}

#[test]
fn unmatched_filter_is_a_usage_error() {
    scpm(&fixture("basic"))
        .args(["ls", "-F", "nope"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("matches nothing"));
}

#[test]
fn single_module_project_without_workspace_file() {
    let dir = workspace(&[("scpm.yaml", "name: solo\nscala: '2.13.18'\n")]);
    assert_eq!(stdout(dir.path(), &["ls"]), "solo@jvm-2.13  .\n");
}

#[test]
fn since_filter_uses_git() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "{}\n"),
        ("modules/b/scpm.yaml", "dependencies: {a: 'workspace:*'}\n"),
        ("modules/c/scpm.yaml", "{}\n"),
    ]);
    let git = |args: &[&str]| {
        let ok = StdCommand::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    fs::create_dir_all(dir.path().join("modules/a/src/main/scala")).unwrap();
    fs::write(dir.path().join("modules/a/src/main/scala/A.scala"), "object A\n").unwrap();
    assert_eq!(stdout(dir.path(), &["ls", "-F", "[main]"]), "a@jvm-3  modules/a\n");
    assert_eq!(stdout(dir.path(), &["ls", "-F", "...[main]"]), "a@jvm-3  modules/a\nb@jvm-3  modules/b\n");
}

#[test]
fn unknown_key_suggests_the_right_one() {
    let dir =
        workspace(&[("scpm-workspace.yaml", WS), ("modules/a/scpm.yaml", "dependecies:\n  org.foo:bar: '1.0'\n")]);
    let err = config_error(dir.path(), &["ls"]);
    assert_eq!(
        err.trim(),
        "error: modules/a/scpm.yaml:1:1: unknown key `dependecies`\n  help: did you mean `dependencies`?"
    );
}

#[test]
fn json_errors_are_structured() {
    let dir = workspace(&[("scpm-workspace.yaml", WS), ("modules/a/scpm.yaml", "platforms: [jmv]\n")]);
    let out = scpm(dir.path()).args(["--json", "ls"]).assert().code(2).get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["ok"], false);
    let d = &v["error"]["diagnostics"][0];
    assert_eq!(d["file"], "modules/a/scpm.yaml");
    assert_eq!(d["key"], "platforms[0]");
    assert_eq!(d["message"], "unknown value `jmv`");
    assert_eq!(d["help"], "did you mean `jvm`?");
}

#[test]
fn unquoted_float_versions_are_rejected() {
    let dir =
        workspace(&[("scpm-workspace.yaml", WS), ("modules/a/scpm.yaml", "dependencies:\n  org.foo:bar: 2.10\n")]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("modules/a/scpm.yaml:2:3: dependencies.org.foo:bar: versions must be quoted strings: YAML reads this unquoted number as `2.1`"), "{err}");
}

#[test]
fn reports_every_broken_module_at_once() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "extends: [libary]\n"),
        ("modules/b/scpm.yaml", "dependencies: {c: 'workspace:*'}\n"),
        ("modules/d/scpm.yaml", "scala: none\ndependencies: {'org.typelevel::cats-core': '2.13.0'}\n"),
        ("modules/e/scpm.yaml", "variants: {'2.12': {scalacOptions: [-x]}}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    for expected in [
        "modules/a/scpm.yaml: extends: unknown preset `libary`",
        "modules/b/scpm.yaml: dependencies.c: unknown workspace module `c`",
        "modules/d/scpm.yaml: dependencies.org.typelevel::cats-core: `org.typelevel::cats-core` needs a Scala version",
        "modules/e/scpm.yaml: variants.2.12: variant selector `2.12` matches no variant",
    ] {
        assert!(err.contains(expected), "missing `{expected}` in:\n{err}");
    }
}

#[test]
fn catalog_misses_point_to_other_catalogs() {
    let ws = "toolchain: {scala: '3.9.0'}\nmodules: ['modules/*']\ncatalogs:\n  spark:\n    org.apache.spark::spark-sql: 3.5.1\n";
    let dir = workspace(&[
        ("scpm-workspace.yaml", ws),
        ("modules/a/scpm.yaml", "dependencies: {'org.apache.spark::spark-sql': catalog}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(
        err.contains("`org.apache.spark::spark-sql` is not in the default catalog\n  help: found in: catalog:spark"),
        "{err}"
    );
}

#[test]
fn compile_scope_cycles_are_errors() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "dependencies: {b: 'workspace:*'}\n"),
        ("modules/b/scpm.yaml", "runtimeDependencies: {a: 'workspace:*'}\n"),
    ]);
    let err = config_error(dir.path(), &["graph"]);
    assert!(err.contains("dependency cycle: a@jvm-3 → b@jvm-3 → a@jvm-3"), "{err}");
}

#[test]
fn preset_cycles_are_errors() {
    let ws = format!("{WS}presets:\n  x: {{extends: [y]}}\n  y: {{extends: [x]}}\n");
    let dir = workspace(&[("scpm-workspace.yaml", &ws), ("modules/a/scpm.yaml", "extends: [x]\n")]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("preset cycle: x → y → x"), "{err}");
}

#[test]
fn replace_tag_and_matrix_keys() {
    let ws = format!("{WS}presets:\n  base:\n    scala: ['3.9.0', '2.13.18']\n    scalacOptions: [-a, -Werror]\n");
    let dir = workspace(&[
        ("scpm-workspace.yaml", &ws),
        ("modules/a/scpm.yaml", "extends: [base]\nscala: '2.13.18'\nscalacOptions: !replace [-a]\n"),
    ]);
    let v = json(dir.path(), &["config", "a"]);
    assert_eq!(v.as_array().unwrap().len(), 1, "module scala replaces the preset's list");
    assert_eq!(v[0]["scalacOptions"], serde_json::json!(["-a"]));
}

#[test]
fn later_phase_keys_say_not_supported_yet() {
    let ws = format!("{WS}pipelines:\n  default: [compile]\n");
    let dir = workspace(&[("scpm-workspace.yaml", &ws)]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("`pipelines` is not supported yet (planned for Phase 3)"), "{err}");
}

#[test]
fn no_workspace_found() {
    let dir = tempfile::tempdir().unwrap();
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("no scpm-workspace.yaml or scpm.yaml found"), "{err}");
}

#[test]
fn committed_schemas_are_up_to_date() {
    for (kind, file) in [("workspace", "workspace.json"), ("module", "module.json")] {
        let generated = stdout(&fixture("basic"), &["schema", kind]);
        let committed = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema").join(file))
            .unwrap_or_default();
        assert!(
            generated == committed,
            "schema/{file} is stale; regenerate with `cargo run -- schema {kind} > schema/{file}`"
        );
    }
}
