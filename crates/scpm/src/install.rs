//! `scpm install`: lock stale variants with coursier, then restore every locked artifact into
//! the coursier cache (design §5.2, §5.4, §5.5).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::coord::Coord;
use crate::coursier::{self, Auth, Repo, ResolveRequest, ResolvedArtifact};
use crate::credentials::CoursierCredentials;
use crate::fetch::{self, FetchItem, FetchOptions, FetchReport};
use crate::graph::Graph;
use crate::lockfile::{
    ArtifactLock, CatalogSelection, ClasspathEntry, LOCKFILE_VERSION, LockError, Lockfile, Platform as HostPlatform,
    ResolutionContextLock, Specifier, VariantLock,
};
use crate::machine::Machine;
use crate::platform::Platform;
use crate::semver::{Range, latest_release};
use crate::version::VersionSpec;
use crate::workspace::{Dependency, Scope, Variant, Workspace};

/// Zinc version whose binary compiler bridges are used for Scala 2 (Scala 3 ships its own).
pub const ZINC_VERSION: &str = "2.1.0";

#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    /// Fail instead of re-locking stale variants (the default when `CI=true`).
    pub frozen: bool,
    /// Re-hash cached files against the lock and re-download any that don't match.
    pub verify: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallSummary {
    /// Variants that were (re-)resolved.
    pub relocked: Vec<String>,
    /// Boot entries that were (re-)resolved.
    pub boot_relocked: Vec<String>,
    /// Variants dropped from the lock because they no longer exist.
    pub removed: Vec<String>,
    pub up_to_date: usize,
    pub lockfile_written: bool,
    /// The machine that ran the resolver, when anything was resolved. Phase 1's `cs` applies
    /// only the JDK part of the declared resolution context, so OS/arch-specific POM profiles
    /// follow this machine (design §5.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_on: Option<HostPlatform>,
    pub fetch: FetchReport,
    pub warnings: Vec<String>,
    pub resolve_ms: u128,
    pub fetch_ms: u128,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{0}")]
    Unsupported(String),
    #[error("{} is out of date for: {}\n  help: run `scpm install` and commit the result", crate::lockfile::LOCK_FILE, .0.join(", "))]
    Stale(Vec<String>),
    #[error(
        "{0}\n  help: resolve the merge conflict, or re-create {lock} locally with `scpm install` (not frozen, outside CI) and commit it",
        lock = crate::lockfile::LOCK_FILE
    )]
    CorruptLock(LockError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Coursier(#[from] coursier::CoursierError),
    #[error("{0}")]
    Resolve(String),
    #[error("{0}")]
    Fetch(fetch::FetchFailure),
    /// The user config is invalid (design §5.9).
    #[error("{0}")]
    MachineConfig(crate::diag::Diagnostics),
}

/// The workspace's repositories with the machine settings applied (design §5.9).
pub fn repositories(ws: &Workspace) -> Result<(Vec<Repo>, &'static Machine), InstallError> {
    let machine = crate::machine::current().map_err(InstallError::MachineConfig)?;
    let repos = coursier::repositories(&ws.repositories())?;
    Ok((machine.apply(repos), machine))
}

/// The four locked classpaths of a variant (design §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ClasspathKind {
    Compile,
    Runtime,
    TestCompile,
    TestRuntime,
}

impl ClasspathKind {
    pub const ALL: [ClasspathKind; 4] =
        [ClasspathKind::Compile, ClasspathKind::Runtime, ClasspathKind::TestCompile, ClasspathKind::TestRuntime];

    /// Own dependency scopes included in this classpath.
    pub(crate) fn own_scopes(self) -> &'static [Scope] {
        match self {
            ClasspathKind::Compile => &[Scope::Compile, Scope::Provided],
            ClasspathKind::Runtime => &[Scope::Compile, Scope::Runtime],
            ClasspathKind::TestCompile => &[Scope::Compile, Scope::Provided, Scope::Test],
            ClasspathKind::TestRuntime => &[Scope::Compile, Scope::Runtime, Scope::Provided, Scope::Test],
        }
    }

    /// Scopes an upstream module contributes (never its provided or test dependencies).
    pub(crate) fn upstream_scopes(self) -> &'static [Scope] {
        match self {
            ClasspathKind::Compile | ClasspathKind::TestCompile => &[Scope::Compile],
            ClasspathKind::Runtime | ClasspathKind::TestRuntime => &[Scope::Compile, Scope::Runtime],
        }
    }

    pub fn entries(self, lock: &VariantLock) -> &[ClasspathEntry] {
        match self {
            ClasspathKind::Compile => &lock.compile_classpath,
            ClasspathKind::Runtime => &lock.runtime_classpath,
            ClasspathKind::TestCompile => &lock.test_compile_classpath,
            ClasspathKind::TestRuntime => &lock.test_runtime_classpath,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ClasspathKind::Compile => "compile",
            ClasspathKind::Runtime => "runtime",
            ClasspathKind::TestCompile => "test-compile",
            ClasspathKind::TestRuntime => "test-runtime",
        }
    }
}

/// Upstream workspace variants on a classpath, in dependency order.
pub fn upstream_variants(graph: &Graph, id: &str, kind: ClasspathKind) -> Vec<String> {
    let mut found = BTreeSet::new();
    let mut frontier: Vec<String> =
        graph.dependencies_of(id).filter(|e| kind.own_scopes().contains(&e.scope)).map(|e| e.to.clone()).collect();
    while let Some(next) = frontier.pop() {
        if found.insert(next.clone()) {
            frontier.extend(
                graph
                    .dependencies_of(&next)
                    .filter(|e| kind.upstream_scopes().contains(&e.scope))
                    .map(|e| e.to.clone()),
            );
        }
    }
    graph.order.iter().filter(|v| found.contains(*v)).cloned().collect()
}

fn resolution_context(ws: &Workspace) -> ResolutionContextLock {
    let declared = ws.config.lock.resolution_context.clone().unwrap_or_default();
    let jdk = declared
        .jdk
        .map(|v| v.0)
        .or_else(|| ws.config.toolchain.jdk.as_deref().and_then(jdk_major))
        .unwrap_or_else(|| "25".to_string());
    ResolutionContextLock {
        os: declared.os.unwrap_or_else(|| "linux".into()),
        arch: declared.arch.unwrap_or_else(|| "x86_64".into()),
        jdk,
    }
}

/// `temurin:25.0.1` → `25`, `21` → `21`.
fn jdk_major(id: &str) -> Option<String> {
    let version = id.rsplit(':').next()?;
    let major: String = version.chars().take_while(char::is_ascii_digit).collect();
    (!major.is_empty()).then_some(major)
}

/// The inputs that determine a variant's own resolution (design §5.4, `inputsHash`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VariantInputs<'a> {
    id: &'a str,
    platform: Platform,
    scala: Option<&'a str>,
    dependencies: Vec<DependencyInput<'a>>,
    compiler_plugins: Vec<DependencyInput<'a>>,
    boms: Vec<String>,
    workspace_dependencies: Vec<(&'a Scope, &'a str)>,
}

/// What a downstream variant's joint resolution consumes from an upstream variant: its compile
/// and runtime dependencies, BOMs and Scala version (never test/provided deps or plugins).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamInputs<'a> {
    id: &'a str,
    platform: Platform,
    scala: Option<&'a str>,
    dependencies: Vec<DependencyInput<'a>>,
    boms: Vec<String>,
}

#[derive(Serialize)]
struct DependencyInput<'a> {
    scope: Scope,
    coord: String,
    /// The spec as written: it decides how the version is selected (a plain range reuses the
    /// variant's selection, `catalog` the workspace's), even when the effective range is equal.
    spec: &'a str,
    version: String,
    exclude: &'a [String],
    classifier: Option<&'a str>,
    kind: Option<&'a str>,
    transitive: bool,
}

impl<'a> From<&'a Dependency> for DependencyInput<'a> {
    fn from(d: &'a Dependency) -> Self {
        DependencyInput {
            scope: d.scope,
            coord: d.coord.to_string(),
            spec: &d.spec,
            version: d.version.to_string(),
            exclude: &d.exclude,
            classifier: d.classifier.as_deref(),
            kind: d.kind.as_deref(),
            transitive: d.transitive,
        }
    }
}

fn variant_inputs(v: &Variant) -> VariantInputs<'_> {
    VariantInputs {
        id: &v.id,
        platform: v.platform,
        scala: v.scala.as_ref().map(|s| s.full.as_str()),
        dependencies: v.dependencies.iter().map(DependencyInput::from).collect(),
        compiler_plugins: v.compiler_plugins.iter().map(DependencyInput::from).collect(),
        boms: v.boms.iter().map(|b| b.to_string()).collect(),
        workspace_dependencies: v.workspace_dependencies.iter().map(|d| (&d.scope, d.module.as_str())).collect(),
    }
}

fn upstream_inputs(v: &Variant) -> UpstreamInputs<'_> {
    UpstreamInputs {
        id: &v.id,
        platform: v.platform,
        scala: v.scala.as_ref().map(|s| s.full.as_str()),
        dependencies: v
            .dependencies
            .iter()
            .filter(|d| matches!(d.scope, Scope::Compile | Scope::Runtime))
            .map(DependencyInput::from)
            .collect(),
        boms: v.boms.iter().map(|b| b.to_string()).collect(),
    }
}

fn repo_inputs(repos: &[Repo]) -> Vec<(&str, &str)> {
    repos.iter().map(|r| (r.id.as_str(), r.url.as_str())).collect()
}

fn sha256_json(doc: &serde_json::Value) -> String {
    let digest = Sha256::digest(serde_json::to_vec(doc).expect("inputs serialize"));
    format!("sha256:{}", fetch::hex(&digest))
}

fn inputs_hash(ws: &Workspace, graph: &Graph, v: &Variant, repos: &[Repo], ctx: &ResolutionContextLock) -> String {
    // Each classpath's upstream closure separately, in order: an upstream workspace edge that is
    // added, removed or changes scope moves variants between closures without changing their union.
    let upstream: BTreeMap<&str, Vec<UpstreamInputs<'_>>> = ClasspathKind::ALL
        .iter()
        .map(|k| {
            let closure = upstream_variants(graph, &v.id, *k);
            (k.name(), closure.iter().filter_map(|id| ws.variant(id)).map(upstream_inputs).collect())
        })
        .collect();
    let overrides: Vec<String> = ws.config.overrides.iter().map(|(k, v)| format!("{k}:{v}")).collect();
    sha256_json(&serde_json::json!({
        "variant": variant_inputs(v),
        "upstream": upstream,
        "overrides": overrides,
        "repositories": repo_inputs(repos),
        "context": ctx,
        "minimumReleaseAge": ws.config.policy.minimum_release_age,
        // Rejected as unsupported for now (so always empty), but part of the contract.
        "minimumReleaseAgeExclude": ws.config.policy.minimum_release_age_exclude,
    }))
}

/// What every boot resolution depends on besides its own root (design §5.4, `bootInputs`).
fn boot_inputs_hash(repos: &[Repo], ctx: &ResolutionContextLock) -> String {
    sha256_json(&serde_json::json!({ "repositories": repo_inputs(repos), "context": ctx }))
}

/// Picks exact versions for ranges, sharing catalog selections workspace-wide (design §5.4).
struct Selector<'a> {
    repos: &'a [Repo],
    proxy: &'a crate::machine::ProxyChoice,
    cache: &'a Path,
    previous: &'a BTreeMap<String, CatalogSelection>,
    catalogs: Mutex<BTreeMap<String, CatalogSelection>>,
    listed: Mutex<HashMap<String, Vec<String>>>,
}

impl Selector<'_> {
    /// Returns the exact version to request for `dep` in `variant`.
    fn select(&self, variant: &Variant, dep: &Dependency) -> Result<String, InstallError> {
        let range = match &dep.version {
            VersionSpec::Exact { version } => return Ok(version.clone()),
            VersionSpec::Range { range } => range,
            other => unreachable!("catalog and workspace specs are resolved when loading: {other}"),
        };
        let parsed =
            Range::parse(range).map_err(|e| InstallError::Resolve(format!("{}: {}: {e}", variant.id, dep.coord)))?;
        let catalog_key = catalog_key(variant, dep);
        if let Some(key) = &catalog_key {
            if let Some(sel) = self.catalogs.lock().expect("catalogs").get(key) {
                return Ok(sel.version.clone());
            }
            if let Some(prev) = self.previous.get(key).filter(|p| &p.range == range) {
                self.catalogs.lock().expect("catalogs").insert(key.clone(), prev.clone());
                return Ok(prev.version.clone());
            }
        }
        let artifact = artifact_name(variant, &dep.coord);
        let list_key = format!("{}:{artifact}", dep.coord.org);
        let versions = {
            let cached = self.listed.lock().expect("listed").get(&list_key).cloned();
            match cached {
                Some(v) => v,
                None => {
                    let v = coursier::list_versions(&dep.coord.org, &artifact, self.repos, self.proxy, self.cache)?;
                    self.listed.lock().expect("listed").insert(list_key.clone(), v.clone());
                    v
                }
            }
        };
        let chosen = parsed.select(versions.iter().map(String::as_str)).ok_or_else(|| {
            InstallError::Resolve(format!(
                "{}: no version of {}:{artifact} matches `{range}` (latest release: {})",
                variant.id,
                dep.coord.org,
                latest_release(versions.iter().map(String::as_str)).unwrap_or("none found")
            ))
        })?;
        if let Some(key) = catalog_key {
            self.catalogs
                .lock()
                .expect("catalogs")
                .insert(key, CatalogSelection { range: range.clone(), version: chosen.to_string() });
        }
        Ok(chosen.to_string())
    }
}

/// `<catalog>/<key>@<platform>-<scala binary>` for catalog references, else `None`.
fn catalog_key(variant: &Variant, dep: &Dependency) -> Option<String> {
    let VersionSpec::Catalog { catalog } = VersionSpec::parse(&dep.spec) else { return None };
    let binary = variant.scala.as_ref().map(|s| s.binary.as_str()).unwrap_or("java");
    Some(format!("{}/{}@{}-{binary}", catalog.as_deref().unwrap_or("default"), dep.coord, variant.platform))
}

fn artifact_name(variant: &Variant, coord: &Coord) -> String {
    match &variant.scala {
        Some(s) => coord.artifact_name(&s.full, variant.platform),
        None => coord.name.clone(),
    }
}

/// Adds a dependency at a chosen version to a request.
fn add_dependency(req: &mut ResolveRequest, variant: &Variant, dep: &Dependency, version: &str) {
    let s = cs_dependency(variant, dep, version);
    if dep.transitive { req.dependencies.push(s) } else { req.intransitive.push(s) }
}

/// The coursier dependency string for a dependency at a chosen version.
fn cs_dependency(variant: &Variant, dep: &Dependency, version: &str) -> String {
    let mut s = format!("{}:{}:{version}", dep.coord.org, artifact_name(variant, &dep.coord));
    for ex in &dep.exclude {
        s.push_str(&format!(",exclude={}", ex.replacen(':', "%", 1)));
    }
    if let Some(c) = &dep.classifier {
        s.push_str(&format!(",classifier={c}"));
    }
    if let Some(t) = &dep.kind {
        s.push_str(&format!(",type={t}"));
    }
    s
}

/// The Scala library a variant compiles against; it is forced to the variant's version.
fn scala_library(variant: &Variant) -> Option<String> {
    let s = variant.scala.as_ref()?;
    Some(if s.binary == "3" {
        format!("org.scala-lang:scala3-library_3:{}", s.full)
    } else {
        format!("org.scala-lang:scala-library:{}", s.full)
    })
}

/// Picks the exact version to request for a dependency declared by a variant.
pub(crate) type VersionOf<'a> = dyn FnMut(&Variant, &Dependency) -> Result<String, InstallError> + 'a;

/// The joint coursier request for one classpath of a variant: its own dependencies in the
/// classpath's scopes plus what its upstream variants contribute (design §5.4).
pub(crate) fn classpath_request(
    ws: &Workspace,
    graph: &Graph,
    v: &Variant,
    kind: ClasspathKind,
    jdk: &str,
    version_of: &mut VersionOf<'_>,
) -> Result<ResolveRequest, InstallError> {
    let mut req = ResolveRequest { jdk_version: Some(jdk.to_string()), ..Default::default() };
    let mut boms: BTreeSet<String> = v.boms.iter().map(|b| bom_string(v, b)).collect();
    for dep in v.dependencies.iter().filter(|d| kind.own_scopes().contains(&d.scope)) {
        let version = version_of(v, dep)?;
        add_dependency(&mut req, v, dep, &version);
    }
    for up_id in upstream_variants(graph, &v.id, kind) {
        let up = ws.variant(&up_id).expect("graph ids are variants");
        boms.extend(up.boms.iter().map(|b| bom_string(up, b)));
        for dep in up.dependencies.iter().filter(|d| kind.upstream_scopes().contains(&d.scope)) {
            let version = version_of(up, dep)?;
            add_dependency(&mut req, up, dep, &version);
        }
    }
    let mut forced: Vec<String> = ws
        .config
        .overrides
        .iter()
        .filter_map(|(k, ver)| Coord::parse(k).ok().map(|c| format!("{}:{}:{ver}", c.org, artifact_name(v, &c))))
        .collect();
    if let Some(lib) = scala_library(v) {
        req.dependencies.insert(0, lib.clone());
        forced.push(lib);
    }
    req.boms = boms.into_iter().collect();
    req.forced = forced;
    Ok(req)
}

/// A boot closure (design §5.4): its lock name, root dependency, and whether it is transitive.
struct BootRoot {
    name: String,
    root: String,
    transitive: bool,
}

/// Boot closures (compiler and bridge) for a Scala version. The Scala 2 bridge comes from Zinc,
/// so its name carries the Zinc version and a Zinc upgrade re-resolves it.
fn boot_roots(full: &str) -> Vec<BootRoot> {
    let binary = crate::coord::scala_binary_version(full);
    let root = |name: String, root: String, transitive: bool| BootRoot { name, root, transitive };
    if binary == "3" {
        vec![
            root(format!("scala-compiler@{full}"), format!("org.scala-lang:scala3-compiler_3:{full}"), true),
            root(format!("compiler-bridge@{full}"), format!("org.scala-lang:scala3-sbt-bridge:{full}"), false),
        ]
    } else {
        vec![
            root(format!("scala-compiler@{full}"), format!("org.scala-lang:scala-compiler:{full}"), true),
            root(
                format!("compiler-bridge@{full}+zinc-{ZINC_VERSION}"),
                format!("org.scala-sbt:compiler-bridge_{binary}:{ZINC_VERSION}"),
                false,
            ),
        ]
    }
}

/// Reads the previous lock. A corrupt or merge-conflicted lock is re-built from scratch
/// (with a warning), except in frozen mode, where it is an error.
fn read_previous(root: &Path, frozen: bool, warnings: &mut Vec<String>) -> Result<Lockfile, InstallError> {
    match Lockfile::read(root) {
        Ok(lock) => Ok(lock.unwrap_or_default()),
        Err(e) if e.is_corrupt() && frozen => Err(InstallError::CorruptLock(e)),
        Err(e) if e.is_corrupt() => {
            warnings.push(format!("{e}; re-locking from scratch"));
            Ok(Lockfile::default())
        }
        Err(e) => Err(e.into()),
    }
}

/// Locks stale variants (unless frozen), writes the lockfile, and restores every locked
/// artifact into the coursier cache.
pub fn install(ws: &Workspace, graph: &Graph, opts: InstallOptions) -> Result<InstallSummary, InstallError> {
    let unsupported: Vec<&str> = ws.variants().filter(|v| v.platform != Platform::Jvm).map(|v| v.id.as_str()).collect();
    if !unsupported.is_empty() {
        return Err(InstallError::Unsupported(format!(
            "locking Scala.js/Native variants is not supported yet (planned for Phase 2): {}",
            unsupported.join(", ")
        )));
    }
    let (repos, machine) = repositories(ws)?;
    let cache = coursier::cache_dir();
    let ctx = resolution_context(ws);
    let mut warnings = machine.warnings.clone();
    let previous = read_previous(&ws.root, opts.frozen, &mut warnings)?;

    let hashes: BTreeMap<String, String> =
        ws.variants().map(|v| (v.id.clone(), inputs_hash(ws, graph, v, &repos, &ctx))).collect();
    let stale: Vec<String> = hashes
        .iter()
        .filter(|(id, hash)| previous.variants.get(*id).is_none_or(|l| &l.inputs_hash != *hash))
        .map(|(id, _)| id.clone())
        .collect();
    let scala_versions: BTreeSet<String> =
        ws.variants().filter_map(|v| v.scala.as_ref().map(|s| s.full.clone())).collect();
    let boot: Vec<BootRoot> = scala_versions.iter().flat_map(|v| boot_roots(v)).collect();
    let boot_inputs = (!boot.is_empty()).then(|| boot_inputs_hash(&repos, &ctx));
    let boot_inputs_changed = previous.boot_inputs != boot_inputs;
    let missing_boot: Vec<&BootRoot> =
        boot.iter().filter(|b| boot_inputs_changed || !previous.boot.contains_key(&b.name)).collect();
    let removed: Vec<String> = previous.variants.keys().filter(|id| !hashes.contains_key(*id)).cloned().collect();

    if opts.frozen && (!stale.is_empty() || !missing_boot.is_empty() || !removed.is_empty()) {
        let mut what = stale.clone();
        what.extend(missing_boot.iter().map(|b| b.name.clone()));
        what.extend(removed.iter().map(|id| format!("{id} (removed)")));
        return Err(InstallError::Stale(what));
    }

    let started = Instant::now();
    let mut lock = Lockfile {
        lockfile_version: LOCKFILE_VERSION,
        resolution_context: ctx.clone(),
        boot_inputs,
        boot: previous.boot.clone(),
        catalogs: BTreeMap::new(),
        variants: BTreeMap::new(),
        artifacts: previous.artifacts.clone(),
    };
    lock.boot.retain(|name, _| boot.iter().any(|b| &b.name == name));

    let scratch = tempfile_dir()?;
    let selector = Selector {
        repos: &repos,
        proxy: &machine.proxy,
        cache: &cache,
        previous: &previous.catalogs,
        catalogs: Mutex::new(BTreeMap::new()),
        listed: Mutex::new(HashMap::new()),
    };
    // Ranges are never re-selected implicitly (design §5.5): seed every dependency whose spec is
    // unchanged with the version the previous lock selected, stale variants included. Only new
    // or edited specs are selected. Catalog ranges go through the shared catalog selections.
    let mut chosen: HashMap<(String, String), String> = HashMap::new();
    for v in ws.variants() {
        let Some(prev) = previous.variants.get(&v.id) else { continue };
        for dep in v.dependencies.iter().chain(&v.compiler_plugins) {
            // Only plain ranges are seeded: exact versions need no selection (and an edited
            // catalog entry keeps the spec text `catalog`), and catalog ranges are shared
            // through the lock's catalog selections.
            if !matches!(dep.version, VersionSpec::Range { .. }) || catalog_key(v, dep).is_some() {
                continue;
            }
            if let Some(Specifier { spec, version: Some(version) }) = prev.specifiers.get(&dep.coord.to_string())
                && spec == &dep.spec
            {
                chosen.insert((v.id.clone(), dep.coord.to_string()), version.clone());
            }
        }
    }
    let mut version_of = |owner: &Variant, dep: &Dependency| -> Result<String, InstallError> {
        let key = (owner.id.clone(), dep.coord.to_string());
        if let Some(found) = chosen.get(&key) {
            return Ok(found.clone());
        }
        let version = selector.select(owner, dep)?;
        chosen.insert(key, version.clone());
        Ok(version)
    };

    // Build every resolution request (labelled for error messages), then run them concurrently.
    let mut requests: BTreeMap<(String, ClasspathKind), ResolveRequest> = BTreeMap::new();
    let mut specifiers: BTreeMap<String, BTreeMap<String, Specifier>> = BTreeMap::new();
    let mut plugin_requests: BTreeMap<String, ResolveRequest> = BTreeMap::new();
    let mut labels: Vec<(ResolveRequest, String)> = Vec::new();
    for id in &stale {
        let v = ws.variant(id).expect("stale ids come from the workspace");
        let mut specs = BTreeMap::new();
        for dep in v.dependencies.iter().chain(&v.compiler_plugins) {
            let version = version_of(v, dep)?;
            specs.entry(dep.coord.to_string()).or_insert(Specifier { spec: dep.spec.clone(), version: Some(version) });
        }
        for wd in &v.workspace_dependencies {
            specs.insert(wd.module.clone(), Specifier { spec: "workspace:*".into(), version: None });
        }
        specifiers.insert(id.clone(), specs);

        for kind in ClasspathKind::ALL {
            let req = classpath_request(ws, graph, v, kind, &ctx.jdk, &mut version_of)?;
            labels.push((req.clone(), format!("the {} classpath of {id}", kind.name())));
            requests.insert((id.clone(), kind), req);
        }
        if !v.compiler_plugins.is_empty() {
            let mut req = ResolveRequest { jdk_version: Some(ctx.jdk.clone()), ..Default::default() };
            for p in &v.compiler_plugins {
                let version = version_of(v, p)?;
                add_dependency(&mut req, v, p, &version);
            }
            labels.push((req.clone(), format!("the compiler plugins of {id}")));
            plugin_requests.insert(id.clone(), req);
        }
    }
    let boot_requests: BTreeMap<String, ResolveRequest> = missing_boot
        .iter()
        .map(|b| {
            let mut req = ResolveRequest { jdk_version: Some(ctx.jdk.clone()), ..Default::default() };
            if b.transitive {
                req.dependencies.push(b.root.clone())
            } else {
                req.intransitive.push(b.root.clone())
            }
            labels.push((req.clone(), format!("boot entry {}", b.name)));
            (b.name.clone(), req)
        })
        .collect();

    let all: Vec<&ResolveRequest> =
        requests.values().chain(plugin_requests.values()).chain(boot_requests.values()).collect();
    let results = resolve_concurrently(&all, &labels, &repos, &machine.proxy, &cache, scratch.path())?;

    record_artifacts(&mut lock, all.iter().flat_map(|req| &results[*req]), &repos)?;
    for (name, req) in &boot_requests {
        lock.boot.insert(name.clone(), results[req].iter().map(|a| a.key.clone()).collect());
    }

    for v in ws.variants() {
        let entry = if let Some(specs) = specifiers.remove(&v.id) {
            let classpath = |kind: ClasspathKind| -> Vec<ClasspathEntry> {
                let mut list: Vec<ClasspathEntry> = upstream_variants(graph, &v.id, kind)
                    .into_iter()
                    .map(|module| ClasspathEntry::Module { module })
                    .collect();
                list.extend(
                    results[&requests[&(v.id.clone(), kind)]].iter().map(|a| ClasspathEntry::Artifact(a.key.clone())),
                );
                list
            };
            VariantLock {
                specifiers: specs,
                inputs_hash: hashes[&v.id].clone(),
                compile_classpath: classpath(ClasspathKind::Compile),
                runtime_classpath: classpath(ClasspathKind::Runtime),
                test_compile_classpath: classpath(ClasspathKind::TestCompile),
                test_runtime_classpath: classpath(ClasspathKind::TestRuntime),
                compiler_plugins: plugin_requests
                    .get(&v.id)
                    .map(|r| results[r].iter().map(|a| a.key.clone()).collect())
                    .unwrap_or_default(),
            }
        } else {
            previous.variants[&v.id].clone()
        };
        lock.variants.insert(v.id.clone(), entry);
    }

    // Catalog selections: keep those still referenced by some variant.
    let mut used_catalogs = BTreeSet::new();
    for v in ws.variants() {
        for dep in v.dependencies.iter().chain(&v.compiler_plugins) {
            if let (Some(key), VersionSpec::Range { .. }) = (catalog_key(v, dep), &dep.version) {
                used_catalogs.insert(key);
            }
        }
    }
    lock.catalogs = previous.catalogs.clone();
    lock.catalogs.extend(selector.catalogs.into_inner().expect("catalogs"));
    lock.prune(&used_catalogs);
    let resolve_ms = started.elapsed().as_millis();

    let lockfile_written = if opts.frozen { false } else { lock.write(&ws.root)? };
    let resolved_on = (!stale.is_empty() || !missing_boot.is_empty()).then(HostPlatform::current);
    if let Some(host) = &resolved_on
        && (host.os != ctx.os || !same_arch(&host.arch, &ctx.arch))
    {
        warnings.push(format!(
            "resolved on {}-{}, but the lock declares {}-{}; OS/arch-specific POM profiles follow the resolving machine until the resolver sidecar lands (design §5.4)",
            host.os, host.arch, ctx.os, ctx.arch
        ));
    }

    let fetch_started = Instant::now();
    let items = fetch_items(&lock, &repos, &CoursierCredentials::load(), &cache)?;
    let fetch = fetch::fetch_all(items, FetchOptions { verify: opts.verify, proxy: machine.proxy.clone() })
        .map_err(InstallError::Fetch)?;
    if fetch.repaired > 0 {
        warnings.push(format!(
            "{} cached artifact{} did not match the lock and {} downloaded again",
            fetch.repaired,
            if fetch.repaired == 1 { "" } else { "s" },
            if fetch.repaired == 1 { "was" } else { "were" }
        ));
    }
    Ok(InstallSummary {
        relocked: stale.clone(),
        boot_relocked: missing_boot.iter().map(|b| b.name.clone()).collect(),
        removed,
        up_to_date: hashes.len() - stale.len(),
        lockfile_written,
        resolved_on,
        fetch,
        warnings,
        resolve_ms,
        fetch_ms: fetch_started.elapsed().as_millis(),
    })
}

fn same_arch(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> &str {
        match s {
            "amd64" | "x86_64" => "x86_64",
            "arm64" | "aarch64" => "aarch64",
            other => other,
        }
    }
    norm(a) == norm(b)
}

fn bom_string(v: &Variant, bom: &crate::coord::VersionedCoord) -> String {
    format!("{}:{}:{}", bom.coord.org, artifact_name(v, &bom.coord), bom.version)
}

pub(crate) fn tempfile_dir() -> Result<tempfile::TempDir, InstallError> {
    tempfile::Builder::new()
        .prefix("scpm-")
        .tempdir()
        .map_err(|e| InstallError::Resolve(format!("cannot create a temp dir: {e}")))
}

/// Runs distinct requests on a few threads (each is one `cs` process). `labels` names what
/// each request is for, so failures say which variant and classpath they belong to.
fn resolve_concurrently<'a>(
    requests: &[&'a ResolveRequest],
    labels: &[(ResolveRequest, String)],
    repos: &[Repo],
    proxy: &crate::machine::ProxyChoice,
    cache: &Path,
    scratch: &Path,
) -> Result<HashMap<&'a ResolveRequest, Vec<ResolvedArtifact>>, InstallError> {
    let mut distinct: Vec<&ResolveRequest> = Vec::new();
    for r in requests {
        if !distinct.contains(r) {
            distinct.push(r);
        }
    }
    let queue = Mutex::new(distinct.clone());
    let results: Mutex<HashMap<&ResolveRequest, Result<Vec<ResolvedArtifact>, coursier::CoursierError>>> =
        Mutex::new(HashMap::new());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let Some(req) = queue.lock().expect("queue").pop() else { break };
                    let r = coursier::resolve(req, repos, proxy, cache, scratch);
                    results.lock().expect("results").insert(req, r);
                }
            });
        }
    });
    let mut results = results.into_inner().expect("results");
    let mut out = HashMap::new();
    // Report failures in a stable order (the order requests were built in).
    for req in distinct {
        let result = results.remove(req).expect("every request ran");
        let resolved = result.map_err(|e| {
            let what: Vec<&str> = labels.iter().filter(|(r, _)| r == req).map(|(_, l)| l.as_str()).collect();
            let what = if what.is_empty() { "a request".to_string() } else { what.join(", ") };
            match e {
                coursier::CoursierError::Failed(msg) => {
                    InstallError::Resolve(format!("resolving {what} failed:\n{msg}"))
                }
                coursier::CoursierError::NoArtifact(_) => InstallError::Resolve(format!("resolving {what}: {e}")),
                other => other.into(),
            }
        })?;
        out.insert(req, resolved);
    }
    Ok(out)
}

/// Adds artifacts from this run's resolutions to the lock. The repository id is always
/// re-derived from where coursier downloaded the file (repositories may have changed since the
/// entry was written). An existing entry keeps its sha256 and size (artifacts are immutable)
/// only if the resolved file has them: the lock doesn't record repository URLs, so a
/// repository whose URL changed under the same id must be checked like one with a new id.
fn record_artifacts<'a>(
    lock: &mut Lockfile,
    resolved: impl IntoIterator<Item = &'a ResolvedArtifact>,
    repos: &[Repo],
) -> Result<(), InstallError> {
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    for a in resolved {
        if !seen.insert((&a.key, &a.url)) {
            continue;
        }
        let repo = repo_for_url(repos, &a.url).ok_or_else(|| {
            InstallError::Resolve(format!(
                "{} was downloaded from {}, which matches no configured repository or mirror",
                a.key, a.url
            ))
        })?;
        let path = coursier::maven_path(&a.key)
            .ok_or_else(|| InstallError::Resolve(format!("coursier resolved an invalid artifact key `{}`", a.key)))?;
        let expected = format!("{}{path}", repo.fetch_url());
        if expected != a.url {
            return Err(InstallError::Resolve(format!(
                "{}: unexpected repository layout (got {}, expected {expected}); snapshots and non-Maven layouts are not supported yet",
                a.key, a.url
            )));
        }
        let file_err = |e: std::io::Error| InstallError::Resolve(format!("{}: {e}", a.file.display()));
        let size = std::fs::metadata(&a.file).map_err(file_err)?.len();
        let sha256 = fetch::sha256_file(&a.file).map_err(file_err)?;
        match lock.artifacts.get_mut(&a.key) {
            // The locked integrity stays only if the repository serves the same bytes;
            // otherwise a restore could never pass the lock's checks.
            Some(existing) => {
                if (size, &sha256) != (existing.size, &existing.sha256) {
                    return Err(InstallError::Resolve(format!(
                        "{key}: {url} (repository `{new}`) serves different bytes than the copy locked from `{old}` \
                         (sha256 {sha256}, locked {locked}); a published artifact must not change\n  \
                         help: if the new bytes are expected, remove `{key}` from the `artifacts` of {lock_file} and run `scpm install`; \
                         if the cached file {file} is damaged, delete it and run `scpm install` again",
                        key = a.key,
                        url = a.url,
                        new = repo.id,
                        old = existing.repo,
                        locked = existing.sha256,
                        lock_file = crate::lockfile::LOCK_FILE,
                        file = a.file.display(),
                    )));
                }
                existing.repo = repo.id.clone();
            }
            None => {
                lock.artifacts.insert(a.key.clone(), ArtifactLock { repo: repo.id.clone(), sha256, size });
            }
        }
    }
    Ok(())
}

/// The configured repository serving `url`, matched against where it is downloaded from (its
/// mirror, if any), so a mirror maps back to the repository id. The longest prefix wins; between
/// repositories sharing a mirror, the first in resolution order.
fn repo_for_url<'a>(repos: &'a [Repo], url: &str) -> Option<&'a Repo> {
    let mut best: Option<&Repo> = None;
    for r in repos.iter().filter(|r| url.starts_with(r.fetch_url())) {
        if best.is_none_or(|b| r.fetch_url().len() > b.fetch_url().len()) {
            best = Some(r);
        }
    }
    best
}

/// How the fetcher reaches a repository: its credentials (read lazily; an error only matters
/// if something must be downloaded) and the `user@` segment coursier uses in cache paths.
struct RepoAccess {
    auth: Result<Option<Auth>, String>,
    /// Coursier credentials sent only after a Basic challenge (see [`FetchItem::challenge`]).
    challenge: Vec<(Option<String>, Auth)>,
    cache_user: CacheUser,
}

enum CacheUser {
    Anonymous,
    Known(String),
    /// Configured basic auth whose user variable is not set: find the file under any user.
    Unknown,
}

fn repo_access(repo: &Repo, creds: &CoursierCredentials) -> RepoAccess {
    if repo.credentials.is_some() {
        let cache_user = match repo.configured_user() {
            Some(user) => CacheUser::Known(user),
            None => CacheUser::Unknown,
        };
        return RepoAccess { auth: repo.auth().map_err(|e| e.to_string()), challenge: Vec::new(), cache_user };
    }
    // No configured credentials: use coursier's own, as `cs` would (design §5.7). Entries
    // scoped to a realm only apply after a challenge, so they don't name the cache directory.
    let basic =
        |c: &crate::credentials::CoursierCredential| Auth::Basic { user: c.user.clone(), password: c.password.clone() };
    let url = repo.fetch_url();
    let challenge = creds.challenge_candidates(url).into_iter().map(|c| (c.realm.clone(), basic(c))).collect();
    match creds.find(url) {
        Some(c) => RepoAccess { auth: Ok(Some(basic(c))), challenge, cache_user: CacheUser::Known(c.user.clone()) },
        None => RepoAccess { auth: Ok(None), challenge, cache_user: CacheUser::Anonymous },
    }
}

/// Every artifact the lock references, as download instructions.
pub fn fetch_items(
    lock: &Lockfile,
    repos: &[Repo],
    creds: &CoursierCredentials,
    cache: &Path,
) -> Result<Vec<FetchItem>, InstallError> {
    // Coursier's own credentials go over plain HTTP only to repositories that opt in.
    let creds = creds.restricted(&coursier::insecure_credential_origins(repos));
    let access: HashMap<&str, RepoAccess> = repos.iter().map(|r| (r.id.as_str(), repo_access(r, &creds))).collect();
    let mut items = Vec::new();
    for key in lock.referenced_artifacts() {
        let art = lock.artifacts.get(key).ok_or_else(|| {
            InstallError::Resolve(format!(
                "{} references {key}, which has no artifacts entry",
                crate::lockfile::LOCK_FILE
            ))
        })?;
        let repo = repos.iter().find(|r| r.id == art.repo).ok_or_else(|| {
            InstallError::Resolve(format!("{key} comes from repository `{}`, which is not configured", art.repo))
        })?;
        let path = coursier::maven_path(key).ok_or_else(|| {
            InstallError::Resolve(format!("{} has an invalid artifact key `{key}`", crate::lockfile::LOCK_FILE))
        })?;
        // The lock names the repository; a mirror only changes where the bytes come from.
        let url = format!("{}{path}", repo.fetch_url());
        let bad_url = || InstallError::Resolve(format!("{key}: {url} has no place in the coursier cache"));
        let access = &access[repo.id.as_str()];
        let mut verify_cached = false;
        let dest = match &access.cache_user {
            CacheUser::Anonymous => coursier::cache_path(cache, &url, None).ok_or_else(bad_url)?,
            CacheUser::Known(user) => coursier::cache_path(cache, &url, Some(user)).ok_or_else(bad_url)?,
            CacheUser::Unknown => match coursier::cached_with_any_user(cache, &url) {
                Some(found) => {
                    verify_cached = true;
                    found
                }
                None => coursier::cache_path(cache, &url, None).ok_or_else(bad_url)?,
            },
        };
        items.push(FetchItem {
            key: key.to_string(),
            url,
            dest,
            sha256: art.sha256.clone(),
            size: art.size,
            auth: access.auth.clone(),
            challenge: access.challenge.clone(),
            verify_cached,
        });
    }
    Ok(items)
}

/// Absolute classpath for a variant from the lock: cached jars, and upstream modules' class
/// directories (`.scpm/out/<id>/classes`).
pub fn classpath_files(
    ws: &Workspace,
    lock: &Lockfile,
    id: &str,
    kind: ClasspathKind,
) -> Result<Vec<PathBuf>, InstallError> {
    let (repos, _) = repositories(ws)?;
    let cache = coursier::cache_dir();
    let variant = lock.variants.get(id).ok_or_else(|| InstallError::Stale(vec![id.to_string()]))?;
    let by_key: HashMap<String, FetchItem> = fetch_items(lock, &repos, &CoursierCredentials::load(), &cache)?
        .into_iter()
        .map(|i| (i.key.clone(), i))
        .collect();
    Ok(kind
        .entries(variant)
        .iter()
        .map(|e| match e {
            ClasspathEntry::Module { module } => ws.root.join(".scpm/out").join(module).join("classes"),
            ClasspathEntry::Artifact(key) => by_key[key].dest.clone(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jdk_majors() {
        assert_eq!(jdk_major("temurin:25.0.1").as_deref(), Some("25"));
        assert_eq!(jdk_major("21").as_deref(), Some("21"));
        assert_eq!(jdk_major("zulu:").as_deref(), None);
    }

    #[test]
    fn boot_roots_per_scala_version() {
        let s3 = boot_roots("3.9.0");
        assert_eq!(s3[0].root, "org.scala-lang:scala3-compiler_3:3.9.0");
        assert_eq!(s3[1].root, "org.scala-lang:scala3-sbt-bridge:3.9.0");
        assert_eq!(s3[1].name, "compiler-bridge@3.9.0");
        let s2 = boot_roots("2.13.18");
        assert_eq!(s2[1].root, format!("org.scala-sbt:compiler-bridge_2.13:{ZINC_VERSION}"));
        assert_eq!(s2[1].name, format!("compiler-bridge@2.13.18+zinc-{ZINC_VERSION}"), "a Zinc upgrade re-resolves it");
    }

    #[test]
    fn boot_inputs_track_repositories_and_context() {
        let ctx = ResolutionContextLock { os: "linux".into(), arch: "x86_64".into(), jdk: "25".into() };
        let central = vec![Repo {
            id: "central".into(),
            url: coursier::CENTRAL_URL.into(),
            credentials: None,
            allow_insecure_credentials: false,
            ..Default::default()
        }];
        let other = vec![Repo {
            id: "apache".into(),
            url: "https://repo.maven.apache.org/maven2/".into(),
            credentials: None,
            allow_insecure_credentials: false,
            ..Default::default()
        }];
        let base = boot_inputs_hash(&central, &ctx);
        assert_eq!(base, boot_inputs_hash(&central, &ctx));
        assert_ne!(base, boot_inputs_hash(&other, &ctx));
        assert_ne!(base, boot_inputs_hash(&central, &ResolutionContextLock { jdk: "21".into(), ..ctx.clone() }));
    }

    /// Loads a workspace of `modules/<name>/scpm.yaml` files on Scala 3.9.0.
    fn workspace(modules: &[(&str, &str)]) -> (tempfile::TempDir, Workspace, Graph) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("scpm-workspace.yaml"), "modules: ['modules/*']\ntoolchain:\n  scala: 3.9.0\n")
            .unwrap();
        for (name, manifest) in modules {
            let module = dir.path().join("modules").join(name);
            std::fs::create_dir_all(&module).unwrap();
            std::fs::write(module.join("scpm.yaml"), manifest).unwrap();
        }
        let ws = crate::workspace::load(dir.path()).unwrap();
        let graph = Graph::build(&ws).unwrap();
        (dir, ws, graph)
    }

    fn hash_of(modules: &[(&str, &str)], id: &str) -> String {
        let (_dir, ws, graph) = workspace(modules);
        let ctx = resolution_context(&ws);
        inputs_hash(&ws, &graph, ws.variant(id).unwrap(), &[], &ctx)
    }

    #[test]
    fn upstream_edge_changes_reach_downstream_hashes() {
        // a compile-depends on b and runtime-depends on c; b then gains a compile edge to c,
        // which moves c into a's compile closure without changing the union of a's closures.
        let a = "dependencies:\n  b: workspace:*\nruntimeDependencies:\n  c: workspace:*\n";
        let c = "dependencies:\n  org.typelevel::cats-core: 2.13.0\n";
        let before = hash_of(&[("a", a), ("b", "dependencies:\n  g:x: '1.0'\n"), ("c", c)], "a@jvm-3");
        let after = hash_of(&[("a", a), ("b", "dependencies:\n  g:x: '1.0'\n  c: workspace:*\n"), ("c", c)], "a@jvm-3");
        assert_ne!(before, after);
        let runtime = hash_of(
            &[("a", a), ("b", "dependencies:\n  g:x: '1.0'\nruntimeDependencies:\n  c: workspace:*\n"), ("c", c)],
            "a@jvm-3",
        );
        assert_ne!(after, runtime, "an upstream edge changing scope re-locks downstream");
    }

    #[test]
    fn the_spec_as_written_is_hashed() {
        // The same effective range, selected through the catalog instead of the variant.
        let plain = "dependencies:\n  g:x: ^1.0\n";
        let via_catalog = "dependencies:\n  g:x: catalog\n";
        let (dir, _, _) = workspace(&[("a", plain)]);
        std::fs::write(
            dir.path().join("scpm-workspace.yaml"),
            "modules: ['modules/*']\ntoolchain:\n  scala: 3.9.0\ncatalog:\n  g:x: ^1.0\n",
        )
        .unwrap();
        let hash = |manifest: &str| {
            std::fs::write(dir.path().join("modules/a/scpm.yaml"), manifest).unwrap();
            let ws = crate::workspace::load(dir.path()).unwrap();
            let graph = Graph::build(&ws).unwrap();
            inputs_hash(&ws, &graph, ws.variant("a@jvm-3").unwrap(), &[], &resolution_context(&ws))
        };
        assert_ne!(hash(plain), hash(via_catalog));
    }

    #[test]
    fn a_repository_move_keeps_integrity_only_for_identical_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x-1.jar");
        std::fs::write(&file, b"bytes").unwrap();
        let repos = vec![
            Repo {
                id: "a".into(),
                url: "https://a.example/".into(),
                credentials: None,
                allow_insecure_credentials: false,
                ..Default::default()
            },
            Repo {
                id: "b".into(),
                url: "https://b.example/".into(),
                credentials: None,
                allow_insecure_credentials: false,
                ..Default::default()
            },
        ];
        let resolved = |repo: &str| ResolvedArtifact {
            key: "g:x:1".into(),
            file: file.clone(),
            url: format!("https://{repo}.example/g/x/1/x-1.jar"),
        };
        let mut lock = Lockfile::default();
        record_artifacts(&mut lock, &[resolved("a")], &repos).unwrap();
        let locked = lock.artifacts["g:x:1"].clone();
        record_artifacts(&mut lock, &[resolved("b")], &repos).unwrap();
        assert_eq!(lock.artifacts["g:x:1"], ArtifactLock { repo: "b".into(), ..locked.clone() });

        std::fs::write(&file, b"other").unwrap();
        let err = record_artifacts(&mut lock, &[resolved("a")], &repos).unwrap_err().to_string();
        assert!(err.contains("serves different bytes"), "{err}");
        assert_eq!(lock.artifacts["g:x:1"].repo, "b", "the lock entry is unchanged");

        // `b` now points at another URL serving other bytes: the same id is no excuse.
        let moved = vec![Repo { url: "https://a.example/".into(), ..repos[1].clone() }];
        let resolved_from_a = ResolvedArtifact { url: "https://a.example/g/x/1/x-1.jar".into(), ..resolved("a") };
        let err = record_artifacts(&mut lock, [&resolved_from_a], &moved).unwrap_err().to_string();
        assert!(err.contains("serves different bytes") && err.contains("repository `b`"), "{err}");
        std::fs::write(&file, b"bytes").unwrap();
        record_artifacts(&mut lock, &[resolved_from_a], &moved).unwrap();
        assert_eq!(lock.artifacts["g:x:1"], ArtifactLock { repo: "b".into(), ..locked });
    }

    #[test]
    fn invalid_lock_keys_are_errors() {
        let mut lock = Lockfile::default();
        lock.boot.insert("x".into(), vec!["g:..:1".into()]);
        lock.artifacts.insert("g:..:1".into(), ArtifactLock { repo: "central".into(), sha256: "0".into(), size: 1 });
        let repos = vec![Repo {
            id: "central".into(),
            url: coursier::CENTRAL_URL.into(),
            credentials: None,
            allow_insecure_credentials: false,
            ..Default::default()
        }];
        let err = fetch_items(&lock, &repos, &CoursierCredentials::default(), Path::new("/c")).unwrap_err();
        assert!(err.to_string().contains("invalid artifact key"), "{err}");
    }

    #[test]
    fn repo_matching_prefers_longest_prefix() {
        let repos = vec![
            Repo {
                id: "a".into(),
                url: "https://h/".into(),
                credentials: None,
                allow_insecure_credentials: false,
                ..Default::default()
            },
            Repo {
                id: "b".into(),
                url: "https://h/maven/".into(),
                credentials: None,
                allow_insecure_credentials: false,
                ..Default::default()
            },
        ];
        assert_eq!(repo_for_url(&repos, "https://h/maven/x.jar").unwrap().id, "b");
        assert_eq!(repo_for_url(&repos, "https://h/other/x.jar").unwrap().id, "a");
        assert!(repo_for_url(&repos, "https://elsewhere/x.jar").is_none());
    }

    #[test]
    fn mirror_urls_map_back_to_repository_ids() {
        let mirror = |url: &str| Some(coursier::Mirror { url: url.into(), matched: "*".into() });
        let repos = vec![
            Repo {
                id: "central".into(),
                url: "https://c/".into(),
                mirror: mirror("https://m/all/"),
                ..Default::default()
            },
            Repo {
                id: "acme".into(),
                url: "https://a/".into(),
                mirror: mirror("https://m/all/"),
                ..Default::default()
            },
            Repo {
                id: "own".into(),
                url: "https://o/".into(),
                mirror: mirror("https://m/all/own/"),
                ..Default::default()
            },
        ];
        assert_eq!(repo_for_url(&repos, "https://m/all/g/a/1/a-1.jar").unwrap().id, "central", "the first sharer");
        assert_eq!(repo_for_url(&repos, "https://m/all/own/g/a/1/a-1.jar").unwrap().id, "own");
        assert!(repo_for_url(&repos, "https://c/g/a/1/a-1.jar").is_none(), "not downloaded from the mirror");

        // The lock records the repository id, and the fetcher downloads from the mirror.
        let mut lock = Lockfile::default();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-1.jar");
        std::fs::write(&file, b"jar").unwrap();
        let resolved = ResolvedArtifact { key: "g:a:1".into(), file, url: "https://m/all/own/g/a/1/a-1.jar".into() };
        record_artifacts(&mut lock, [&resolved], &repos).unwrap();
        assert_eq!(lock.artifacts["g:a:1"].repo, "own");
        lock.boot.insert("b".into(), vec!["g:a:1".into()]);
        let items = fetch_items(&lock, &repos, &CoursierCredentials::default(), Path::new("/c")).unwrap();
        assert_eq!(items[0].url, "https://m/all/own/g/a/1/a-1.jar");
        assert_eq!(items[0].dest, Path::new("/c/https/m/all/own/g/a/1/a-1.jar"));
    }
}
