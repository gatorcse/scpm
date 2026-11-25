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

/// Runs git in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let ok = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// Makes `dir` a git repository with everything committed on `main`.
fn git_init(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Variant ids selected by `ls` with `args`, run in `dir`.
fn ls_ids(dir: &Path, args: &[&str]) -> Vec<String> {
    let mut all = vec!["ls"];
    all.extend_from_slice(args);
    json(dir, &all).as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
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
javalib@jvm    modules/javalib
util@jvm-3     modules/util
util@jvm-2.13  modules/util
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
    assert_eq!(ids(&["-F", "core^..."]), vec!["util@jvm-3", "util@jvm-2.13"]);
    assert_eq!(ids(&["-F", "...^util@jvm-3"]), vec!["cli@jvm-3", "core@jvm-3"]);
    assert_eq!(ids(&["-F", "{apps/cli}"]), vec!["cli@jvm-3"]);
    assert_eq!(ids(&["-F", "./modules/**", "-F", "!*@*-2.13", "-F", "!util"]), vec!["core@jvm-3", "javalib@jvm"]);
    assert_eq!(ids(&["-F", "*@*-2.13"]), vec!["core@jvm-2.13", "util@jvm-2.13"]);
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
    git_init(dir.path());
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

/// Asserts `needle` occurs exactly once in `haystack`.
fn once(haystack: &str, needle: &str) {
    assert_eq!(haystack.matches(needle).count(), 1, "expected `{needle}` exactly once in:\n{haystack}");
}

const CROSS_WS: &str =
    "toolchain: {scala: '3.9.0'}\nmodules: ['modules/*']\npresets:\n  cross:\n    scala: ['3.9.0', '2.13.18']\n";

#[test]
fn filter_variant_part_uses_the_selector_grammar() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", CROSS_WS),
        ("modules/core/scpm.yaml", "extends: [cross]\n"),
        ("modules/javalib/scpm.yaml", "scala: none\n"),
    ]);
    let d = dir.path();
    assert_eq!(ls_ids(d, &["-F", "core@3"]), vec!["core@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "core@2.13"]), vec!["core@jvm-2.13"]);
    assert_eq!(ls_ids(d, &["-F", "core@3.9.0"]), vec!["core@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "core@2.13.18"]), vec!["core@jvm-2.13"]);
    assert_eq!(ls_ids(d, &["-F", "core@jvm-3"]), vec!["core@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "core@jvm"]), vec!["core@jvm-3", "core@jvm-2.13"]);
    assert_eq!(ls_ids(d, &["-F", "core@jvm-*"]), vec!["core@jvm-3", "core@jvm-2.13"]);
    assert_eq!(ls_ids(d, &["-F", "*@*-2.13"]), vec!["core@jvm-2.13"]);
    // A Scala part never matches a Java-only variant; a platform part does.
    assert_eq!(ls_ids(d, &["-F", "*@3"]), vec!["core@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "*@jvm"]), vec!["core@jvm-3", "core@jvm-2.13", "javalib@jvm"]);
    for bad in ["core@foo", "core@jvm-foo", "core@jvm-", "core@3.x"] {
        scpm(d)
            .args(["ls", "-F", bad])
            .assert()
            .code(2)
            .stderr(predicate::str::contains(format!("invalid selector `{bad}`")));
    }
    scpm(d).args(["ls", "-F", "core@js"]).assert().code(2).stderr(predicate::str::contains("matches nothing"));
}

#[test]
fn filters_with_only_exclusions_start_from_everything() {
    let out = stdout(&fixture("basic"), &["ls", "-F", "!core", "-F", "!util"]);
    assert_eq!(out, "cli@jvm-3    apps/cli\njavalib@jvm  modules/javalib\n");
}

#[test]
fn dependency_and_dependent_closures_without_self() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "dependencies: {b: 'workspace:*'}\n"),
        ("modules/b/scpm.yaml", "dependencies: {c: 'workspace:*'}\n"),
        ("modules/c/scpm.yaml", "{}\n"),
        ("modules/d/scpm.yaml", "{}\n"),
    ]);
    let d = dir.path();
    assert_eq!(ls_ids(d, &["-F", "a^..."]), vec!["b@jvm-3", "c@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "a..."]), vec!["a@jvm-3", "b@jvm-3", "c@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "...^c"]), vec!["a@jvm-3", "b@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "...c"]), vec!["a@jvm-3", "b@jvm-3", "c@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "...^b", "-F", "d"]), vec!["a@jvm-3", "d@jvm-3"]);
}

#[test]
fn path_selectors_are_relative_to_the_invocation_directory() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("scpm.yaml", "name: root\n"),
        ("modules/a/scpm.yaml", "{}\n"),
        ("modules/b/scpm.yaml", "{}\n"),
    ]);
    let inside = dir.path().join("modules/a");
    assert_eq!(ls_ids(&inside, &["-F", "."]), vec!["a@jvm-3"]);
    assert_eq!(ls_ids(&inside, &["-F", "./"]), vec!["a@jvm-3"]);
    assert_eq!(ls_ids(&inside, &["-F", "../b"]), vec!["b@jvm-3"]);
    assert_eq!(ls_ids(&inside, &["-F", "{../b}"]), vec!["b@jvm-3"]);
    assert_eq!(ls_ids(&inside, &["-F", "../*"]), vec!["a@jvm-3", "b@jvm-3"]);
    assert_eq!(ls_ids(&inside, &["-F", "../.."]), vec!["root@jvm-3"]);
    let absolute = dir.path().join("modules/b");
    assert_eq!(ls_ids(&inside, &["-F", absolute.to_str().unwrap()]), vec!["b@jvm-3"]);
    assert_eq!(ls_ids(dir.path(), &["-F", "."]), vec!["root@jvm-3"]);
    assert_eq!(ls_ids(dir.path(), &["-F", "./modules/*"]), vec!["a@jvm-3", "b@jvm-3"]);
    // Relative `-C` directories work too.
    let out = Command::cargo_bin("scpm")
        .unwrap()
        .current_dir(dir.path())
        .args(["-C", "modules/b", "ls", "-F", "."])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(String::from_utf8(out).unwrap(), "b@jvm-3  modules/b\n");
}

#[test]
fn since_filter_handles_renames_non_ascii_paths_and_the_root_module() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("scpm.yaml", "name: root\ndependencies: {a: 'workspace:*'}\n"),
        ("README.md", "hi\n"),
        ("modules/a/scpm.yaml", "{}\n"),
        ("modules/a/src/main/scala/Moved.scala", "object Moved\n"),
        ("modules/b/scpm.yaml", "{}\n"),
        ("modules/ü/scpm.yaml", "name: u\n"),
        ("modules/ü/src/main/scala/Ä.scala", "object A\n"),
        ("modules/c/scpm.yaml", "{}\n"),
    ]);
    let d = dir.path();
    git_init(d);
    assert_eq!(ls_ids(d, &["-F", "[main]"]), Vec::<String>::new());
    // A rename counts for both the old and the new owner.
    fs::create_dir_all(d.join("modules/b/src/main/scala")).unwrap();
    git(d, &["mv", "modules/a/src/main/scala/Moved.scala", "modules/b/src/main/scala/Moved.scala"]);
    assert_eq!(ls_ids(d, &["-F", "[main]"]), vec!["a@jvm-3", "b@jvm-3"]);
    git(d, &["commit", "-q", "-am", "move"]);
    git(d, &["checkout", "-q", "-b", "next"]);
    git(d, &["branch", "-q", "-f", "main", "HEAD"]);
    // Non-ASCII paths, both modified and untracked.
    fs::write(d.join("modules/ü/src/main/scala/Ä.scala"), "object A2\n").unwrap();
    assert_eq!(ls_ids(d, &["-F", "[main]"]), vec!["u@jvm-3"]);
    git(d, &["checkout", "-q", "--", "."]);
    fs::write(d.join("modules/ü/src/main/scala/Ö.scala"), "object O\n").unwrap();
    assert_eq!(ls_ids(d, &["-F", "[main]"]), vec!["u@jvm-3"]);
    fs::remove_file(d.join("modules/ü/src/main/scala/Ö.scala")).unwrap();
    // Files outside every module directory belong to the root module.
    fs::write(d.join("README.md"), "changed\n").unwrap();
    assert_eq!(ls_ids(d, &["-F", "[main]"]), vec!["root@jvm-3"]);
    // ...and the root module is a dependent of `a`.
    git(d, &["checkout", "-q", "--", "."]);
    fs::write(d.join("modules/a/new.txt"), "x\n").unwrap();
    assert_eq!(ls_ids(d, &["-F", "[main]"]), vec!["a@jvm-3"]);
    assert_eq!(ls_ids(d, &["-F", "...[main]"]), vec!["a@jvm-3", "root@jvm-3"]);
}

#[test]
fn root_module_can_be_a_dependency() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("scpm.yaml", "name: base\n"),
        ("modules/a/scpm.yaml", "dependencies: {base: 'workspace:*'}\n"),
    ]);
    assert_eq!(stdout(dir.path(), &["graph"]), "base@jvm-3\na@jvm-3 → base@jvm-3\n");
    assert_eq!(ls_ids(dir.path(), &["-F", "...^base"]), vec!["a@jvm-3"]);
}

#[test]
fn duplicate_module_names_are_errors() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "name: same\n"),
        ("modules/b/scpm.yaml", "name: same\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("module name `same` is used by both modules/a and modules/b"), "{err}");
}

#[test]
fn self_dependencies_are_errors() {
    let dir =
        workspace(&[("scpm-workspace.yaml", WS), ("modules/a/scpm.yaml", "testDependencies: {a: 'workspace:*'}\n")]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "modules/a/scpm.yaml: testDependencies.a: module `a` cannot depend on itself");
}

#[test]
fn preset_variants_do_not_break_modules_without_that_variant() {
    let ws = format!(
        "{WS}presets:\n  lib:\n    variants:\n      '2.13': {{scalacOptions: [-Xsource:3]}}\n      js: {{scalacOptions: [-x]}}\n"
    );
    let dir = workspace(&[("scpm-workspace.yaml", &ws), ("modules/a/scpm.yaml", "extends: [lib]\n")]);
    assert_eq!(ls_ids(dir.path(), &[]), vec!["a@jvm-3"]);

    // Syntax is still checked, in the workspace file.
    let ws = format!("{WS}presets:\n  lib:\n    variants:\n      jvm-banana: {{scalacOptions: [-x]}}\n");
    let dir = workspace(&[("scpm-workspace.yaml", &ws), ("modules/a/scpm.yaml", "extends: [lib]\n")]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "scpm-workspace.yaml: presets.lib.variants.jvm-banana: invalid variant selector `jvm-banana`");
}

#[test]
fn module_variant_selectors_use_the_grammar() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", CROSS_WS),
        (
            "modules/a/scpm.yaml",
            "extends: [cross]\nvariants:\n  3: {scalacOptions: [-three]}\n  jvm-2.13: {scalacOptions: [-two]}\n  2.13.18: {javacOptions: [-full]}\n  jvm: {jvmOptions: [-all]}\n",
        ),
    ]);
    let v = json(dir.path(), &["config", "a"]);
    assert_eq!(v[0]["id"], "a@jvm-3");
    assert_eq!(v[0]["scalacOptions"], serde_json::json!(["-three"]));
    assert_eq!(v[0]["jvmOptions"], serde_json::json!(["-all"]));
    assert_eq!(v[1]["scalacOptions"], serde_json::json!(["-two"]));
    assert_eq!(v[1]["javacOptions"], serde_json::json!(["-full"]));

    let dir = workspace(&[
        ("scpm-workspace.yaml", CROSS_WS),
        ("modules/a/scpm.yaml", "extends: [cross]\nvariants:\n  2.13: {scalacOptions: [-x]}\n  foo: {}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("variants.2.13: unquoted variant selector"), "{err}");
    assert!(err.contains("help: quote variant selectors, e.g. '2.13'"), "{err}");
    assert!(err.contains("variants.foo: invalid variant selector `foo`"), "{err}");
}

#[test]
fn forbidden_keys_inside_variants() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "variants:\n  '3':\n    scala: '3.8.0'\n    extends: [x]\n    name: b\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    for key in ["scala", "extends", "name"] {
        once(&err, &format!("modules/a/scpm.yaml: variants.3: `{key}` cannot be set in a variant override"));
    }
    let ws = format!("{WS}presets:\n  p:\n    variants:\n      jvm: {{platforms: [jvm]}}\n");
    let dir = workspace(&[("scpm-workspace.yaml", &ws)]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "scpm-workspace.yaml: presets.p.variants.jvm: `platforms` cannot be set in a variant override");
}

#[test]
fn presets_are_applied_once() {
    let ws = format!(
        "{WS}presets:\n  base: {{scalacOptions: [-base], boms: ['a:b:1']}}\n  a: {{extends: [base], scalacOptions: [-a]}}\n  b: {{extends: [base, base], scalacOptions: [-b]}}\n"
    );
    let dir = workspace(&[("scpm-workspace.yaml", &ws), ("modules/m/scpm.yaml", "extends: [a, b, a]\n")]);
    let v = json(dir.path(), &["config", "m"]);
    assert_eq!(v[0]["scalacOptions"], serde_json::json!(["-base", "-a", "-b"]));
}

#[test]
fn empty_keys_do_not_wipe_inherited_values() {
    let ws = format!("{WS}presets:\n  lib:\n    dependencies: {{'org.foo:bar': '1.0'}}\n    scalacOptions: [-x]\n");
    let dir = workspace(&[
        ("scpm-workspace.yaml", &ws),
        ("modules/a/scpm.yaml", "extends: [lib]\ndependencies:\nscalacOptions:\nvariants:\n  '3':\n"),
    ]);
    let v = json(dir.path(), &["config", "a"]);
    assert_eq!(v[0]["dependencies"][0]["coord"], "org.foo:bar");
    assert_eq!(v[0]["scalacOptions"], serde_json::json!(["-x"]));
}

#[test]
fn integers_are_valid_versions() {
    let ws = format!("{WS}lock: {{resolutionContext: {{jdk: 25}}}}\n");
    let dir = workspace(&[("scpm-workspace.yaml", &ws), ("modules/a/scpm.yaml", "release: 17\n")]);
    let v = json(dir.path(), &["config", "a"]);
    assert_eq!(v[0]["release"], "17");
}

#[test]
fn workspace_errors_are_reported_once_in_the_workspace_file() {
    let ws = "toolchain: {scala: '3.9'}\nmodules: ['modules/*']\nboms: ['nope']\npresets:\n  x: {extends: [missing]}\n";
    let dir = workspace(&[
        ("scpm-workspace.yaml", ws),
        ("modules/a/scpm.yaml", "{}\n"),
        ("modules/b/scpm.yaml", "{}\n"),
        ("modules/c/scpm.yaml", "scala: '2.13.18'\nextends: [x]\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "scpm-workspace.yaml: toolchain.scala: `3.9` is not a full Scala version");
    once(&err, "is not a full Scala version");
    once(&err, "scpm-workspace.yaml: boms[0]: invalid coordinate `nope`");
    once(&err, "scpm-workspace.yaml: presets.x.extends: unknown preset `missing`");
    once(&err, "unknown preset");
}

#[test]
fn scala_errors_point_at_the_file_that_set_scala() {
    let ws = format!("{WS}presets:\n  old: {{scala: ['2.11.12']}}\n");
    let dir = workspace(&[
        ("scpm-workspace.yaml", &ws),
        ("modules/a/scpm.yaml", "extends: [old]\n"),
        ("modules/b/scpm.yaml", "scala: '4.0.0'\n"),
        ("modules/c/scpm.yaml", "scala: []\n"),
        ("modules/d/scpm.yaml", "scala: '3.9'\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "scpm-workspace.yaml: presets.old.scala: Scala 2.11 is not supported");
    once(&err, "Scala 2.11 is not supported");
    once(&err, "modules/b/scpm.yaml: scala: Scala 4.0 is not supported");
    once(&err, "modules/c/scpm.yaml: scala: `scala` lists no versions");
    once(&err, "modules/d/scpm.yaml: scala: `3.9` is not a full Scala version");
}

#[test]
fn cross_built_errors_are_reported_once() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", CROSS_WS),
        ("modules/a/scpm.yaml", "extends: [cross]\ndependencies: {'bad coord:x': '1.0'}\n"),
        ("modules/c/scpm.yaml", "extends: [cross]\ndependencies: {b: 'workspace:*'}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "invalid coordinate `bad coord:x`");
    once(&err, "unknown workspace module `b`");
}

#[test]
fn every_preset_is_validated_even_if_unused() {
    let ws = format!(
        "{WS}presets:\n  a: {{extends: [b]}}\n  b: {{extends: [a]}}\n  c: {{scala: banana, tasks: {{gen: {{run: x}}}}, platforms: [js]}}\n  d: {{name: d}}\n"
    );
    let dir = workspace(&[("scpm-workspace.yaml", &ws)]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "scpm-workspace.yaml: presets.a: preset cycle: a → b → a");
    once(&err, "preset cycle");
    once(&err, "scpm-workspace.yaml: presets.c.scala: `banana` is not a full Scala version");
    once(&err, "scpm-workspace.yaml: presets.c.tasks: `tasks` is not supported yet (planned for Phase 3)");
    once(
        &err,
        "scpm-workspace.yaml: presets.c.platforms: the `js` platform is not supported yet (planned for Phase 2)",
    );
    once(&err, "scpm-workspace.yaml: presets.d.name: presets cannot set `name`");
}

#[test]
fn later_phase_module_keys_say_not_supported_yet() {
    let ws = format!(
        "{WS}tasks: {{compile: {{dependsOn: ['^compile']}}}}\npolicy:\n  minimumReleaseAgeExclude: [org.foo:bar]\n  securityBypass: 'off'\n  versionConflicts: warn\n"
    );
    let dir = workspace(&[
        ("scpm-workspace.yaml", &ws),
        ("modules/a/scpm.yaml", "platforms: [jvm, native]\n"),
        ("modules/b/scpm.yaml", "tasks: {gen: {run: x}}\nvariants:\n  jvm: {generatedSources: [gen]}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    for expected in [
        "scpm-workspace.yaml: tasks: `tasks` is not supported yet (planned for Phase 3)",
        "scpm-workspace.yaml: policy.minimumReleaseAgeExclude: `policy.minimumReleaseAgeExclude` is not supported yet (planned for Phase 3)",
        "scpm-workspace.yaml: policy.securityBypass: `policy.securityBypass` is not supported yet (planned for Phase 3)",
        "scpm-workspace.yaml: policy.versionConflicts: `policy.versionConflicts` is not supported yet (planned for Phase 3)",
        "modules/a/scpm.yaml: platforms: the `native` platform is not supported yet (planned for Phase 2)",
        "modules/b/scpm.yaml: tasks: `tasks` is not supported yet (planned for Phase 3)",
        "modules/b/scpm.yaml: variants.jvm.generatedSources: `generatedSources` is not supported yet (planned for Phase 3)",
    ] {
        once(&err, expected);
    }
}

#[test]
fn unknown_yaml_tags_are_errors_and_replace_works_on_scalars() {
    let dir = workspace(&[("scpm-workspace.yaml", WS), ("modules/a/scpm.yaml", "scalacOptions: [-a, !foo -b]\n")]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("modules/a/scpm.yaml: scalacOptions[1]: unknown YAML tag `!foo`"), "{err}");

    let ws = format!("{WS}presets:\n  lib: {{release: '11', scalacOptions: [-a]}}\n");
    let dir = workspace(&[
        ("scpm-workspace.yaml", &ws),
        ("modules/a/scpm.yaml", "extends: [lib]\nrelease: !replace '17'\nscalacOptions: !replace [-b]\n"),
    ]);
    let v = json(dir.path(), &["config", "a"]);
    assert_eq!(v[0]["release"], "17");
    assert_eq!(v[0]["scalacOptions"], serde_json::json!(["-b"]));
}

#[test]
fn replace_inside_variants_entries_replaces() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "scalacOptions: [-a, -Werror]\nvariants:\n  '3': {scalacOptions: !replace [-a]}\n"),
    ]);
    let v = json(dir.path(), &["config", "a"]);
    assert_eq!(v[0]["scalacOptions"], serde_json::json!(["-a"]));
}

#[test]
fn module_discovery_ignores_gitignore() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        (".gitignore", "modules/b/\n"),
        (".ignore", "modules/c/\n"),
        ("modules/a/scpm.yaml", "{}\n"),
        ("modules/b/scpm.yaml", "{}\n"),
        ("modules/c/scpm.yaml", "{}\n"),
        ("modules/target/scpm.yaml", "{}\n"),
        ("modules/.hidden/scpm.yaml", "{}\n"),
    ]);
    git(dir.path(), &["init", "-q", "-b", "main"]);
    assert_eq!(ls_ids(dir.path(), &[]), vec!["a@jvm-3", "b@jvm-3", "c@jvm-3"]);
}

#[cfg(unix)]
#[test]
fn module_discovery_follows_symlinks_and_tolerates_loops() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", "toolchain: {scala: '3.9.0'}\nmodules: ['modules/*', 'linked/**']\n"),
        ("modules/a/scpm.yaml", "{}\n"),
        ("elsewhere/b/scpm.yaml", "{}\n"),
    ]);
    std::os::unix::fs::symlink(dir.path().join("elsewhere/b"), dir.path().join("modules/b")).unwrap();
    fs::create_dir_all(dir.path().join("linked")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("linked"), dir.path().join("linked/loop")).unwrap();
    assert_eq!(ls_ids(dir.path(), &[]), vec!["a@jvm-3", "b@jvm-3"]);
}

#[test]
fn module_name_errors_show_the_value() {
    let dir = workspace(&[("scpm-workspace.yaml", WS), ("modules/My_Mod/scpm.yaml", "{}\n")]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(
        err.contains("modules/My_Mod/scpm.yaml: invalid module name `My_Mod` (taken from the directory name)"),
        "{err}"
    );

    let dir = workspace(&[("scpm-workspace.yaml", WS), ("modules/a/scpm.yaml", "name: Bad Name\n")]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("modules/a/scpm.yaml: name: invalid module name `Bad Name`"), "{err}");
}

#[test]
fn source_directories_may_point_anywhere_inside_the_workspace() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "sources: [src, ../shared, ../../common]\ntestResources: [./res]\n"),
    ]);
    let v = json(dir.path(), &["config", "a"]);
    assert_eq!(v[0]["sources"], serde_json::json!(["modules/a/src", "modules/shared", "common"]));
    assert_eq!(v[0]["testResources"], serde_json::json!(["modules/a/res"]));

    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "sources: [src, ../../../outside]\ntestResources: [/etc]\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "modules/a/scpm.yaml: sources[1]: `../../../outside` is outside the workspace");
    once(&err, "modules/a/scpm.yaml: testResources[0]: `/etc` is an absolute path");

    // A preset's directories are checked against each module that uses it.
    let ws = format!("{WS}presets:\n  lib: {{resources: ['../res']}}\n");
    let dir = workspace(&[
        ("scpm-workspace.yaml", &ws),
        ("scpm.yaml", "name: root\nextends: [lib]\n"),
        ("modules/a/scpm.yaml", "extends: [lib]\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "scpm.yaml: resources: `../res` (from a preset) is outside the workspace for this module");
    assert!(!err.contains("modules/a/scpm.yaml"), "{err}");
}

#[test]
fn graph_errors_point_at_modules() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "dependencies: {b: 'workspace:*'}\n"),
        ("modules/b/scpm.yaml", "runtimeDependencies: {a: 'workspace:*'}\n"),
    ]);
    let err = config_error(dir.path(), &["graph"]);
    assert!(err.contains("modules/a/scpm.yaml: dependency cycle: a@jvm-3 → b@jvm-3 → a@jvm-3"), "{err}");

    let dir = workspace(&[
        ("scpm-workspace.yaml", CROSS_WS),
        ("modules/a/scpm.yaml", "extends: [cross]\nprovidedDependencies: {b: 'workspace:*'}\n"),
        ("modules/b/scpm.yaml", "{}\n"),
    ]);
    let err = config_error(dir.path(), &["graph"]);
    assert!(
        err.contains(
            "modules/a/scpm.yaml: providedDependencies.b: a@jvm-2.13 depends on `b`, which has no matching variant"
        ),
        "{err}"
    );
}

#[test]
fn modules_that_fail_to_load_are_not_unknown_to_their_dependents() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/b/scpm.yaml", "platforms: [js]\n"),
        ("modules/a/scpm.yaml", "dependencies: {b: 'workspace:*'}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "modules/b/scpm.yaml: platforms: the `js` platform is not supported yet (planned for Phase 2)");
    assert!(!err.contains("unknown workspace module"), "{err}");

    // The name comes from `name:` when the file has one, even if the file doesn't validate.
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/b/scpm.yaml", "name: lib\nscalacOptoins: [-x]\n"),
        ("modules/a/scpm.yaml", "dependencies: {lib: 'workspace:*'}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "modules/b/scpm.yaml");
    assert!(!err.contains("unknown workspace module"), "{err}");
}

#[test]
fn unknown_workspace_modules_never_suggest_the_depending_module() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/core/scpm.yaml", "dependencies: {cor: 'workspace:*'}\n"),
        ("modules/util/scpm.yaml", "{}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "modules/core/scpm.yaml: dependencies.cor: unknown workspace module `cor`");
    assert!(!err.contains("did you mean"), "{err}");
}

#[test]
fn scpm_version_pin_is_not_supported_yet() {
    let ws = format!("scpm: 0.4.2\n{WS}");
    let dir = workspace(&[("scpm-workspace.yaml", &ws), ("modules/a/scpm.yaml", "{}\n")]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "scpm-workspace.yaml: scpm: `scpm` is not supported yet (planned for Phase 3)");
}

#[test]
fn ivy_dynamic_revisions_are_ranges() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", WS),
        ("modules/a/scpm.yaml", "dependencies: {'org.foo:bar': 1.2.+, 'org.foo:baz': 1.2.3}\n"),
    ]);
    let v = json(dir.path(), &["config", "a"]);
    let deps = &v[0]["dependencies"];
    assert_eq!(deps[0]["version"], serde_json::json!({"kind": "range", "range": "1.2.+"}));
    assert_eq!(deps[1]["version"], serde_json::json!({"kind": "exact", "version": "1.2.3"}));
}

#[test]
fn double_bang_tags_are_errors() {
    let ws = format!("{WS}presets:\n  lib: {{scalacOptions: [-a, -Werror]}}\n");
    let dir = workspace(&[
        ("scpm-workspace.yaml", &ws),
        ("modules/a/scpm.yaml", "extends: [lib]\nscalacOptions: !!replace [-a]\nrelease: !!str 17\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(
        &err,
        "modules/a/scpm.yaml: scalacOptions: unknown YAML tag `!!replace`\n  help: write `!replace` with a single `!`",
    );
    once(&err, "modules/a/scpm.yaml: release: unknown YAML tag `!!str`");

    // Also when the file starts with a document marker.
    let dir =
        workspace(&[("scpm-workspace.yaml", &ws), ("modules/a/scpm.yaml", "---\nscalacOptions: !!replace [-a]\n")]);
    let err = config_error(dir.path(), &["ls"]);
    once(&err, "modules/a/scpm.yaml: scalacOptions: unknown YAML tag `!!replace`");
}

#[test]
fn lone_scala_majors_other_than_3_are_invalid_selectors() {
    let dir = workspace(&[
        ("scpm-workspace.yaml", CROSS_WS),
        ("modules/core/scpm.yaml", "extends: [cross]\nvariants:\n  '2': {scalacOptions: [-x]}\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    once(
        &err,
        "modules/core/scpm.yaml: variants.2: invalid variant selector `2`: `2` is not a Scala binary version\n  help: use a binary version such as 2.12 or 2.13",
    );

    let dir = workspace(&[("scpm-workspace.yaml", CROSS_WS), ("modules/core/scpm.yaml", "extends: [cross]\n")]);
    scpm(dir.path()).args(["ls", "-F", "core@2"]).assert().code(2).stderr(predicate::str::contains(
        "invalid selector `core@2`: `2` is not a Scala binary version; use a binary version such as 2.12 or 2.13",
    ));
    assert_eq!(ls_ids(dir.path(), &["-F", "core@3"]), vec!["core@jvm-3"]);
}

#[test]
fn failed_replace_fallback_reports_the_positioned_error() {
    let ws = format!("{WS}presets:\n  lib: {{release: '11'}}\n");
    let dir = workspace(&[
        ("scpm-workspace.yaml", &ws),
        ("modules/a/scpm.yaml", "extends: [lib]\nmainClass: [x]\nrelease: !replace '17'\n"),
    ]);
    let err = config_error(dir.path(), &["ls"]);
    assert!(err.contains("modules/a/scpm.yaml:2:12: mainClass: "), "{err}");
}

#[cfg(unix)]
#[test]
fn module_discovery_walks_only_the_glob_prefixes() {
    let dir = workspace(&[("scpm-workspace.yaml", WS), ("modules/a/scpm.yaml", "{}\n")]);
    // Outside `modules/`: a tree of symlinks that takes 8^8 steps to walk with links followed.
    let fan = dir.path().join("fan");
    for level in 0..=8 {
        fs::create_dir_all(fan.join(format!("d{level}"))).unwrap();
    }
    for level in 0..8 {
        for i in 0..8 {
            let link = fan.join(format!("d{level}/l{i}"));
            std::os::unix::fs::symlink(fan.join(format!("d{}", level + 1)), link).unwrap();
        }
    }
    scpm(dir.path())
        .args(["ls"])
        .timeout(std::time::Duration::from_secs(20))
        .assert()
        .success()
        .stdout("a@jvm-3  modules/a\n");
}
