//! Loading a workspace: discover modules, validate files, expand presets, resolve catalogs
//! and compute each module's variants (design §4).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use indexmap::IndexMap;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_yaml_ng::Value;

use crate::config::{
    DepSpec, MODULE_FILE, ModuleFile, OneOrMany, Repository, TaskDef, VersionScheme, WORKSPACE_FILE, WorkspaceFile,
};
use crate::coord::{Coord, CrossKind, VersionedCoord};
use crate::diag::{Diagnostic, Diagnostics, closest, ser_opt_path, ser_path, ser_paths, slash_path};
use crate::merge::{
    has_replace_tags, merge_module, parse_with_visible_tags, strip_replace_tags, unknown_tags, untag_replace,
};
use crate::platform::Platform;
use crate::selector::{VariantSelector, full_version_parts};
use crate::version::{Ver, VersionSpec};

/// A loaded, fully resolved workspace.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    #[serde(serialize_with = "ser_path")]
    pub root: PathBuf,
    /// `None` for a lone `scpm.yaml` without a workspace file.
    #[serde(serialize_with = "ser_opt_path")]
    pub workspace_file: Option<PathBuf>,
    #[serde(skip)]
    pub config: WorkspaceFile,
    /// Sorted by name.
    pub modules: Vec<Module>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Module {
    pub name: String,
    /// Directory relative to the workspace root (`.` for the root module), `/`-separated.
    pub dir: String,
    pub variants: Vec<Variant>,
}

/// One buildable unit: a module on one platform with one Scala version.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Variant {
    /// `core@jvm-3`, or `core@jvm` for a Java-only module.
    pub id: String,
    pub module: String,
    pub platform: Platform,
    /// `None` for Java-only modules (`scala: none`).
    pub scala: Option<ScalaVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub dependencies: Vec<Dependency>,
    pub workspace_dependencies: Vec<WorkspaceDependency>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub compiler_plugins: Vec<Dependency>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub boms: Vec<VersionedCoord>,
    pub scalac_options: Vec<String>,
    pub javac_options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_class: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jvm_options: Vec<String>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    /// Paths relative to the workspace root.
    #[serde(serialize_with = "ser_paths")]
    pub sources: Vec<PathBuf>,
    #[serde(serialize_with = "ser_paths")]
    pub resources: Vec<PathBuf>,
    #[serde(serialize_with = "ser_paths")]
    pub test_sources: Vec<PathBuf>,
    #[serde(serialize_with = "ser_paths")]
    pub test_resources: Vec<PathBuf>,
    /// `.scpm/out/<variant-id>/`, relative to the workspace root.
    #[serde(serialize_with = "ser_path")]
    pub out_dir: PathBuf,
    #[serde(skip_serializing_if = "Vec::is_empty", serialize_with = "ser_paths")]
    pub generated_sources: Vec<PathBuf>,
    pub publish: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_scheme: Option<VersionScheme>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub tasks: IndexMap<String, TaskDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScalaVersion {
    pub full: String,
    pub binary: String,
}

impl ScalaVersion {
    /// Parses a full Scala version, `major.minor.patch[-qualifier]`, for Scala 2.12, 2.13 or 3.
    pub fn parse(full: &str) -> Result<ScalaVersion, String> {
        let Some((major, minor, _, _)) = full_version_parts(full) else {
            return Err(format!(
                "`{full}` is not a full Scala version; expected `major.minor.patch[-qualifier]`, e.g. `3.9.0`"
            ));
        };
        let binary = match (major, minor) {
            ("3", _) => "3".to_string(),
            ("2", "12" | "13") => format!("2.{minor}"),
            _ => return Err(format!("Scala {major}.{minor} is not supported; use Scala 2.12, 2.13 or 3")),
        };
        Ok(ScalaVersion { full: full.to_string(), binary })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    Compile,
    Provided,
    Runtime,
    Test,
}

impl Scope {
    /// The manifest key listing dependencies of this scope.
    pub fn key(self) -> &'static str {
        match self {
            Scope::Compile => "dependencies",
            Scope::Provided => "providedDependencies",
            Scope::Runtime => "runtimeDependencies",
            Scope::Test => "testDependencies",
        }
    }
}

/// An external (Maven) dependency.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependency {
    pub scope: Scope,
    pub coord: Coord,
    /// As written, e.g. `catalog:`.
    pub spec: String,
    /// After catalog lookup: an exact version or a range.
    pub version: VersionSpec,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub transitive: bool,
}

/// A dependency on another module in the workspace (`util: workspace:*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDependency {
    pub scope: Scope,
    pub module: String,
}

impl Workspace {
    pub fn variants(&self) -> impl Iterator<Item = &Variant> {
        self.modules.iter().flat_map(|m| m.variants.iter())
    }

    pub fn variant(&self, id: &str) -> Option<&Variant> {
        self.variants().find(|v| v.id == id)
    }

    pub fn module(&self, name: &str) -> Option<&Module> {
        self.modules.iter().find(|m| m.name == name)
    }

    /// Repositories in resolution order; `[central]` when none are configured.
    pub fn repositories(&self) -> Vec<Repository> {
        if self.config.repositories.is_empty() {
            vec![Repository::Named("central".into())]
        } else {
            self.config.repositories.clone()
        }
    }
}

/// Finds the workspace containing `start` and loads it.
pub fn load(start: &Path) -> Result<Workspace, Diagnostics> {
    let (root, workspace_file) = find_root(start)?;
    load_at(&root, workspace_file)
}

/// Walks up from `start` to the nearest `scpm-workspace.yaml`; falls back to the nearest
/// `scpm.yaml` (a single-module project).
pub fn find_root(start: &Path) -> Result<(PathBuf, Option<PathBuf>), Diagnostics> {
    let start = start.canonicalize().map_err(|e| Diagnostic::new(format!("cannot access {}: {e}", start.display())))?;
    let mut nearest_module = None;
    for dir in start.ancestors() {
        if dir.join(WORKSPACE_FILE).is_file() {
            return Ok((dir.to_path_buf(), Some(dir.join(WORKSPACE_FILE))));
        }
        if nearest_module.is_none() && dir.join(MODULE_FILE).is_file() {
            nearest_module = Some(dir.to_path_buf());
        }
    }
    nearest_module.map(|dir| (dir, None)).ok_or_else(|| {
        Diagnostic::new(format!("no {WORKSPACE_FILE} or {MODULE_FILE} found in {} or any parent", start.display()))
            .with_help("run scpm from inside a workspace, or pass -C <dir>")
            .into()
    })
}

fn load_at(root: &Path, workspace_file: Option<PathBuf>) -> Result<Workspace, Diagnostics> {
    let mut diags = Diagnostics::default();
    let (config, raw) = match &workspace_file {
        Some(path) => parse_file::<WorkspaceFile>(root, path)?,
        None => (WorkspaceFile::default(), Value::Mapping(Default::default())),
    };
    let mut ctx = Ctx {
        root,
        config: &config,
        presets: IndexMap::new(),
        ws_file: workspace_file.as_ref().map(|p| rel(root, p)),
        boms: Vec::new(),
    };
    if let Some(Value::Mapping(m)) = raw.get("presets") {
        for (key, value) in m {
            let Some(name) = key.as_str() else { continue };
            let mut value = value.clone();
            for d in normalize_variant_keys(&mut value, &format!("presets.{name}.")) {
                diags.push(ctx.in_ws(d));
            }
            ctx.presets.insert(name.to_string(), value);
        }
    }
    // Workspace BOMs apply to every module: parse them once.
    for (i, bom) in config.boms.iter().enumerate() {
        match VersionedCoord::parse(bom) {
            Ok(b) => ctx.boms.push(b),
            Err(e) => diags.push(ctx.in_ws(Diagnostic::new(e.to_string()).at_key(format!("boms[{i}]")))),
        }
    }
    validate_workspace(&ctx, &mut diags);

    let mut modules = Vec::new();
    // Modules that failed to load: dependents get no extra "unknown workspace module" error.
    let mut failed = BTreeSet::new();
    for dir in discover_modules(root, &config, ctx.ws_file.as_deref(), &mut diags) {
        match load_module(&ctx, &dir) {
            Ok(m) => modules.push(m),
            Err((name, d)) => {
                failed.insert(name);
                diags.0.extend(d.0);
            }
        }
    }
    modules.sort_by(|a, b| a.name.cmp(&b.name));
    for pair in modules.windows(2) {
        if pair[0].name == pair[1].name {
            diags.push(
                Diagnostic::new(format!(
                    "module name `{}` is used by both {} and {}",
                    pair[0].name, pair[0].dir, pair[1].dir
                ))
                .with_help("set a unique `name:` in one of the scpm.yaml files"),
            );
        }
    }
    let ws = Workspace { root: root.to_path_buf(), workspace_file, config, modules };
    validate_workspace_deps(&ws, &failed, &mut diags);
    diags.dedup();
    diags.into_result(ws)
}

struct Ctx<'a> {
    root: &'a Path,
    config: &'a WorkspaceFile,
    /// Raw preset values for merging, with variant keys normalized.
    presets: IndexMap<String, Value>,
    ws_file: Option<PathBuf>,
    /// Parsed workspace `boms`.
    boms: Vec<VersionedCoord>,
}

impl Ctx<'_> {
    /// Points `d` at the workspace file.
    fn in_ws(&self, d: Diagnostic) -> Diagnostic {
        match &self.ws_file {
            Some(f) => d.in_file(f.clone()),
            None => d,
        }
    }
}

/// Parses a YAML file into `T` (for validation with positions) and into a raw value (for merging).
fn parse_file<T: DeserializeOwned>(root: &Path, path: &Path) -> Result<(T, Value), Diagnostics> {
    let display = rel(root, path);
    let text = fs::read_to_string(path)
        .map_err(|e| Diagnostic::new(format!("cannot read file: {e}")).in_file(display.clone()))?;
    let raw: Value = serde_yaml_ng::from_str(&text).map_err(|e| Diagnostic::from_yaml_syntax(&display, e))?;
    let raw = if raw.is_null() { Value::Mapping(Default::default()) } else { raw };
    let mut diags = Diagnostics::default();
    // The plain parse drops `!!` tags, so look for tags in a parse that keeps them.
    let visible = if text.contains("!!") { parse_with_visible_tags(&text) } else { None };
    unknown_tags(visible.as_ref().unwrap_or(&raw), "", &mut |key, tag| {
        let help = if tag == "!!replace" {
            "write `!replace` with a single `!`"
        } else {
            "the only supported tag is `!replace`"
        };
        let d = Diagnostic::new(format!("unknown YAML tag `{tag}`")).in_file(display.clone()).with_help(help);
        diags.push(if key.is_empty() { d } else { d.at_key(key) });
    });
    if !diags.is_empty() {
        return Err(diags);
    }
    let de = serde_yaml_ng::Deserializer::from_str(&text);
    let typed = match serde_path_to_error::deserialize::<_, T>(de) {
        Ok(typed) => typed,
        // `!replace` on a scalar (`release: !replace '17'`) reads as an enum in the typed parse;
        // validate the untagged value instead. If that fails too, report the fallback's error
        // (the original one may just be the `!replace` scalar), borrowing the original error's
        // position when both point at the same key.
        Err(e) if has_replace_tags(&raw) => {
            let mut untagged = strip_replace_tags(raw.clone());
            // The text parse reads an unquoted `variants` key (`3:`) as a string; a `Value`
            // holds a number, so normalize the copy first.
            let key_diags = normalize_all_variant_keys(&mut untagged);
            if !key_diags.is_empty() {
                return Err(Diagnostics(key_diags.into_iter().map(|d| d.in_file(display.clone())).collect()));
            }
            match serde_path_to_error::deserialize::<_, T>(untagged) {
                Ok(typed) => typed,
                Err(fallback) => {
                    let original = Diagnostic::from_yaml(&display, e);
                    let mut d = Diagnostic::from_value_error(&display, fallback);
                    if d.key == original.key {
                        (d.line, d.column) = (original.line, original.column);
                    }
                    return Err(d.into());
                }
            }
        }
        Err(e) => return Err(Diagnostic::from_yaml(&display, e).into()),
    };
    Ok((typed, raw))
}

fn rel(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).map(Path::to_path_buf).unwrap_or_else(|_| path.to_path_buf())
}

/// YAML reads unquoted `variants` keys such as `3` or `2.13` as numbers. Integers are turned
/// into strings; floats are rejected because `2.10` would silently become `2.1`.
fn normalize_variant_keys(module: &mut Value, prefix: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let Some(variants) = module.get_mut("variants") else { return diags };
    // `variants: !replace {...}`: normalize the mapping under the tag.
    let variants = match variants {
        Value::Tagged(tagged) => &mut tagged.value,
        other => other,
    };
    let Value::Mapping(variants) = variants else { return diags };
    for (key, value) in std::mem::take(variants) {
        let key = match key {
            Value::Number(n) if n.is_f64() => {
                diags.push(
                    Diagnostic::new(format!("unquoted variant selector: YAML reads it as the number `{n}`"))
                        .at_key(format!("{prefix}variants.{n}"))
                        .with_help("quote variant selectors, e.g. '2.13'"),
                );
                continue;
            }
            Value::Number(n) => Value::String(n.to_string()),
            other => other,
        };
        if variants.contains_key(&key) {
            let shown = key.as_str().unwrap_or("?");
            diags.push(
                Diagnostic::new(format!("variant selector `{shown}` is written twice"))
                    .at_key(format!("{prefix}variants.{shown}"))
                    .with_help(format!("`{shown}` and '{shown}' are the same selector; merge the two entries")),
            );
            continue;
        }
        variants.insert(key, value);
    }
    diags
}

/// [`normalize_variant_keys`] for a whole file: a module file's own `variants`, and those of
/// each preset in a workspace file.
fn normalize_all_variant_keys(file: &mut Value) -> Vec<Diagnostic> {
    let mut diags = normalize_variant_keys(file, "");
    if let Some(Value::Mapping(presets)) = file.get_mut("presets") {
        for (name, preset) in presets.iter_mut() {
            let prefix = format!("presets.{}.", name.as_str().unwrap_or("?"));
            diags.extend(normalize_variant_keys(preset, &prefix));
        }
    }
    diags
}

fn not_supported(key: impl Into<String>, what: &str, phase: u8) -> Diagnostic {
    Diagnostic::new(format!("{what} is not supported yet (planned for Phase {phase})"))
        .at_key(key)
        .with_help("remove it for now; see docs/roadmap.md")
}

fn unknown_preset(name: &str, known: &[&str]) -> Diagnostic {
    let help = match closest(name, known.iter().copied()) {
        Some(b) => format!("did you mean `{b}`?"),
        _ if known.is_empty() => "no presets are defined in the workspace file".to_string(),
        _ => format!("defined presets: {}", known.join(", ")),
    };
    Diagnostic::new(format!("unknown preset `{name}`")).with_help(help)
}

fn validate_workspace(ctx: &Ctx, diags: &mut Diagnostics) {
    let config = ctx.config;
    for (key, version) in &config.overrides {
        if let Err(e) = Coord::parse(key) {
            diags.push(ctx.in_ws(Diagnostic::new(e.to_string()).at_key(format!("overrides.{key}"))));
        }
        if !matches!(VersionSpec::parse(version.as_str()), VersionSpec::Exact { .. }) {
            diags.push(
                ctx.in_ws(Diagnostic::new("overrides must be exact versions").at_key(format!("overrides.{key}"))),
            );
        }
    }
    if config.catalogs.contains_key("default") {
        diags.push(
            ctx.in_ws(
                Diagnostic::new("`default` is reserved for the default catalog")
                    .at_key("catalogs.default")
                    .with_help("put these entries under `catalog:`"),
            ),
        );
    }
    for (name, catalog) in std::iter::once((&String::new(), &config.catalog)).chain(&config.catalogs) {
        for (key, version) in catalog {
            let at = if name.is_empty() { format!("catalog.{key}") } else { format!("catalogs.{name}.{key}") };
            if key != "scala"
                && let Err(e) = Coord::parse(key)
            {
                diags.push(ctx.in_ws(Diagnostic::new(e.to_string()).at_key(at.clone())));
            }
            if matches!(
                VersionSpec::parse(version.as_str()),
                VersionSpec::Catalog { .. } | VersionSpec::Workspace { .. }
            ) {
                diags.push(ctx.in_ws(Diagnostic::new("catalog entries must be versions or ranges").at_key(at)));
            }
        }
    }
    if let Some(scala) = &config.toolchain.scala
        && let Err(d) = resolve_scala_versions(ctx, std::slice::from_ref(scala))
    {
        diags.push(ctx.in_ws(d.at_key("toolchain.scala")));
    }
    let policy = &config.policy;
    let unsupported = [
        (config.scpm.is_some(), "scpm", 3),
        (!config.tasks.is_empty(), "tasks", 3),
        (!config.pipelines.is_empty(), "pipelines", 3),
        (config.pipeline_base.is_some(), "pipelineBase", 3),
        (config.versioning.is_some(), "versioning", 4),
        (policy.minimum_release_age.is_some(), "policy.minimumReleaseAge", 3),
        (!policy.minimum_release_age_exclude.is_empty(), "policy.minimumReleaseAgeExclude", 3),
        (policy.security_bypass.is_some(), "policy.securityBypass", 3),
        (policy.version_conflicts.is_some(), "policy.versionConflicts", 3),
    ];
    for (used, key, phase) in unsupported {
        if used {
            diags.push(ctx.in_ws(not_supported(key, &format!("`{key}`"), phase)));
        }
    }

    // Every preset is validated once here, whether or not a module uses it.
    let names: Vec<&str> = config.presets.keys().map(String::as_str).collect();
    for (name, preset) in &config.presets {
        let prefix = format!("presets.{name}.");
        for d in module_checks(preset, &prefix, Kind::Preset, None) {
            diags.push(ctx.in_ws(d));
        }
        let others: Vec<&str> = names.iter().copied().filter(|n| n != name).collect();
        for parent in &preset.extends {
            if !config.presets.contains_key(parent) {
                diags.push(ctx.in_ws(unknown_preset(parent, &others).at_key(format!("{prefix}extends"))));
            }
        }
        if let Some(scala) = &preset.scala
            && let Err(d) = resolve_scala_versions(ctx, &scala.clone().into_vec())
        {
            diags.push(ctx.in_ws(d.at_key(format!("{prefix}scala"))));
        }
    }
    for cycle in preset_cycles(&config.presets) {
        diags.push(ctx.in_ws(
            Diagnostic::new(format!("preset cycle: {}", cycle.join(" → "))).at_key(format!("presets.{}", cycle[0])),
        ));
    }
}

/// Cycles in presets' `extends`, each reported once (a coloured depth-first search).
fn preset_cycles(presets: &IndexMap<String, ModuleFile>) -> Vec<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        InProgress,
        Done,
    }
    fn visit<'a>(
        name: &'a str,
        presets: &'a IndexMap<String, ModuleFile>,
        colour: &mut HashMap<&'a str, Colour>,
        stack: &mut Vec<&'a str>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        colour.insert(name, Colour::InProgress);
        stack.push(name);
        for parent in &presets[name].extends {
            let Some((parent, _)) = presets.get_key_value(parent) else { continue };
            match colour.get(parent.as_str()) {
                None => visit(parent, presets, colour, stack, cycles),
                Some(Colour::InProgress) => {
                    let start = stack.iter().position(|s| *s == parent).expect("in-progress presets are on the stack");
                    let mut cycle: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                    cycle.push(parent.clone());
                    cycles.push(cycle);
                }
                Some(Colour::Done) => {}
            }
        }
        stack.pop();
        colour.insert(name, Colour::Done);
    }
    let mut colour = HashMap::new();
    let mut cycles = Vec::new();
    for name in presets.keys() {
        if !colour.contains_key(name.as_str()) {
            visit(name, presets, &mut colour, &mut Vec::new(), &mut cycles);
        }
    }
    cycles
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Module,
    Preset,
    Variant,
}

/// Checks on a module file, a preset, or a `variants` entry, as written. Keys are prefixed with
/// `prefix` (e.g. `presets.library.`); the caller sets the file. `depth` is the number of
/// directories between the workspace root and the module (unknown for presets).
fn module_checks(m: &ModuleFile, prefix: &str, kind: Kind, depth: Option<usize>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let at = |key: &str| format!("{prefix}{key}");
    if kind == Kind::Preset && m.name.is_some() {
        out.push(Diagnostic::new("presets cannot set `name`").at_key(at("name")));
    }
    if kind == Kind::Variant {
        let forbidden = [
            ("name", m.name.is_some()),
            ("extends", !m.extends.is_empty()),
            ("platforms", !m.platforms.is_empty()),
            ("scala", m.scala.is_some()),
            ("variants", !m.variants.is_empty()),
        ];
        for (key, set) in forbidden {
            if set {
                out.push(
                    Diagnostic::new(format!("`{key}` cannot be set in a variant override"))
                        .at_key(prefix.trim_end_matches('.')),
                );
            }
        }
    }
    if let Some(p) = m.platforms.iter().find(|p| **p != Platform::Jvm) {
        out.push(not_supported(at("platforms"), &format!("the `{p}` platform"), 2));
    }
    if !m.tasks.is_empty() {
        out.push(not_supported(at("tasks"), "`tasks`", 3));
    }
    if !m.generated_sources.is_empty() {
        out.push(not_supported(at("generatedSources"), "`generatedSources`", 3));
    }
    for (i, bom) in m.boms.iter().enumerate() {
        if let Err(e) = VersionedCoord::parse(bom) {
            out.push(Diagnostic::new(e.to_string()).at_key(at(&format!("boms[{i}]"))));
        }
    }
    let dirs = [
        ("sources", &m.sources),
        ("resources", &m.resources),
        ("testSources", &m.test_sources),
        ("testResources", &m.test_resources),
    ];
    for (key, list) in dirs {
        for (i, dir) in list.iter().flatten().enumerate() {
            if let Some(d) = check_source_dir(dir, depth) {
                out.push(d.at_key(at(&format!("{key}[{i}]"))));
            }
        }
    }
    if kind != Kind::Variant {
        for (selector, entry) in &m.variants {
            if let Err(e) = VariantSelector::parse(selector) {
                let d = Diagnostic::new(format!("invalid variant selector `{selector}`: {e}"))
                    .at_key(at(&format!("variants.{selector}")));
                out.push(match e.help() {
                    Some(help) => d.with_help(help),
                    None => d,
                });
            }
            out.extend(module_checks(entry, &format!("{prefix}variants.{selector}."), Kind::Variant, depth));
        }
    }
    out
}

/// Source and resource directories are relative to the module and may point anywhere inside the
/// workspace (`../shared` is a common sbt layout), but not outside it. With `depth` unknown (a
/// preset), only absolute paths are rejected.
fn check_source_dir(dir: &str, depth: Option<usize>) -> Option<Diagnostic> {
    if is_absolute(dir) {
        return Some(
            Diagnostic::new(format!("`{dir}` is an absolute path"))
                .with_help("source and resource directories are relative to the module directory"),
        );
    }
    depth
        .filter(|&d| leaves_root(dir, d))
        .map(|_| Diagnostic::new(format!("`{dir}` is outside the workspace")).with_help(INSIDE_WORKSPACE))
}

const INSIDE_WORKSPACE: &str =
    "source and resource directories are relative to the module and may point anywhere inside the workspace root";

/// Absolute on any OS, so a config is valid or invalid everywhere: a leading `/` or `\`, or a
/// drive letter (`C:`, `C:/…`, `C:\…`).
fn is_absolute(dir: &str) -> bool {
    let path = Path::new(dir);
    let drive = matches!(dir.as_bytes(), [letter, b':', rest @ ..]
        if letter.is_ascii_alphabetic() && matches!(rest.first(), None | Some(b'/' | b'\\')));
    drive
        || path.has_root()
        || dir.starts_with(['/', '\\'])
        || matches!(path.components().next(), Some(Component::Prefix(_)))
}

/// Whether the relative path `dir`, taken from a module `depth` directories below the workspace
/// root, climbs out of the root with `..`.
fn leaves_root(dir: &str, mut depth: usize) -> bool {
    for component in Path::new(dir).components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return true,
            },
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

/// Resolves `.` and `..` lexically (`modules/a/../shared` → `modules/shared`), matching how
/// [`leaves_root`] checks paths.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if matches!(out.components().next_back(), Some(Component::Normal(_))) => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from(".") } else { out }
}

/// Directories between the workspace root and `rel_dir`.
fn depth_of(rel_dir: &Path) -> usize {
    rel_dir.components().filter(|c| matches!(c, Component::Normal(_))).count()
}

/// Module directories, relative to the root, in a stable order.
fn discover_modules(
    root: &Path,
    config: &WorkspaceFile,
    ws_file: Option<&Path>,
    diags: &mut Diagnostics,
) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    if root.join(MODULE_FILE).is_file() {
        dirs.insert(PathBuf::new());
    }
    if config.modules.is_empty() {
        return dirs.into_iter().collect();
    }
    let mut include = GlobSetBuilder::new();
    let mut exclude = GlobSetBuilder::new();
    // The part before `/**` of each `!…/**` exclusion: everything under a match is excluded.
    let mut recursive = GlobSetBuilder::new();
    let mut exclude_all = false;
    // Only the literal prefix of each include glob is walked (`modules/*` walks `modules/`), so
    // large trees elsewhere in the workspace are never traversed.
    let mut walk_roots = BTreeSet::new();
    for pattern in &config.modules {
        let (negated, pat) = match pattern.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, pattern.as_str()),
        };
        let pat = pat.trim_start_matches("./").trim_end_matches('/');
        match GlobBuilder::new(pat).literal_separator(true).build() {
            Ok(glob) => {
                if negated {
                    exclude.add(glob);
                    if pat == "**" {
                        exclude_all = true;
                    } else if let Some(prefix) = pat.strip_suffix("/**")
                        && let Ok(g) = GlobBuilder::new(prefix).literal_separator(true).build()
                    {
                        recursive.add(g);
                    }
                } else {
                    include.add(glob);
                    walk_roots.extend(literal_prefix(pat));
                }
            }
            Err(e) => {
                let d = Diagnostic::new(format!("invalid glob `{pattern}`: {e}")).at_key("modules");
                diags.push(match ws_file {
                    Some(f) => d.in_file(f),
                    None => d,
                });
            }
        }
    }
    let (include, exclude): (GlobSet, GlobSet) =
        (include.build().unwrap_or_default(), exclude.build().unwrap_or_default());
    let recursive = recursive.build().unwrap_or_default();
    // Whether `dir` and everything beneath it is excluded: `dir` itself is, and a `!…/**`
    // exclusion covers it (its prefix matches `dir` or an ancestor). `!modules/b` alone leaves
    // `modules/b/c` included, and finite-depth patterns (`!modules/b/*/*`) never cover a subtree.
    let fully_excluded = |dir: &Path| {
        !dir.as_os_str().is_empty()
            && exclude.is_match(dir)
            && (exclude_all || dir.ancestors().any(|a| !a.as_os_str().is_empty() && recursive.is_match(a)))
    };
    // A root nested in another is walked as part of it.
    let walk_roots: Vec<PathBuf> = walk_roots
        .iter()
        .filter(|r| !walk_roots.iter().any(|other| other != *r && r.starts_with(other)))
        .map(|r| root.join(r))
        .filter(|r| r.is_dir())
        .collect();
    let Some((first, rest)) = walk_roots.split_first() else { return dirs.into_iter().collect() };
    let mut builder = ignore::WalkBuilder::new(first);
    for r in rest {
        builder.add(r);
    }
    // Ignore files don't hide modules, and symlinked module directories count (symlink loops
    // and dangling symlinks are skipped; other walk errors are reported). Dot-directories are
    // skipped, as `*` and `**` skip them in pnpm's workspace globs.
    let walker = builder
        .hidden(true)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .ignore(false)
        .parents(false)
        .follow_links(true)
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | "target" | "node_modules" | ".scpm")))
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if is_skippable_walk_error(&e) => continue,
            // Errors inside a fully excluded subtree don't matter.
            Err(e) if walk_error_path(&e).is_some_and(|p| rel(root, p).ancestors().any(fully_excluded)) => {
                continue;
            }
            Err(e) => {
                let d = Diagnostic::new(format!("cannot search for modules: {e}"))
                    .at_key("modules")
                    .with_help("make the directory readable, or exclude it with a `!` pattern");
                diags.push(match ws_file {
                    Some(f) => d.in_file(f),
                    None => d,
                });
                continue;
            }
        };
        if entry.file_name() != MODULE_FILE || !entry.path().is_file() {
            continue;
        }
        let Some(dir) = entry.path().parent() else { continue };
        let rel_dir = rel(root, dir);
        if rel_dir.as_os_str().is_empty() {
            continue;
        }
        if include.is_match(&rel_dir) && !exclude.is_match(&rel_dir) {
            dirs.insert(rel_dir);
        }
    }
    dirs.into_iter().collect()
}

/// Walk errors that don't mean part of the tree went unsearched: symlink loops, and dangling
/// symlinks or files removed mid-walk.
fn is_skippable_walk_error(err: &ignore::Error) -> bool {
    match err {
        ignore::Error::Loop { .. } => true,
        ignore::Error::WithPath { err, .. }
        | ignore::Error::WithDepth { err, .. }
        | ignore::Error::WithLineNumber { err, .. } => is_skippable_walk_error(err),
        ignore::Error::Io(e) => e.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

/// The path a walk error is about, if it names one.
fn walk_error_path(err: &ignore::Error) -> Option<&Path> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => walk_error_path(err),
        _ => None,
    }
}

/// The directory a `modules` glob can only match inside: its leading components without glob
/// characters (`modules/*` → `modules`, `*` → the root). `None` for globs that climb out of the
/// root, which can't match a module directory.
fn literal_prefix(pattern: &str) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for part in pattern.split('/') {
        if part.contains(['*', '?', '[', ']', '{', '}', '\\']) {
            break;
        }
        match part {
            "" | "." => {}
            ".." => return None,
            _ => prefix.push(part),
        }
    }
    Some(prefix)
}

/// Where a module's effective `scala` came from, so errors point at the right file.
#[derive(Debug, Clone, PartialEq)]
enum ScalaOrigin {
    Toolchain,
    Preset(String),
    Module,
}

/// A `variants` entry; entries repeated under the same selector are merged into the first.
struct VariantEntry {
    key: String,
    selector: VariantSelector,
    overlays: Vec<Value>,
    /// Whether the module's own file has this selector (only then is "matches no variant" an
    /// error: presets may carry entries for variants a module doesn't have).
    in_module: bool,
    matched: bool,
}

/// A module's config during expansion (design §4.4, "Effective variant config").
struct Expansion {
    effective: Value,
    applied: HashSet<String>,
    variants: Vec<VariantEntry>,
    scala_origin: Option<ScalaOrigin>,
}

impl Expansion {
    /// Applies `name` after its own `extends`, depth-first; each preset at most once. Unknown
    /// presets and cycles are reported by [`validate_workspace`].
    fn apply_preset(&mut self, ctx: &Ctx, name: &str, stack: &mut Vec<String>) {
        if self.applied.contains(name) || stack.iter().any(|s| s == name) {
            return;
        }
        let (Some(raw), Some(typed)) = (ctx.presets.get(name), ctx.config.presets.get(name)) else { return };
        stack.push(name.to_string());
        for parent in &typed.extends {
            self.apply_preset(ctx, parent, stack);
        }
        stack.pop();
        self.applied.insert(name.to_string());
        let (own, variants) = split_module(raw.clone());
        if own.get("scala").is_some_and(|v| !v.is_null()) {
            self.scala_origin = Some(ScalaOrigin::Preset(name.to_string()));
        }
        merge_module(&mut self.effective, own);
        self.add_variants(variants, false);
    }

    fn add_variants(&mut self, variants: Option<Value>, in_module: bool) {
        // `variants: !replace {...}` drops the entries inherited so far.
        let variants = match variants.map(untag_replace) {
            Some((value, true)) => {
                self.variants.clear();
                value
            }
            Some((value, false)) => value,
            None => return,
        };
        let Value::Mapping(map) = variants else { return };
        for (key, overlay) in map {
            // Invalid selectors are reported where they are written.
            let Some(key) = key.as_str() else { continue };
            let Ok(selector) = VariantSelector::parse(key) else { continue };
            match self.variants.iter_mut().find(|e| e.key == key) {
                Some(entry) => {
                    entry.overlays.push(overlay);
                    entry.in_module |= in_module;
                }
                None => self.variants.push(VariantEntry {
                    key: key.to_string(),
                    selector,
                    overlays: vec![overlay],
                    in_module,
                    matched: false,
                }),
            }
        }
    }
}

/// Splits a module-shaped value into its own settings and its `variants` (dropping `extends`,
/// which is applied separately).
fn split_module(mut value: Value) -> (Value, Option<Value>) {
    let Some(map) = value.as_mapping_mut() else { return (value, None) };
    map.remove("extends");
    let variants = map.remove("variants");
    (value, variants)
}

/// Loads one module. On failure, also returns the module's name (from `name:` if the file has
/// one, else the directory) so that its dependents aren't reported as depending on an unknown
/// module.
fn load_module(ctx: &Ctx, rel_dir: &Path) -> Result<Module, (String, Diagnostics)> {
    expand_module(ctx, rel_dir).map_err(|d| {
        let declared = fs::read_to_string(ctx.root.join(rel_dir).join(MODULE_FILE))
            .ok()
            .and_then(|text| serde_yaml_ng::from_str::<Value>(&text).ok())
            .and_then(|raw| raw.get("name").and_then(Value::as_str).map(str::to_string));
        (declared.unwrap_or_else(|| dir_name(ctx.root, rel_dir)), d)
    })
}

/// The module name implied by its directory.
fn dir_name(root: &Path, rel_dir: &Path) -> String {
    let dir = if rel_dir.as_os_str().is_empty() { root } else { rel_dir };
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn expand_module(ctx: &Ctx, rel_dir: &Path) -> Result<Module, Diagnostics> {
    let file = rel_dir.join(MODULE_FILE);
    let (typed, mut raw): (ModuleFile, Value) = parse_file(ctx.root, &ctx.root.join(&file))?;
    let mut diags = Diagnostics::default();
    let checks = module_checks(&typed, "", Kind::Module, Some(depth_of(rel_dir)));
    for d in normalize_variant_keys(&mut raw, "").into_iter().chain(checks) {
        diags.push(d.in_file(&file));
    }

    // Resolution order (design §4.4): toolchain defaults → presets → module → variants.
    let mut exp = Expansion {
        effective: Value::Mapping(Default::default()),
        applied: HashSet::new(),
        variants: Vec::new(),
        scala_origin: None,
    };
    if let Some(scala) = &ctx.config.toolchain.scala {
        exp.effective.as_mapping_mut().expect("mapping").insert("scala".into(), Value::String(scala.0.clone()));
        exp.scala_origin = Some(ScalaOrigin::Toolchain);
    }
    let known: Vec<&str> = ctx.presets.keys().map(String::as_str).collect();
    for preset in &typed.extends {
        if ctx.presets.contains_key(preset) {
            exp.apply_preset(ctx, preset, &mut Vec::new());
        } else {
            diags.push(unknown_preset(preset, &known).in_file(&file).at_key("extends"));
        }
    }
    let (own, own_variants) = split_module(raw);
    if own.get("scala").is_some_and(|v| !v.is_null()) {
        exp.scala_origin = Some(ScalaOrigin::Module);
    }
    merge_module(&mut exp.effective, own);
    exp.add_variants(own_variants, true);
    if !diags.is_empty() {
        diags.dedup();
        return Err(diags);
    }

    let base: ModuleFile = from_value(&exp.effective, &file)?;
    let (name, from_dir) = match &base.name {
        Some(n) => (n.clone(), false),
        None => (dir_name(ctx.root, rel_dir), true),
    };
    if !valid_module_name(&name) {
        let rules = "module names use lowercase letters, digits, `-`, `_` and `.`, starting with a letter or digit";
        let d = if from_dir {
            Diagnostic::new(format!("invalid module name `{name}` (taken from the directory name)"))
                .with_help(format!("{rules}; set `name:` to a valid name"))
        } else {
            Diagnostic::new(format!("invalid module name `{name}`")).at_key("name").with_help(rules)
        };
        return Err(d.in_file(&file).into());
    }

    let platforms: Vec<Platform> = if base.platforms.is_empty() {
        vec![Platform::Jvm]
    } else {
        base.platforms.iter().copied().collect::<BTreeSet<_>>().into_iter().collect()
    };
    let Some(specs) = base.scala.clone().map(OneOrMany::into_vec) else {
        return Err(Diagnostic::new("no Scala version")
            .in_file(&file)
            .with_help(
                "set `scala:` here, `toolchain.scala` in scpm-workspace.yaml, or `scala: none` for a Java-only module",
            )
            .into());
    };
    let scalas = match resolve_scala_versions(ctx, &specs) {
        Ok(s) => s,
        // Errors in the workspace file are reported there, once.
        Err(d) => {
            return Err(match exp.scala_origin {
                Some(ScalaOrigin::Toolchain) => ctx.in_ws(d.at_key("toolchain.scala")),
                Some(ScalaOrigin::Preset(p)) => ctx.in_ws(d.at_key(format!("presets.{p}.scala"))),
                Some(ScalaOrigin::Module) | None => d.in_file(&file).at_key("scala"),
            }
            .into());
        }
    };
    if scalas == [None] && platforms != [Platform::Jvm] {
        return Err(Diagnostic::new("Java-only modules (`scala: none`) can only target the jvm platform")
            .in_file(&file)
            .at_key("platforms")
            .into());
    }

    let mut variants = Vec::new();
    for &platform in &platforms {
        for scala in &scalas {
            let id = variant_id(&name, platform, scala.as_ref());
            let mut value = exp.effective.clone();
            for entry in &mut exp.variants {
                if entry.selector.matches(platform, scala.as_ref()) {
                    entry.matched = true;
                    for overlay in &entry.overlays {
                        merge_module(&mut value, overlay.clone());
                    }
                }
            }
            match from_value::<ModuleFile>(&value, &file)
                .and_then(|cfg| build_variant(ctx, &name, rel_dir, &file, id, platform, scala.clone(), cfg))
            {
                Ok(v) => variants.push(v),
                Err(d) => diags.0.extend(d.0),
            }
        }
    }
    let all_ids: Vec<String> = platforms
        .iter()
        .flat_map(|&p| scalas.iter().map(move |s| (p, s)))
        .map(|(p, s)| variant_id(&name, p, s.as_ref()))
        .collect();
    for entry in exp.variants.iter().filter(|e| e.in_module && !e.matched) {
        diags.push(
            Diagnostic::new(format!("variant selector `{}` matches no variant", entry.key))
                .in_file(&file)
                .at_key(format!("variants.{}", entry.key))
                .with_help(format!("this module's variants: {}", all_ids.join(", "))),
        );
    }
    let dir = if rel_dir.as_os_str().is_empty() { ".".to_string() } else { slash_path(rel_dir) };
    diags.dedup();
    diags.into_result(Module { name, dir, variants })
}

fn from_value<T: DeserializeOwned>(value: &Value, file: &Path) -> Result<T, Diagnostics> {
    serde_path_to_error::deserialize(value.clone()).map_err(|e| Diagnostic::from_value_error(file, e).into())
}

fn valid_module_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
}

pub fn variant_id(module: &str, platform: Platform, scala: Option<&ScalaVersion>) -> String {
    match scala {
        Some(s) => format!("{module}@{platform}-{}", s.binary),
        None => format!("{module}@{platform}"),
    }
}

/// Resolves a `scala` value (one or more versions, catalog references, or `none`).
fn resolve_scala_versions(ctx: &Ctx, specs: &[Ver]) -> Result<Vec<Option<ScalaVersion>>, Diagnostic> {
    if specs.is_empty() {
        return Err(Diagnostic::new("`scala` lists no versions")
            .with_help("list at least one Scala version, or use `scala: none` for a Java-only module"));
    }
    if specs.iter().any(|s| s.as_str() == "none") {
        if specs.len() > 1 {
            return Err(Diagnostic::new("`none` cannot be combined with other Scala versions"));
        }
        return Ok(vec![None]);
    }
    let mut out: Vec<Option<ScalaVersion>> = Vec::new();
    for spec in specs {
        let VersionSpec::Exact { version } = resolve_catalog(ctx, "scala", spec)? else {
            return Err(Diagnostic::new(format!("Scala version `{spec}` must be an exact version, not a range")));
        };
        let sv = ScalaVersion::parse(&version).map_err(Diagnostic::new)?;
        if out.iter().flatten().any(|s| s.binary == sv.binary) {
            return Err(Diagnostic::new(format!("two Scala versions with binary version {}", sv.binary)));
        }
        out.push(Some(sv));
    }
    Ok(out)
}

/// Resolves `catalog:`/`catalog:<name>` for `key`; other specs pass through.
fn resolve_catalog(ctx: &Ctx, key: &str, spec: &Ver) -> Result<VersionSpec, Diagnostic> {
    let parsed = VersionSpec::parse(spec.as_str());
    let VersionSpec::Catalog { catalog } = &parsed else { return Ok(parsed) };
    let table = match catalog {
        None => Some(&ctx.config.catalog),
        Some(name) => ctx.config.catalogs.get(name),
    };
    let Some(table) = table else {
        let known: Vec<_> = ctx.config.catalogs.keys().map(String::as_str).collect();
        return Err(Diagnostic::new(format!("unknown catalog `{}`", catalog.as_deref().unwrap_or_default()))
            .with_help(format!(
                "named catalogs: {}",
                if known.is_empty() { "(none)".into() } else { known.join(", ") }
            )));
    };
    match table.get(key) {
        Some(v) => Ok(VersionSpec::parse(v.as_str())),
        None => {
            let label =
                catalog.as_deref().map(|c| format!("catalog `{c}`")).unwrap_or_else(|| "the default catalog".into());
            let elsewhere: Vec<_> = ctx
                .config
                .catalogs
                .iter()
                .filter(|(_, t)| t.contains_key(key))
                .map(|(n, _)| format!("catalog:{n}"))
                .collect();
            let d = Diagnostic::new(format!("`{key}` is not in {label}"));
            Err(if elsewhere.is_empty() { d } else { d.with_help(format!("found in: {}", elsewhere.join(", "))) })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_variant(
    ctx: &Ctx,
    name: &str,
    rel_dir: &Path,
    file: &Path,
    id: String,
    platform: Platform,
    scala: Option<ScalaVersion>,
    cfg: ModuleFile,
) -> Result<Variant, Diagnostics> {
    let mut diags = Diagnostics::default();
    let mut dependencies = Vec::new();
    let mut workspace_dependencies = Vec::new();
    // Each external coordinate's first declaration (key path) and its effective version.
    let mut declared: std::collections::HashMap<String, (String, VersionSpec)> = std::collections::HashMap::new();
    let scoped = [
        (Scope::Compile, &cfg.dependencies),
        (Scope::Provided, &cfg.provided_dependencies),
        (Scope::Runtime, &cfg.runtime_dependencies),
        (Scope::Test, &cfg.test_dependencies),
    ];
    for (scope, deps) in scoped {
        for (key, spec) in deps {
            let at = format!("{}.{key}", scope.key());
            let written = VersionSpec::parse(spec.version().as_str());
            if !key.contains(':') {
                match written {
                    _ if key == name => diags.push(
                        Diagnostic::new(format!("module `{name}` cannot depend on itself")).in_file(file).at_key(at),
                    ),
                    VersionSpec::Workspace { range } if range == "*" => match spec {
                        DepSpec::Detailed(d)
                            if !d.exclude.is_empty()
                                || d.classifier.is_some()
                                || d.kind.is_some()
                                || d.transitive.is_some() =>
                        {
                            diags.push(
                                Diagnostic::new(
                                    "`exclude`, `classifier`, `type` and `transitive` apply only to external \
                                     dependencies",
                                )
                                .in_file(file)
                                .at_key(at),
                            )
                        }
                        _ => workspace_dependencies.push(WorkspaceDependency { scope, module: key.clone() }),
                    },
                    VersionSpec::Workspace { range } => diags.push(
                        Diagnostic::new(format!("`workspace:{range}` is not supported; use `workspace:*`"))
                            .in_file(file)
                            .at_key(at),
                    ),
                    _ => diags.push(
                        Diagnostic::new(format!(
                            "`{key}` looks like a workspace module; its version must be `workspace:*`"
                        ))
                        .in_file(file)
                        .at_key(at)
                        .with_help("external dependencies are written `org:name`, `org::name` or `org:::name`"),
                    ),
                }
                continue;
            }
            match external_dependency(ctx, scope, key, spec, scala.is_some()) {
                Ok(d) => {
                    // One version per coordinate: a variant selects and locks each once.
                    let coord = d.coord.to_string();
                    match declared.get(&coord) {
                        Some((first_at, first)) if *first != d.version => diags.push(
                            Diagnostic::new(format!(
                                "`{coord}` is declared with different versions: `{first}` at `{first_at}` and `{}` at `{at}`",
                                d.version
                            ))
                            .in_file(file)
                            .at_key(at)
                            .with_help(
                                "use one version in every scope, e.g. put it in the catalog and write `catalog` in each",
                            ),
                        ),
                        Some(_) => {}
                        None => {
                            declared.insert(coord, (at, d.version.clone()));
                        }
                    }
                    dependencies.push(d);
                }
                Err(d) => diags.push(d.in_file(file).at_key(at)),
            }
        }
    }
    let mut compiler_plugins = Vec::new();
    for (key, spec) in &cfg.compiler_plugins {
        let at = format!("compilerPlugins.{key}");
        if scala.is_none() {
            diags.push(Diagnostic::new("Java-only modules cannot have compiler plugins").in_file(file).at_key(at));
            continue;
        }
        match external_dependency(ctx, Scope::Provided, key, spec, true) {
            Ok(d) => compiler_plugins.push(d),
            Err(d) => diags.push(d.in_file(file).at_key(at)),
        }
    }
    // Invalid module and preset BOMs are reported by `module_checks`.
    let mut boms = ctx.boms.clone();
    for b in cfg.boms.iter().filter_map(|raw| VersionedCoord::parse(raw).ok()) {
        if !boms.contains(&b) {
            boms.push(b);
        }
    }
    // The module's own directories were checked as written; any that leave the workspace here
    // came from a preset, which can't know the module's depth.
    let depth = depth_of(rel_dir);
    let dir_lists = [
        ("sources", &cfg.sources),
        ("resources", &cfg.resources),
        ("testSources", &cfg.test_sources),
        ("testResources", &cfg.test_resources),
    ];
    for (key, list) in dir_lists {
        for dir in list.iter().flatten().filter(|d| !is_absolute(d) && leaves_root(d, depth)) {
            diags.push(
                Diagnostic::new(format!("`{dir}` (from a preset) is outside the workspace for this module"))
                    .in_file(file)
                    .at_key(key)
                    .with_help(INSIDE_WORKSPACE),
            );
        }
    }
    let module_root = rel_dir.to_path_buf();
    let resolve_dirs = |explicit: &Option<Vec<String>>, defaults: &[&str], scope_dir: &str| -> Vec<PathBuf> {
        match explicit {
            Some(list) => list.iter().map(|d| normalize(&module_root.join(d))).collect(),
            None => {
                let mut dirs: Vec<PathBuf> = defaults.iter().map(|d| module_root.join(d)).collect();
                if defaults.iter().any(|d| d.ends_with("scala")) {
                    for suffix in source_suffixes(platform, scala.as_ref()) {
                        let dir = module_root.join(format!("src/{scope_dir}/scala-{suffix}"));
                        if ctx.root.join(&dir).is_dir() {
                            dirs.push(dir);
                        }
                    }
                }
                dirs
            }
        }
    };
    let sources = resolve_dirs(&cfg.sources, &["src/main/scala", "src/main/java"], "main");
    let resources = resolve_dirs(&cfg.resources, &["src/main/resources"], "main");
    let test_sources = resolve_dirs(&cfg.test_sources, &["src/test/scala", "src/test/java"], "test");
    let test_resources = resolve_dirs(&cfg.test_resources, &["src/test/resources"], "test");
    let out_dir = PathBuf::from(".scpm/out").join(&id);
    let mut tasks = ctx.config.tasks.clone();
    tasks.extend(cfg.tasks.clone());
    let variant = Variant {
        id,
        module: name.to_string(),
        platform,
        scala,
        organization: cfg.organization.clone().or_else(|| ctx.config.organization.clone()),
        version: cfg.version.as_ref().map(|v| v.0.clone()),
        dependencies,
        workspace_dependencies,
        compiler_plugins,
        boms,
        scalac_options: cfg.scalac_options.clone(),
        javac_options: cfg.javac_options.clone(),
        release: cfg.release.as_ref().map(|v| v.0.clone()),
        main_class: cfg.main_class.clone(),
        jvm_options: cfg.jvm_options.clone(),
        env: cfg.env.clone(),
        sources,
        resources,
        test_sources,
        test_resources,
        generated_sources: cfg.generated_sources.iter().map(|d| out_dir.join(d)).collect(),
        out_dir,
        publish: cfg.publish.unwrap_or(false),
        version_scheme: cfg.version_scheme,
        tasks,
    };
    diags.into_result(variant)
}

/// `scala-3`, `scala-2.13`, `scala-jvm`, … in the order sbt uses.
fn source_suffixes(platform: Platform, scala: Option<&ScalaVersion>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(s) = scala {
        let major = s.full.split('.').next().unwrap_or_default().to_string();
        out.push(major.clone());
        if s.binary != major {
            out.push(s.binary.clone());
        }
    }
    out.push(platform.as_str().to_string());
    out
}

fn external_dependency(
    ctx: &Ctx,
    scope: Scope,
    key: &str,
    spec: &DepSpec,
    is_scala: bool,
) -> Result<Dependency, Diagnostic> {
    let coord = Coord::parse(key).map_err(|e| Diagnostic::new(e.to_string()))?;
    if !is_scala && coord.cross != CrossKind::Java {
        return Err(Diagnostic::new(format!(
            "`{coord}` needs a Scala version, but this module is Java-only (`scala: none`)"
        ))
        .with_help("use the full artifact name with a single `:`, e.g. `org:name_3`"));
    }
    let version = resolve_catalog(ctx, key, spec.version())?;
    if let VersionSpec::Workspace { .. } = version {
        return Err(Diagnostic::new("`workspace:` is only for workspace modules (keys without `:`)"));
    }
    let (exclude, classifier, kind, transitive) = match spec {
        DepSpec::Version(_) => (Vec::new(), None, None, true),
        DepSpec::Detailed(d) => (d.exclude.clone(), d.classifier.clone(), d.kind.clone(), d.transitive.unwrap_or(true)),
    };
    for pattern in &exclude {
        if pattern.split(':').count() != 2 {
            return Err(Diagnostic::new(format!("invalid exclusion `{pattern}`; expected `org:name` (`*` allowed)")));
        }
    }
    Ok(Dependency { scope, coord, spec: spec.version().0.clone(), version, exclude, classifier, kind, transitive })
}

/// Reports dependencies on modules that don't exist. `failed` holds the names of modules that
/// exist but failed to load; their errors are reported already.
fn validate_workspace_deps(ws: &Workspace, failed: &BTreeSet<String>, diags: &mut Diagnostics) {
    let names: Vec<_> = ws.modules.iter().map(|m| m.name.as_str()).chain(failed.iter().map(String::as_str)).collect();
    for module in &ws.modules {
        for variant in &module.variants {
            for dep in &variant.workspace_dependencies {
                if ws.module(&dep.module).is_none() && !failed.contains(&dep.module) {
                    let mut d = Diagnostic::new(format!("unknown workspace module `{}`", dep.module))
                        .in_file(module_file(module))
                        .at_key(format!("{}.{}", dep.scope.key(), dep.module));
                    let others = names.iter().copied().filter(|n| *n != module.name);
                    if let Some(b) = closest(&dep.module, others) {
                        d = d.with_help(format!("did you mean `{b}`?"));
                    }
                    diags.push(d);
                }
            }
        }
    }
}

pub fn module_file(module: &Module) -> PathBuf {
    if module.dir == "." { PathBuf::from(MODULE_FILE) } else { Path::new(&module.dir).join(MODULE_FILE) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scala_versions() {
        assert_eq!(ScalaVersion::parse("3.9.0").unwrap().binary, "3");
        assert_eq!(ScalaVersion::parse("3.8.0-RC1").unwrap().binary, "3");
        assert_eq!(ScalaVersion::parse("2.13.18").unwrap().binary, "2.13");
        assert_eq!(ScalaVersion::parse("2.12.20").unwrap().binary, "2.12");
        for bad in ["3", "3.9", "2.11.12", "4.0.0", "3.9.0-", "banana"] {
            assert!(ScalaVersion::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn module_globs_walk_their_literal_prefix() {
        let prefix = |p: &str| literal_prefix(p).map(|p| slash_path(&p));
        assert_eq!(prefix("modules/*"), Some("modules".into()));
        assert_eq!(prefix("libs/core/**"), Some("libs/core".into()));
        assert_eq!(prefix("modules/a"), Some("modules/a".into()));
        assert_eq!(prefix("*"), Some("".into()));
        assert_eq!(prefix("**/x"), Some("".into()));
        assert_eq!(prefix("mod{a,b}/*"), Some("".into()));
        assert_eq!(prefix("../x/*"), None);
    }

    #[test]
    fn source_dirs_stay_inside_the_workspace() {
        for ok in ["src/main/scala", "./gen", "a/../b"] {
            assert!(!leaves_root(ok, 0), "{ok}");
        }
        assert!(!leaves_root("../shared", 1));
        assert!(!leaves_root("../../shared/src", 2));
        for bad in ["../shared", "a/../../b"] {
            assert!(leaves_root(bad, 0), "{bad}");
        }
        assert!(leaves_root("../../x", 1));
        for abs in ["/abs", "\\\\server\\share", "C:/src", "c:\\src", "D:"] {
            assert!(is_absolute(abs), "{abs}");
        }
        for rel in ["src", "C", "ab:/x", "1:/x"] {
            assert!(!is_absolute(rel), "{rel}");
        }
    }
}
