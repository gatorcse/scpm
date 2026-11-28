//! `.metals/mbt.json` for Metals v2 (design §10.1).
//!
//! Metals v2's language server reads only this file, never build files. The format matches
//! scalameta/metals branch `main-v2` at commit aa5a3baca813f780b22427a36d3b63cea4890bd5
//! (2026-11-25, after v2.0.0-RC1): `docs/build-tools/mbt.schema.json` and the Gson classes it
//! is read into, `metals/src/main/scala/scala/meta/internal/metals/mbt/MbtBuild.scala`,
//! `MbtNamespace.scala`, `MbtDependencyModule.scala` and `MbtMainClass.scala`.
//!
//! - Top level: `dependencyModules` (`{id, jar, sources?}`, URIs) and `namespaces`.
//! - Each variant is one namespace keyed by its id (`core@jvm-3`), and its test sources a second
//!   one keyed `<id>:test`: Metals marks a target as a test target by that `:test` suffix
//!   (`MbtTarget.isTestTarget`) and its built-in Maven importer emits the same pair. The test
//!   namespace depends on the main one.
//! - `scalaVersion` per namespace picks the presentation compiler. The published schema doesn't
//!   list it (or `classDirectories`), but `MbtNamespace` reads both and the Maven importer
//!   writes both; without `scalaVersion`, every namespace would get Metals' fallback version.
//! - Classpaths come from the lock, as for the Maven importer: a main namespace gets the
//!   compile classpath (compile + provided); a test namespace the test compile classpath plus
//!   anything only on the test runtime classpath. Upstream workspace variants are `dependsOn`.
//!   Non-jar artifacts (`@aar`, …) are left out.
//! - The file is shared with Metals: its importers write a merged mbt.json too. The automatic
//!   refresh ([`super::refresh`]) only touches a file whose content is what scpm last wrote
//!   ([`owned`]), and never when a Metals script importer ([`importer_script`]) owns it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{
    IdeError, SourcesOptions, SourcesReport, file_uri, is_jar, sha256_hex, sources_jars, sources_module,
    write_if_changed,
};
use crate::compile::{Toolchain, classes_dir, compiler_options};
use crate::diag::slash_path;
use crate::install::{ClasspathKind, InstallError};
use crate::lockfile::ClasspathEntry;
use crate::testing::test_classes_dir;

/// Where Metals v2 reads the build from, relative to the workspace root.
pub const MBT_JSON: &str = ".metals/mbt.json";

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Write here instead of `<workspace>/.metals/mbt.json` (e.g. `$MBT_OUTPUT_FILE` from a
    /// Metals script importer).
    pub output: Option<PathBuf>,
    /// Whether to download sources jars that aren't cached yet.
    pub sources: SourcesOptions,
}

/// The sha256 of the mbt.json scpm last wrote: the install refresh only regenerates a file
/// whose content is still scpm's.
const OWNER: &str = ".scpm/ide/mbt.json.sha256";

/// Whether `.metals/mbt.json` exists and is exactly what scpm last wrote there (so Metals or
/// another importer hasn't replaced it since).
pub fn owned(root: &Path) -> bool {
    let Ok(current) = std::fs::read(root.join(MBT_JSON)) else { return false };
    std::fs::read_to_string(root.join(OWNER)).is_ok_and(|h| h.trim() == sha256_hex(&current))
}

/// A Metals v2 script importer at the workspace root (`*.mbt.sh`, `*.mbt.scala`, `*.mbt.java`,
/// `*.mbt.bat`): Metals then merges its importers' output into `.metals/mbt.json` itself.
pub fn importer_script(root: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| [".mbt.sh", ".mbt.scala", ".mbt.java", ".mbt.bat"].iter().any(|ext| n.ends_with(ext)))
        .collect();
    names.sort();
    names.into_iter().next()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    /// The file, relative to the workspace root when inside it.
    pub path: String,
    /// False when the file already had this content.
    pub written: bool,
    pub namespaces: usize,
    pub dependency_modules: usize,
    pub sources: SourcesReport,
    pub warnings: Vec<String>,
}

/// The whole file (`MbtBuild`).
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MbtBuild {
    pub dependency_modules: Vec<DependencyModule>,
    pub namespaces: BTreeMap<String, Namespace>,
}

/// `MbtDependencyModule`.
#[derive(Debug, Serialize, PartialEq)]
pub struct DependencyModule {
    pub id: String,
    /// `file:` URI of the jar.
    pub jar: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<String>,
}

/// `MbtNamespace`. `sources`, `dependencyModules` and `dependsOn` are required by the schema.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Namespace {
    /// Source directories relative to the workspace root, `/`-separated.
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scalac_options: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub javac_options: Vec<String>,
    /// Ids into the top-level `dependencyModules`, in classpath order.
    pub dependency_modules: Vec<String>,
    /// Absent for Java-only variants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scala_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_home: Option<String>,
    pub depends_on: Vec<String>,
    /// scpm's output directories (`.scpm/out/<id>/classes`), relative to the workspace root.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub class_directories: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub main_classes: Vec<MainClass>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MainClass {
    pub class_name: String,
}

/// The mbt.json id of a locked artifact: `org:name:version`, or `org:name:classifier:version`
/// as in the schema's examples (`io.netty:netty-transport-native-epoll:linux-x86_64:4.1.130.Final`);
/// a non-jar type keeps its `@type` suffix.
pub fn module_id(key: &str) -> String {
    let (coords, kind) = match key.split_once('@') {
        Some((c, t)) => (c, Some(t)),
        None => (key, None),
    };
    let parts: Vec<&str> = coords.split(':').collect();
    let id = match parts.as_slice() {
        [g, a, v, c] => format!("{g}:{a}:{c}:{v}"),
        _ => coords.to_string(),
    };
    match kind {
        Some(t) => format!("{id}@{t}"),
        None => id,
    }
}

/// Artifact keys and upstream variants of `kinds`' classpaths, merged in order, first wins.
fn classpath(entries: &[&[ClasspathEntry]]) -> (Vec<String>, Vec<String>) {
    let (mut artifacts, mut modules) = (Vec::new(), Vec::new());
    let (mut seen_a, mut seen_m) = (BTreeSet::new(), BTreeSet::new());
    for list in entries {
        for e in *list {
            match e {
                ClasspathEntry::Artifact(k) if seen_a.insert(k.clone()) => artifacts.push(k.clone()),
                ClasspathEntry::Module { module } if seen_m.insert(module.clone()) => modules.push(module.clone()),
                _ => {}
            }
        }
    }
    (artifacts, modules)
}

fn rel(root: &Path, path: &Path) -> String {
    slash_path(path.strip_prefix(root).unwrap_or(path))
}

/// Builds the mbt.json model. `sources` maps [`sources_module`] ids to sources jars.
pub fn build(
    tc: &Toolchain,
    sources: &BTreeMap<String, PathBuf>,
    java_home: Option<&Path>,
) -> Result<MbtBuild, IdeError> {
    let ws = tc.ws;
    let java_home = java_home.map(|h| h.to_string_lossy().into_owned());
    let mut modules: BTreeMap<String, DependencyModule> = BTreeMap::new();
    let mut namespaces = BTreeMap::new();
    for v in ws.variants() {
        let vlock = tc.lock.variants.get(&v.id).ok_or_else(|| InstallError::Stale(vec![v.id.clone()]))?;
        let opts = compiler_options(tc, v)?;
        let scala_version = v.scala.as_ref().map(|s| s.full.clone());
        for test in [false, true] {
            let (artifacts, upstream) = if test {
                classpath(&[ClasspathKind::TestCompile.entries(vlock), ClasspathKind::TestRuntime.entries(vlock)])
            } else {
                classpath(&[ClasspathKind::Compile.entries(vlock)])
            };
            let mut ids = Vec::new();
            for key in artifacts.iter().filter(|k| is_jar(k)) {
                let id = module_id(key);
                modules.entry(id.clone()).or_insert_with(|| DependencyModule {
                    id: id.clone(),
                    jar: file_uri(&tc.file(key)),
                    sources: sources_module(key).and_then(|m| sources.get(&m)).map(|p| file_uri(p)),
                });
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            let mut depends_on = if test { vec![v.id.clone()] } else { Vec::new() };
            depends_on.extend(upstream);
            let (dirs, out) = if test {
                (&v.test_sources, test_classes_dir(&ws.root, &v.id))
            } else {
                (&v.sources, classes_dir(&ws.root, &v.id))
            };
            let ns = Namespace {
                sources: dirs.iter().map(|d| slash_path(d)).collect(),
                scalac_options: if v.scala.is_some() { opts.scalac.clone() } else { Vec::new() },
                javac_options: opts.javac.clone(),
                dependency_modules: ids,
                scala_version: scala_version.clone(),
                java_home: java_home.clone(),
                depends_on,
                class_directories: vec![rel(&ws.root, &out)],
                main_classes: match (&v.main_class, test) {
                    (Some(c), false) => vec![MainClass { class_name: c.clone() }],
                    _ => Vec::new(),
                },
            };
            let name = if test { format!("{}:test", v.id) } else { v.id.clone() };
            namespaces.insert(name, ns);
        }
    }
    Ok(MbtBuild { dependency_modules: modules.into_values().collect(), namespaces })
}

/// Renders the file: pretty JSON with a trailing newline (stable for unchanged inputs).
pub fn render(build: &MbtBuild) -> String {
    let mut text = serde_json::to_string_pretty(build).expect("mbt.json serializes");
    text.push('\n');
    text
}

/// Generates the file, fetching missing sources jars if asked, and writes it if it changed.
pub fn generate(tc: &Toolchain, opts: &Options) -> Result<Summary, IdeError> {
    let ws = tc.ws;
    let mut warnings = Vec::new();
    let keys: BTreeSet<String> = tc
        .lock
        .variants
        .iter()
        .filter(|(id, _)| ws.variant(id).is_some())
        .flat_map(|(_, l)| [&l.compile_classpath, &l.test_compile_classpath, &l.test_runtime_classpath])
        .flatten()
        .filter_map(|e| match e {
            ClasspathEntry::Artifact(k) => Some(k.clone()),
            ClasspathEntry::Module { .. } => None,
        })
        .collect();
    let (sources, report) = sources_jars(tc, &keys, opts.sources, &mut warnings);
    let java_home = crate::worker::java_home();
    if java_home.is_none() {
        warnings.push("no JDK found (set JAVA_HOME); mbt.json has no javaHome".into());
    }
    let model = build(tc, &sources, java_home.as_deref())?;
    let path = opts.output.clone().unwrap_or_else(|| ws.root.join(MBT_JSON));
    let text = render(&model);
    let written = write_if_changed(&path, &text)?;
    if opts.output.is_none() {
        write_if_changed(&ws.root.join(OWNER), &format!("{}\n", sha256_hex(text.as_bytes())))?;
    }
    Ok(Summary {
        path: rel(&ws.root, &path),
        written,
        namespaces: model.namespaces.len(),
        dependency_modules: model.dependency_modules.len(),
        sources: report,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_ids_follow_the_schema_examples() {
        assert_eq!(module_id("org.typelevel:cats-core_3:2.13.0"), "org.typelevel:cats-core_3:2.13.0");
        assert_eq!(
            module_id("io.netty:netty-transport-native-epoll:4.1.115.Final:linux-x86_64"),
            "io.netty:netty-transport-native-epoll:linux-x86_64:4.1.115.Final"
        );
        assert_eq!(module_id("com.x:lib:1.0@aar"), "com.x:lib:1.0@aar");
    }

    #[test]
    fn classpaths_merge_in_order() {
        let a = |k: &str| ClasspathEntry::Artifact(k.into());
        let m = |k: &str| ClasspathEntry::Module { module: k.into() };
        let first = [m("u@jvm-3"), a("x"), a("y")];
        let second = [m("u@jvm-3"), m("w@jvm-3"), a("y"), a("z")];
        let (artifacts, modules) = classpath(&[&first, &second]);
        assert_eq!(artifacts, ["x", "y", "z"]);
        assert_eq!(modules, ["u@jvm-3", "w@jvm-3"]);
    }
}
