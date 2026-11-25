//! Loading a workspace: discover modules, validate files, expand presets, resolve catalogs
//! and compute each module's variants (design §4).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use indexmap::IndexMap;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_yaml_ng::Value;

use crate::config::{
    DepSpec, MODULE_FILE, ModuleFile, Repository, TaskDef, VersionScheme, WORKSPACE_FILE, WorkspaceFile,
};
use crate::coord::{Coord, CrossKind, VersionedCoord, scala_binary_version};
use crate::diag::{Diagnostic, Diagnostics, closest};
use crate::merge::merge_module;
use crate::platform::Platform;
use crate::version::{Ver, VersionSpec};

/// A loaded, fully resolved workspace.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub root: PathBuf,
    /// `None` for a lone `scpm.yaml` without a workspace file.
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
    /// Directory relative to the workspace root (`.` for the root module).
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
    pub sources: Vec<PathBuf>,
    pub resources: Vec<PathBuf>,
    pub test_sources: Vec<PathBuf>,
    pub test_resources: Vec<PathBuf>,
    /// `.scpm/out/<variant-id>/`, relative to the workspace root.
    pub out_dir: PathBuf,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub generated_sources: Vec<PathBuf>,
    pub publish: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_scheme: Option<VersionScheme>,
    pub tasks: IndexMap<String, TaskDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScalaVersion {
    pub full: String,
    pub binary: String,
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
    pub const ALL: [Scope; 4] = [Scope::Compile, Scope::Provided, Scope::Runtime, Scope::Test];

    fn key(self) -> &'static str {
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
    let presets: IndexMap<String, Value> = match raw.get("presets") {
        Some(Value::Mapping(m)) => m.iter().filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.clone()))).collect(),
        _ => IndexMap::new(),
    };
    let ws_rel = workspace_file.as_ref().map(|p| rel(root, p));
    let ctx = Ctx { root, config: &config, presets: &presets, ws_file: ws_rel.clone() };
    validate_workspace(&ctx, &mut diags);

    let mut modules = Vec::new();
    for dir in discover_modules(root, &config, ws_rel.as_deref(), &mut diags) {
        match load_module(&ctx, &dir) {
            Ok(m) => modules.push(m),
            Err(d) => diags.0.extend(d.0),
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
    validate_workspace_deps(&ws, &mut diags);
    diags.into_result(ws)
}

struct Ctx<'a> {
    root: &'a Path,
    config: &'a WorkspaceFile,
    presets: &'a IndexMap<String, Value>,
    ws_file: Option<PathBuf>,
}

impl Ctx<'_> {
    fn ws_diag(&self, message: impl Into<String>) -> Diagnostic {
        let d = Diagnostic::new(message);
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
    let de = serde_yaml_ng::Deserializer::from_str(&text);
    let typed = serde_path_to_error::deserialize::<_, T>(de).map_err(|e| Diagnostic::from_yaml(&display, e))?;
    let raw: Value = serde_yaml_ng::from_str(&text).map_err(|e| Diagnostic::new(e.to_string()).in_file(display))?;
    let raw = if raw.is_null() { Value::Mapping(Default::default()) } else { raw };
    Ok((typed, raw))
}

fn rel(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).map(Path::to_path_buf).unwrap_or_else(|_| path.to_path_buf())
}

fn validate_workspace(ctx: &Ctx, diags: &mut Diagnostics) {
    for bom in &ctx.config.boms {
        if let Err(e) = VersionedCoord::parse(bom) {
            diags.push(ctx.ws_diag(e.to_string()).at_key("boms"));
        }
    }
    for (key, version) in &ctx.config.overrides {
        if let Err(e) = Coord::parse(key) {
            diags.push(ctx.ws_diag(e.to_string()).at_key(format!("overrides.{key}")));
        }
        if !matches!(VersionSpec::parse(version.as_str()), VersionSpec::Exact { .. }) {
            diags.push(ctx.ws_diag("overrides must be exact versions").at_key(format!("overrides.{key}")));
        }
    }
    for (name, preset) in &ctx.config.presets {
        if preset.name.is_some() {
            diags.push(ctx.ws_diag("presets cannot set `name`").at_key(format!("presets.{name}")));
        }
    }
    if ctx.config.catalogs.contains_key("default") {
        diags.push(
            ctx.ws_diag("`default` is reserved for the default catalog")
                .at_key("catalogs.default")
                .with_help("put these entries under `catalog:`"),
        );
    }
    for (name, catalog) in std::iter::once((&String::new(), &ctx.config.catalog)).chain(&ctx.config.catalogs) {
        for (key, version) in catalog {
            let at = if name.is_empty() { format!("catalog.{key}") } else { format!("catalogs.{name}.{key}") };
            if key != "scala"
                && let Err(e) = Coord::parse(key)
            {
                diags.push(ctx.ws_diag(e.to_string()).at_key(at.clone()));
            }
            if matches!(
                VersionSpec::parse(version.as_str()),
                VersionSpec::Catalog { .. } | VersionSpec::Workspace { .. }
            ) {
                diags.push(ctx.ws_diag("catalog entries must be versions or ranges").at_key(at));
            }
        }
    }
    let unsupported = [
        (!ctx.config.pipelines.is_empty(), "pipelines", 3),
        (ctx.config.pipeline_base.is_some(), "pipelineBase", 3),
        (ctx.config.versioning.is_some(), "versioning", 4),
        (ctx.config.policy.minimum_release_age.is_some(), "policy.minimumReleaseAge", 3),
    ];
    for (used, key, phase) in unsupported {
        if used {
            diags.push(
                ctx.ws_diag(format!("`{key}` is not supported yet (planned for Phase {phase})"))
                    .with_help("remove it for now; see docs/roadmap.md"),
            );
        }
    }
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
    for pattern in &config.modules {
        let (negated, pat) = match pattern.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, pattern.as_str()),
        };
        let pat = pat.trim_start_matches("./").trim_end_matches('/');
        match GlobBuilder::new(pat).literal_separator(true).build() {
            Ok(glob) => {
                if negated {
                    exclude.add(glob)
                } else {
                    include.add(glob)
                };
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
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("target" | "node_modules" | ".scpm")))
        .build();
    for entry in walker.flatten() {
        if entry.file_name() != MODULE_FILE {
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

fn load_module(ctx: &Ctx, rel_dir: &Path) -> Result<Module, Diagnostics> {
    let file = rel_dir.join(MODULE_FILE);
    let (_typed, raw): (ModuleFile, Value) = parse_file(ctx.root, &ctx.root.join(&file))?;
    let mut diags = Diagnostics::default();

    // Resolution order (design §4.4): toolchain defaults → presets → module → variants.
    let mut effective = Value::Mapping(Default::default());
    if let Some(scala) = &ctx.config.toolchain.scala {
        effective.as_mapping_mut().expect("mapping").insert("scala".into(), Value::String(scala.0.clone()));
    }
    let extends = string_list(raw.get("extends"));
    for preset in &extends {
        apply_preset(ctx, preset, &mut effective, &mut Vec::new(), &file, &mut diags);
    }
    let mut own = raw.clone();
    if let Some(m) = own.as_mapping_mut() {
        m.remove("extends");
    }
    merge_module(&mut effective, own);
    if !diags.is_empty() {
        return Err(diags);
    }

    let base: ModuleFile = from_value(&effective, &file)?;
    let dir_name = rel_dir.file_name().and_then(|n| n.to_str()).map(str::to_string);
    let name =
        base.name.clone().or(dir_name).or_else(|| ctx.root.file_name().and_then(|n| n.to_str()).map(str::to_string));
    let Some(name) = name.filter(|n| valid_module_name(n)) else {
        return Err(Diagnostic::new("invalid or missing module name")
            .in_file(file)
            .with_help("names use lowercase letters, digits, `-`, `_` and `.`; set `name:`")
            .into());
    };

    let platforms: Vec<Platform> = if base.platforms.is_empty() {
        vec![Platform::Jvm]
    } else {
        base.platforms.iter().copied().collect::<BTreeSet<_>>().into_iter().collect()
    };
    let scalas = resolve_scala_versions(ctx, &base, &file, &platforms)?;

    let variant_overrides: Vec<(String, Value)> = match effective.get("variants") {
        Some(Value::Mapping(m)) => m.iter().filter_map(|(k, v)| Some((selector_string(k)?, v.clone()))).collect(),
        _ => Vec::new(),
    };
    for (selector, value) in &variant_overrides {
        for forbidden in ["name", "extends", "platforms", "scala", "variants"] {
            if value.get(forbidden).is_some() {
                diags.push(
                    Diagnostic::new(format!("`{forbidden}` cannot be set in a variant override"))
                        .in_file(file.clone())
                        .at_key(format!("variants.{selector}")),
                );
            }
        }
    }

    let mut variants = Vec::new();
    let mut matched = vec![false; variant_overrides.len()];
    for &platform in &platforms {
        for scala in &scalas {
            let id = variant_id(&name, platform, scala.as_ref());
            let mut value = effective.clone();
            if let Some(m) = value.as_mapping_mut() {
                m.remove("variants");
            }
            for (i, (selector, overlay)) in variant_overrides.iter().enumerate() {
                if selector_matches(selector, platform, scala.as_ref()) {
                    matched[i] = true;
                    merge_module(&mut value, overlay.clone());
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
    for (i, (selector, _)) in variant_overrides.iter().enumerate() {
        if !matched[i] {
            let ids: Vec<_> = variants.iter().map(|v| v.id.as_str()).collect();
            diags.push(
                Diagnostic::new(format!("variant selector `{selector}` matches no variant"))
                    .in_file(file.clone())
                    .at_key(format!("variants.{selector}"))
                    .with_help(format!("this module's variants: {}", ids.join(", "))),
            );
        }
    }
    let dir =
        if rel_dir.as_os_str().is_empty() { ".".to_string() } else { rel_dir.to_string_lossy().replace('\\', "/") };
    diags.into_result(Module { name, dir, variants })
}

fn apply_preset(
    ctx: &Ctx,
    name: &str,
    into: &mut Value,
    stack: &mut Vec<String>,
    file: &Path,
    diags: &mut Diagnostics,
) {
    if stack.iter().any(|s| s == name) {
        stack.push(name.to_string());
        diags.push(ctx.ws_diag(format!("preset cycle: {}", stack.join(" → "))).at_key(format!("presets.{name}")));
        stack.pop();
        return;
    }
    let Some(preset) = ctx.presets.get(name) else {
        let known: Vec<_> = ctx.presets.keys().map(String::as_str).collect();
        let help = match closest(name, known.iter().copied()) {
            Some(b) => format!("did you mean `{b}`?"),
            _ if known.is_empty() => "no presets are defined in the workspace file".to_string(),
            _ => format!("defined presets: {}", known.join(", ")),
        };
        diags.push(Diagnostic::new(format!("unknown preset `{name}`")).in_file(file).at_key("extends").with_help(help));
        return;
    };
    stack.push(name.to_string());
    for parent in string_list(preset.get("extends")) {
        apply_preset(ctx, &parent, into, stack, file, diags);
    }
    stack.pop();
    let mut own = preset.clone();
    if let Some(m) = own.as_mapping_mut() {
        m.remove("extends");
    }
    merge_module(into, own);
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Sequence(items)) => items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// YAML keys like `3` or `2.13` may be numbers; selectors are strings.
fn selector_string(key: &Value) -> Option<String> {
    match key {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
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

/// A selector is dash-separated parts, each matching the platform, the Scala binary version
/// or the full Scala version (design §4.4).
pub fn selector_matches(selector: &str, platform: Platform, scala: Option<&ScalaVersion>) -> bool {
    selector
        .split('-')
        .all(|part| part == platform.as_str() || scala.is_some_and(|s| part == s.binary || part == s.full))
}

fn resolve_scala_versions(
    ctx: &Ctx,
    base: &ModuleFile,
    file: &Path,
    platforms: &[Platform],
) -> Result<Vec<Option<ScalaVersion>>, Diagnostics> {
    let Some(specs) = base.scala.clone().map(|s| s.into_vec()) else {
        return Err(Diagnostic::new("no Scala version")
            .in_file(file)
            .with_help(
                "set `scala:` here, `toolchain.scala` in scpm-workspace.yaml, or `scala: none` for a Java-only module",
            )
            .into());
    };
    if specs.iter().any(|s| s.as_str() == "none") {
        if specs.len() > 1 {
            return Err(Diagnostic::new("`none` cannot be combined with other Scala versions")
                .in_file(file)
                .at_key("scala")
                .into());
        }
        if platforms != [Platform::Jvm] {
            return Err(Diagnostic::new("Java-only modules (`scala: none`) can only target the jvm platform")
                .in_file(file)
                .at_key("platforms")
                .into());
        }
        return Ok(vec![None]);
    }
    let mut out: Vec<Option<ScalaVersion>> = Vec::new();
    for spec in specs {
        let version = resolve_catalog(ctx, "scala", &spec).map_err(|d| d.in_file(file).at_key("scala"))?;
        let VersionSpec::Exact { version } = version else {
            return Err(Diagnostic::new(format!("Scala version `{spec}` must be an exact version, not a range"))
                .in_file(file)
                .at_key("scala")
                .into());
        };
        let sv = ScalaVersion { binary: scala_binary_version(&version), full: version };
        if out.iter().flatten().any(|s| s.binary == sv.binary) {
            return Err(Diagnostic::new(format!("two Scala versions with binary version {}", sv.binary))
                .in_file(file)
                .at_key("scala")
                .into());
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
                    VersionSpec::Workspace { range } if range == "*" => {
                        workspace_dependencies.push(WorkspaceDependency { scope, module: key.clone() })
                    }
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
                Ok(d) => dependencies.push(d),
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
    let mut boms = Vec::new();
    for raw in ctx.config.boms.iter().chain(&cfg.boms) {
        match VersionedCoord::parse(raw) {
            Ok(b) if !boms.contains(&b) => boms.push(b),
            Ok(_) => {}
            Err(e) => diags.push(Diagnostic::new(e.to_string()).in_file(file).at_key("boms")),
        }
    }
    let module_root = rel_dir.to_path_buf();
    let resolve_dirs = |explicit: &Option<Vec<String>>, defaults: &[&str], scope_dir: &str| -> Vec<PathBuf> {
        match explicit {
            Some(list) => list.iter().map(|d| module_root.join(d)).collect(),
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

fn validate_workspace_deps(ws: &Workspace, diags: &mut Diagnostics) {
    for module in &ws.modules {
        for variant in &module.variants {
            for dep in &variant.workspace_dependencies {
                if ws.module(&dep.module).is_none() {
                    let names: Vec<_> = ws.modules.iter().map(|m| m.name.as_str()).collect();
                    let mut d = Diagnostic::new(format!("unknown workspace module `{}`", dep.module))
                        .in_file(module_file(module))
                        .at_key(format!("{}.{}", dep.scope.key(), dep.module));
                    if let Some(b) = closest(&dep.module, names.iter().copied()) {
                        d = d.with_help(format!("did you mean `{b}`?"));
                    }
                    if !diags.0.contains(&d) {
                        diags.push(d);
                    }
                }
            }
        }
    }
}

pub fn module_file(module: &Module) -> PathBuf {
    if module.dir == "." { PathBuf::from(MODULE_FILE) } else { Path::new(&module.dir).join(MODULE_FILE) }
}
