//! `scpm install`: lock stale variants with coursier, then restore every locked artifact into
//! the coursier cache (design §5.2, §5.4, §5.5).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::coord::{Coord, CrossKind};
use crate::coursier::{self, Auth, Repo, ResolveRequest, ResolvedArtifact};
use crate::credentials::CoursierCredentials;
use crate::fetch::{self, FetchItem, FetchOptions, FetchReport};
use crate::graph::Graph;
use crate::lockfile::{
    ArtifactLock, CatalogSelection, Classpath, ClasspathEntry, LOCKFILE_VERSION, LockEntry, LockError, Lockfile,
    Platform as HostPlatform, Specifier, VariantLock,
};
use crate::machine::Machine;
use crate::platform::Platform;
use crate::semver::{Range, latest_release};
use crate::version::VersionSpec;
use crate::workspace::{Dependency, Scope, Variant, Workspace};

/// Zinc version whose binary compiler bridges are used for Scala 2 (Scala 3 ships its own).
pub const ZINC_VERSION: &str = "2.1.0";

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Fail instead of re-locking stale variants (the default when `CI=true`).
    pub frozen: bool,
    /// Re-hash cached files against the lock and re-download any that don't match.
    pub verify: bool,
    /// Whose files to restore besides the host system's (design §5.4, "Restores").
    pub restore: RestoreSystems,
    /// Use scpm's current default system set even when the lock's header recorded another
    /// (`scpm install --reset-systems`); only when `lock.systems` is unset.
    pub reset_systems: bool,
}

/// Which systems' files a restore downloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RestoreSystems {
    #[default]
    Host,
    /// The host's and these (`scpm install --system`).
    Also(Vec<String>),
    /// Every system of the lock, and the host's (`scpm install --all-systems`).
    All,
    /// This system's only, not the host's (`scpm classpath --system`): the host may well be
    /// outside the lock's set, as when packaging for Linux on a Mac.
    Only(String),
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
    /// Information for `scpm install`'s summary, such as a host system outside the lock's
    /// system set (design §5.4). Commands that install first don't print these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Settings of other tools (Maven `settings.xml`, …) that touch the repositories this run
    /// contacted and that scpm ignored (design §5.9).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
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
    /// The system set, the host system or a JDK's system doesn't allow the command (design §5.4).
    #[error("{0}")]
    Systems(String),
    /// Starting or talking to the resolver sidecar failed (design §5.2.1).
    #[error(transparent)]
    Sidecar(#[from] crate::resolver::ResolverError),
    #[error("{0}")]
    Fetch(fetch::FetchFailure),
    /// The user config is invalid (design §5.9).
    #[error("{0}")]
    MachineConfig(crate::diag::Diagnostics),
    /// An error from contacting the repositories, with the notice about ignored settings of
    /// other tools that may be why (design §5.9).
    #[error("{error}\n  note: {notice}")]
    WithNotice { error: Box<InstallError>, notice: String },
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

    pub fn classpath(self, lock: &VariantLock) -> &Classpath {
        match self {
            ClasspathKind::Compile => &lock.compile_classpath,
            ClasspathKind::Runtime => &lock.runtime_classpath,
            ClasspathKind::TestCompile => &lock.test_compile_classpath,
            ClasspathKind::TestRuntime => &lock.test_runtime_classpath,
        }
    }

    /// The classpath's entries, from a projected lock ([`ensure`] returns one): the host
    /// system's classpath. On a lock that isn't projected, only the entries on every system.
    pub fn entries(self, lock: &VariantLock) -> Vec<&ClasspathEntry> {
        self.classpath(lock).common()
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

/// Upstream workspace variants on a classpath by dependency distance from `id` (direct ones
/// first), ties broken by id.
fn upstream_by_distance(graph: &Graph, id: &str, kind: ClasspathKind) -> Vec<String> {
    let mut dist: BTreeMap<String, usize> = BTreeMap::new();
    let mut queue: std::collections::VecDeque<(String, usize)> =
        graph.dependencies_of(id).filter(|e| kind.own_scopes().contains(&e.scope)).map(|e| (e.to.clone(), 1)).collect();
    while let Some((next, d)) = queue.pop_front() {
        if dist.contains_key(&next) {
            continue;
        }
        dist.insert(next.clone(), d);
        queue.extend(
            graph
                .dependencies_of(&next)
                .filter(|e| kind.upstream_scopes().contains(&e.scope))
                .map(|e| (e.to.clone(), d + 1)),
        );
    }
    let mut ids: Vec<(usize, String)> = dist.into_iter().map(|(id, d)| (d, id)).collect();
    ids.sort();
    ids.into_iter().map(|(_, id)| id).collect()
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

/// What a lock is resolved for: its system set and JDK (design §5.4, "Systems").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockContext {
    /// Sorted.
    pub systems: Vec<String>,
    /// The default set's version when the set is scpm's default (`lock.systems` unset).
    pub default_systems: Option<u32>,
    pub jdk: String,
}

/// The system set and JDK for a lock (design §5.4, "What decides the system set"):
/// `lock.systems` when set; else the set the previous lock's header recorded from a default set
/// (`defaultSystems`), so a newer binary's default never makes a lock stale; else (a new lock, a
/// header written from a `lock.systems` that has since been removed, or `reset_systems`)
/// scpm's current default set.
pub fn lock_context(ws: &Workspace, previous: &Lockfile, reset_systems: bool) -> LockContext {
    let jdk = ws
        .config
        .lock
        .jdk
        .as_ref()
        .map(|v| v.0.clone())
        .or_else(|| ws.config.toolchain.jdk.as_deref().and_then(jdk_major))
        .unwrap_or_else(|| "25".to_string());
    let (systems, default_systems) = match &ws.config.lock.systems {
        Some(list) => (crate::system::normalized(list.iter().cloned()), None),
        None => match previous.default_systems {
            Some(version) if !reset_systems && !previous.systems.is_empty() => {
                (crate::system::normalized(previous.systems.iter().cloned()), Some(version))
            }
            _ => (
                crate::system::normalized(crate::system::DEFAULT_SYSTEMS.iter().copied()),
                Some(crate::system::DEFAULT_SYSTEMS_VERSION),
            ),
        },
    };
    LockContext { systems, default_systems, jdk }
}

/// The resolution context as hashed into `inputsHash` and `bootInputs`. A single system hashes
/// exactly as a version 1 lock's `resolutionContext` did (`{os, arch, jdk}`), so converting a
/// v1 lock re-locks nothing (design §5.4).
fn context_input(ctx: &LockContext) -> serde_json::Value {
    #[derive(Serialize)]
    struct Single<'a> {
        os: &'a str,
        arch: &'a str,
        jdk: &'a str,
    }
    match ctx.systems.as_slice() {
        [one] => {
            let (os, arch) = one.split_once('-').unwrap_or((one.as_str(), ""));
            serde_json::to_value(Single { os, arch, jdk: &ctx.jdk }).expect("context serializes")
        }
        systems => serde_json::json!({ "systems": systems, "jdk": ctx.jdk }),
    }
}

/// Scala version used to drive Zinc for Java-only modules when the workspace has no Scala.
pub const JAVA_ONLY_SCALA: &str = "2.13.18";

/// The Scala version that drives Zinc for a variant (Java-only variants borrow one).
pub fn compiler_scala(ws: &Workspace, v: &Variant) -> String {
    v.scala.as_ref().map(|s| s.full.clone()).unwrap_or_else(|| {
        ws.variants()
            .find_map(|o| o.scala.as_ref().map(|s| s.full.clone()))
            .unwrap_or_else(|| JAVA_ONLY_SCALA.to_string())
    })
}

/// Boot entry name of the compiler bridge for a Scala version. The Scala 2 bridge comes from
/// Zinc, so its name carries the Zinc version and a Zinc upgrade re-resolves it.
pub fn bridge_boot(scala: &str) -> String {
    if crate::coord::scala_binary_version(scala) == "3" {
        format!("compiler-bridge@{scala}")
    } else {
        format!("compiler-bridge@{scala}+zinc-{ZINC_VERSION}")
    }
}

/// `temurin:25.0.1` → `25`, `21` → `21`.
pub fn jdk_major(id: &str) -> Option<String> {
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
    boms: Vec<DependencyInput<'a>>,
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
    boms: Vec<DependencyInput<'a>>,
}

#[derive(Serialize)]
struct DependencyInput<'a> {
    scope: Scope,
    coord: String,
    /// The spec as written: it decides how the version is selected (a plain range reuses the
    /// variant's selection, `catalog` the workspace's), even when the effective range is equal;
    /// `catalog:a` and `catalog:b` may also hold the same range but have different locked
    /// selections, so switching between them must re-lock.
    spec: &'a str,
    version: String,
    exclude: &'a [String],
    classifier: Option<&'a str>,
    kind: Option<&'a str>,
    transitive: bool,
    /// `natives:`, left out when absent so hashes are what they were before the key existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    natives: Option<&'a BTreeMap<String, String>>,
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
            natives: d.natives.as_ref(),
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
        boms: v.boms.iter().map(DependencyInput::from).collect(),
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
        boms: v.boms.iter().map(DependencyInput::from).collect(),
    }
}

/// Each repository's id and URL, and its `groups` when it has any (left out otherwise, so
/// locks written before `groups` existed stay fresh).
fn repo_inputs(repos: &[Repo]) -> Vec<serde_json::Value> {
    repos
        .iter()
        .map(|r| match r.groups.as_slice() {
            [] => serde_json::json!([r.id, r.url]),
            groups => serde_json::json!([r.id, r.url, groups]),
        })
        .collect()
}

fn sha256_json(doc: &serde_json::Value) -> String {
    let digest = Sha256::digest(serde_json::to_vec(doc).expect("inputs serialize"));
    format!("sha256:{}", fetch::hex(&digest))
}

fn inputs_hash(ws: &Workspace, graph: &Graph, v: &Variant, repos: &[Repo], ctx: &LockContext) -> String {
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
    // Which upstream BOM wins depends on dependency distance, so a topology change that
    // reorders the BOM owners must re-lock even when the upstream set is the same.
    let bom_owners: Vec<Vec<String>> = ClasspathKind::ALL
        .iter()
        .map(|k| {
            upstream_by_distance(graph, &v.id, *k)
                .into_iter()
                .filter(|id| ws.variant(id).is_some_and(|u| !u.boms.is_empty()))
                .collect()
        })
        .collect();
    let mut inputs = serde_json::json!({
        "variant": variant_inputs(v),
        "upstream": upstream,
        "bomOwners": bom_owners,
        "overrides": overrides,
        "repositories": repo_inputs(repos),
        "context": context_input(ctx),
        "minimumReleaseAge": ws.config.policy.minimum_release_age,
        // Rejected as unsupported for now (so always empty), but part of the contract.
        "minimumReleaseAgeExclude": ws.config.policy.minimum_release_age_exclude,
    });
    // The whole workspace list, in every JVM variant's hash; absent, it adds nothing, so hashes
    // stay what they were before the key existed. Built-in mappings are never hashed: the
    // header's `nativeMappings` versions them (design §5.4).
    if v.platform == Platform::Jvm
        && let Some(list) = &ws.config.lock.native_classifiers
    {
        inputs["nativeClassifiers"] = serde_json::to_value(list).expect("serializes");
    }
    sha256_json(&inputs)
}

/// What every boot resolution depends on besides its own root (design §5.4, `bootInputs`).
fn boot_inputs_hash(repos: &[Repo], ctx: &LockContext) -> String {
    sha256_json(&serde_json::json!({ "repositories": repo_inputs(repos), "context": context_input(ctx) }))
}

/// Picks exact versions for ranges, sharing catalog selections workspace-wide (design §5.4).
struct Selector<'a> {
    repos: &'a [Repo],
    proxy: &'a crate::machine::ProxyChoice,
    cache: &'a Path,
    previous: &'a BTreeMap<String, CatalogSelection>,
    catalogs: Mutex<BTreeMap<String, CatalogSelection>>,
    listed: Mutex<HashMap<String, Vec<String>>>,
    sidecar: &'a LazySidecar<'a>,
}

/// The resolver sidecar, started at the first request that needs it and used for the rest of
/// the command (design §5.2.1, "Lifecycle"); never for `cs`.
pub(crate) struct LazySidecar<'a> {
    resolver: Resolver,
    machine: &'a crate::machine::Machine,
    repos: &'a [Repo],
    cell: std::cell::OnceCell<crate::resolver::Sidecar>,
}

impl<'a> LazySidecar<'a> {
    fn new(resolver: Resolver, machine: &'a crate::machine::Machine, repos: &'a [Repo]) -> Self {
        LazySidecar { resolver, machine, repos, cell: std::cell::OnceCell::new() }
    }

    fn get(&self) -> Result<Option<&crate::resolver::Sidecar>, InstallError> {
        if self.resolver != Resolver::Sidecar {
            return Ok(None);
        }
        if let Some(s) = self.cell.get() {
            return Ok(Some(s));
        }
        // As for `cs`: a repository that needs an unusable environment proxy fails up front.
        if let Some(p) = &self.machine.proxy {
            for r in self.repos {
                p.route(r.fetch_url()).map_err(|e| InstallError::Coursier(coursier::CoursierError::Config(e)))?;
            }
        }
        let started = crate::resolver::Sidecar::start(self.machine)?;
        let _ = self.cell.set(started);
        Ok(self.cell.get())
    }

    /// Stops the sidecar if it was started.
    fn finish(self) {
        if let Some(sidecar) = self.cell.into_inner() {
            sidecar.shutdown();
        }
    }
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
                    // Only the repositories its group comes from are asked (design §5.4).
                    let allowed: Vec<Repo> =
                        coursier::allowed_repos(self.repos, &dep.coord.org).into_iter().cloned().collect();
                    if allowed.is_empty() {
                        return Err(InstallError::Resolve(format!(
                            "{}: {}: {}",
                            variant.id,
                            dep.coord,
                            coursier::no_repository_for(&dep.coord.org)
                        )));
                    }
                    // Contacting the repositories: an ignored setting of another tool may be why it fails.
                    let v = match self.sidecar.get()? {
                        // The sidecar routes by `groups` itself, so it gets every repository.
                        Some(sidecar) => sidecar
                            .list_versions(&list_key, self.repos, self.cache)
                            .map_err(|e| with_notice(e.into(), self.repos, true))?,
                        None => coursier::list_versions(&dep.coord.org, &artifact, &allowed, self.proxy, self.cache)
                            .map_err(|e| with_notice(e.into(), self.repos, true))?,
                    };
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

/// `<catalog>/<key>@<platform>-<scala binary>` for catalog references, else `None`. A Java
/// coordinate (`org:name`) names the same artifact on every Scala version, so it gets one
/// selection per platform (`@<platform>-java`).
fn catalog_key(variant: &Variant, dep: &Dependency) -> Option<String> {
    let VersionSpec::Catalog { catalog } = VersionSpec::parse(&dep.spec) else { return None };
    let binary = match &variant.scala {
        Some(s) if dep.coord.cross != CrossKind::Java => s.binary.as_str(),
        _ => "java",
    };
    Some(format!("{}/{}@{}-{binary}", catalog.as_deref().unwrap_or("default"), dep.coord, variant.platform))
}

/// Locks written before Java coordinates shared one catalog selection keyed them by the
/// variant's Scala version. Moves such selections to the shared key (when it has none yet), so
/// they survive pruning and are honoured: a range is never re-selected implicitly (§5.5).
///
/// Legacy selections of one coordinate for different Scala versions can disagree. They can't
/// all survive under one shared key, so the first (in variant order) is kept and the others
/// are named in a warning: those variants move to the shared version on their next re-lock.
fn migrate_catalog_keys(ws: &Workspace, catalogs: &mut BTreeMap<String, CatalogSelection>, warnings: &mut Vec<String>) {
    let mut migrated: BTreeMap<String, String> = BTreeMap::new();
    for v in ws.variants() {
        for dep in versioned(v) {
            let (Some(key), Some(legacy)) = (catalog_key(v, dep), legacy_catalog_key(v, dep)) else { continue };
            let Some(sel) = catalogs.get(&legacy).cloned() else { continue };
            match catalogs.get(&key) {
                None => {
                    catalogs.insert(key.clone(), sel);
                    migrated.insert(key, legacy);
                }
                // Also when the shared key came from elsewhere (e.g. a Java-only module): the
                // legacy selection is about to be pruned either way.
                Some(shared) if shared.version != sel.version => {
                    let warning = format!(
                        "{}: the lock selected {} for {} (`{legacy}`) and {} (`{}`); Java coordinates now share one selection, so {} moves to {} when it re-locks",
                        dep.coord,
                        sel.version,
                        v.id,
                        shared.version,
                        migrated.get(&key).unwrap_or(&key),
                        v.id,
                        shared.version
                    );
                    if !warnings.contains(&warning) {
                        warnings.push(warning);
                    }
                }
                Some(_) => {}
            }
        }
    }
}

/// The pre-sharing catalog key of a Java coordinate in a Scala variant, if it differs.
fn legacy_catalog_key(variant: &Variant, dep: &Dependency) -> Option<String> {
    let VersionSpec::Catalog { catalog } = VersionSpec::parse(&dep.spec) else { return None };
    let scala = variant.scala.as_ref().filter(|_| dep.coord.cross == CrossKind::Java)?;
    Some(format!("{}/{}@{}-{}", catalog.as_deref().unwrap_or("default"), dep.coord, variant.platform, scala.binary))
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
    for dep in v.dependencies.iter().filter(|d| kind.own_scopes().contains(&d.scope)) {
        let version = version_of(v, dep)?;
        add_dependency(&mut req, v, dep, &version);
    }
    let upstreams = upstream_variants(graph, &v.id, kind);
    // One version per BOM coordinate (coursier applies the first `--bom` it is given): the
    // variant's own BOM wins, then upstreams by dependency distance (nearest first).
    let mut boms: Vec<String> = Vec::new();
    let mut bom_coords = BTreeSet::new();
    let nearest = upstream_by_distance(graph, &v.id, kind);
    for owner in std::iter::once(v).chain(nearest.iter().map(|id| ws.variant(id).expect("graph ids are variants"))) {
        for bom in &owner.boms {
            if bom_coords.insert(format!("{}:{}", bom.coord.org, artifact_name(owner, &bom.coord))) {
                boms.push(bom_string(owner, bom, &version_of(owner, bom)?));
            }
        }
    }
    for up_id in upstreams {
        let up = ws.variant(&up_id).expect("graph ids are variants");
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
    req.boms = boms;
    req.forced = forced;
    req.natives = native_specs(ws, graph, v, kind);
    Ok(req)
}

/// The native mappings for one classpath of a JVM variant (design §5.4): its dependencies'
/// `natives:` (the variant's own, then upstream ones), then `lock.nativeClassifiers`, each
/// replacing the built-in mapping for its module.
/// Mapped modules on the host system's locked classpaths whose native jar for the host isn't
/// there, read from the lock alone: for each variant the sidecar locked against this table
/// (`fullContext`, and the header's `nativeMappings` equal to the table's version), each main
/// jar whose mapping (built-in, or the manifest's override) names a classifier for the host.
fn host_native_warnings(
    ws: &Workspace,
    graph: &Graph,
    lock: &Lockfile,
    host: &str,
    table: &crate::natives::Table,
) -> Vec<String> {
    if lock.native_mappings != Some(table.version) || !lock.systems.iter().any(|s| s == host) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (id, vlock) in lock.variants.iter().filter(|(_, l)| l.full_context) {
        let Some(v) = ws.variant(id) else { continue };
        for kind in ClasspathKind::ALL {
            let specs = native_specs(ws, graph, v, kind);
            let entries = kind.classpath(vlock).project(host);
            let keys: BTreeSet<&str> = entries
                .iter()
                .filter_map(|e| match e {
                    ClasspathEntry::Artifact(k) => Some(k.as_str()),
                    ClasspathEntry::Module { .. } => None,
                })
                .collect();
            for key in keys.iter().filter(|k| k.split(':').count() == 3 && !k.contains('@')) {
                let mut parts = key.split(':');
                let (Some(g), Some(a), Some(ver)) = (parts.next(), parts.next(), parts.next()) else { continue };
                let module = format!("{g}:{a}");
                let classifier = specs
                    .iter()
                    .filter(|s| s.module == module)
                    .filter(|s| s.versions.as_deref().is_none_or(|i| interval_contains(i, ver)))
                    .find_map(|s| s.natives.get(host));
                if let Some(c) = classifier
                    && !keys.contains(format!("{key}:{c}").as_str())
                {
                    let w = format!(
                        "{key} has no native jar for {host} (`{c}`) in {}: the repository didn't serve it when \
                         {id} was locked, so {id} runs without it here\n  help: once the repository (or the mirror \
                         you reach it through) serves it, remove {id} from {} and run `scpm install`",
                        crate::lockfile::LOCK_FILE,
                        crate::lockfile::LOCK_FILE
                    );
                    if !out.contains(&w) {
                        out.push(w);
                    }
                }
            }
        }
    }
    out
}

fn native_specs(ws: &Workspace, graph: &Graph, v: &Variant, kind: ClasspathKind) -> Vec<crate::natives::NativeSpec> {
    if v.platform != Platform::Jvm {
        return Vec::new();
    }
    let mut overrides: Vec<(String, BTreeMap<String, String>)> = Vec::new();
    let mut add = |owner: &Variant, dep: &Dependency| {
        if let Some(natives) = &dep.natives {
            overrides.push((format!("{}:{}", dep.coord.org, artifact_name(owner, &dep.coord)), natives.clone()));
        }
    };
    for dep in v.dependencies.iter().filter(|d| kind.own_scopes().contains(&d.scope)) {
        add(v, dep);
    }
    for up in upstream_variants(graph, &v.id, kind).iter().filter_map(|id| ws.variant(id)) {
        for dep in up.dependencies.iter().filter(|d| kind.upstream_scopes().contains(&d.scope)) {
            add(up, dep);
        }
    }
    for n in ws.config.lock.native_classifiers.iter().flatten() {
        overrides.push((n.module.clone(), n.natives.clone()));
    }
    crate::natives::specs(&crate::natives::BUILT_IN, &overrides)
}

/// A boot closure (design §5.4): its lock name, root dependencies, and whether they are fetched transitively.
struct BootRoot {
    name: String,
    roots: Vec<String>,
    transitive: bool,
}

/// Boot closures (compiler and bridge) for a Scala version. The Scala 2 bridge comes from Zinc,
/// so its name carries the Zinc version and a Zinc upgrade re-resolves it.
fn boot_roots(full: &str) -> Vec<BootRoot> {
    let binary = crate::coord::scala_binary_version(full);
    let root = |name: String, root: String, transitive: bool| BootRoot { name, roots: vec![root], transitive };
    if binary == "3" {
        vec![
            root(format!("scala-compiler@{full}"), format!("org.scala-lang:scala3-compiler_3:{full}"), true),
            root(bridge_boot(full), format!("org.scala-lang:scala3-sbt-bridge:{full}"), false),
        ]
    } else {
        vec![
            root(format!("scala-compiler@{full}"), format!("org.scala-lang:scala-compiler:{full}"), true),
            root(bridge_boot(full), format!("org.scala-sbt:compiler-bridge_{binary}:{ZINC_VERSION}"), false),
        ]
    }
}

/// The newest scalameta release that publishes `semanticdb-scalac` for a Scala 2 version (the
/// plugin is published per full Scala version, each release for a window of them), from Maven
/// Central as of 2026-11. `None` for versions without a 4.x plugin, and for versions newer
/// than this table.
pub fn semanticdb_version(scala: &str) -> Option<&'static str> {
    let parts: Vec<u32> = scala.split(['.', '-']).take(3).filter_map(|p| p.parse().ok()).collect();
    let [2, minor, patch] = parts[..] else { return None };
    Some(match (minor, patch) {
        (13, 0) => "4.6.0",
        (13, 1..=7) => "4.8.4",
        (13, 8) => "4.8.10",
        (13, 9) => "4.9.0",
        (13, 10) => "4.9.3",
        (13, 11) => "4.9.9",
        (13, 12) => "4.12.3",
        (13, 13) => "4.13.10",
        (13, 14) => "4.14.1",
        (13, 15..=18) => "4.17.4",
        (12, 4..=6) => "4.1.9",
        (12, 7) => "4.2.3",
        (12, 8) => "4.5.13",
        (12, 9..=14) => "4.8.4",
        (12, 15) => "4.9.0",
        (12, 16) => "4.9.9",
        (12, 17) => "4.14.2",
        (12, 18..=21) => "4.17.4",
        _ => return None,
    })
}

/// Whether the workspace asks for SemanticDB output (`ide.semanticdb`).
pub fn semanticdb_enabled(ws: &Workspace) -> bool {
    ws.config.ide.as_ref().and_then(|i| i.semanticdb).unwrap_or(false)
}

/// Boot entry name of the SemanticDB compiler plugin for a Scala 2 version (`None` if no
/// plugin is published for it).
pub fn semanticdb_boot(scala: &str) -> Option<String> {
    semanticdb_version(scala).map(|v| format!("semanticdb@{scala}+{v}"))
}

/// The SemanticDB plugin closures (`ide.semanticdb`): one per Scala 2 version of a Scala
/// variant (Scala 3 compilers write SemanticDB themselves). A Scala 2 version without a
/// published plugin is an error naming it, rather than a resolution failure.
fn semanticdb_roots(ws: &Workspace) -> Result<Vec<BootRoot>, InstallError> {
    if !semanticdb_enabled(ws) {
        return Ok(Vec::new());
    }
    let versions: BTreeSet<&str> =
        ws.variants().filter_map(|v| v.scala.as_ref()).filter(|s| s.binary != "3").map(|s| s.full.as_str()).collect();
    versions
        .into_iter()
        .map(|full| {
            let version = semanticdb_version(full).ok_or_else(|| {
                InstallError::Unsupported(format!(
                    "`ide.semanticdb` needs the semanticdb-scalac plugin, which scpm knows for Scala 2.12.4–2.12.21 and 2.13.0–2.13.18, not Scala {full}\n  help: use a supported Scala version or turn `ide.semanticdb` off"
                ))
            })?;
            Ok(BootRoot {
                name: format!("semanticdb@{full}+{version}"),
                roots: vec![format!("org.scalameta:semanticdb-scalac_{full}:{version}")],
                transitive: false,
            })
        })
        .collect()
}

/// Boot entry name for a JVM helper: keyed by a hash of its dependency list, so changing the
/// list (a new scpm version) re-locks it.
pub fn helper_boot_name(helper: &str, deps: &str) -> String {
    let digest = Sha256::digest(deps.as_bytes());
    format!("{helper}@{}", &fetch::hex(&digest)[..12])
}

/// The compile worker's boot entry name.
pub fn worker_boot() -> String {
    helper_boot_name("worker", crate::worker::COMPILE_WORKER_DEPS)
}

/// The test runner's boot entry name.
pub fn test_runner_boot() -> String {
    helper_boot_name("test-runner", crate::worker::TEST_RUNNER_DEPS)
}

fn helper_roots() -> Vec<BootRoot> {
    let lines = |text: &str| -> Vec<String> {
        text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string).collect()
    };
    vec![
        BootRoot { name: worker_boot(), roots: lines(crate::worker::COMPILE_WORKER_DEPS), transitive: true },
        BootRoot { name: test_runner_boot(), roots: lines(crate::worker::TEST_RUNNER_DEPS), transitive: true },
    ]
}

/// Install-before-run (design §5.4): installs (frozen when asked or in CI), then reads the lock
/// as the host system sees it ([`Lockfile::project`]).
pub fn ensure(ws: &Workspace, graph: &Graph, frozen: bool) -> Result<(Lockfile, InstallSummary), InstallError> {
    ensure_for(ws, graph, frozen, None)
}

/// [`ensure`] for `system` instead of the host (`--system`): that system's files are restored
/// too, and the lock is its projection.
pub fn ensure_for(
    ws: &Workspace,
    graph: &Graph,
    frozen: bool,
    system: Option<&str>,
) -> Result<(Lockfile, InstallSummary), InstallError> {
    let restore = match system {
        Some(s) => RestoreSystems::Only(s.to_string()),
        None => RestoreSystems::Host,
    };
    let opts = InstallOptions { frozen: frozen || in_ci(), restore, ..Default::default() };
    let summary = install(ws, graph, opts)?;
    let lock = Lockfile::read(&ws.root)?.ok_or_else(|| InstallError::Stale(vec!["(no scpm-lock.yaml)".into()]))?;
    let system = system.unwrap_or(&crate::system::host().system);
    let (lock, _) = projection(&lock, system)?;
    Ok((lock, summary))
}

/// The host system's view of a lock ([`projection`]).
pub fn host_projection(lock: &Lockfile) -> Result<(Lockfile, Option<String>), InstallError> {
    projection(lock, &crate::system::host().system)
}

/// `system`'s view of a lock (design §5.4, "Using it"): its projection. A system outside the
/// lock's set is an error only when the lock has system-specific entries; otherwise it gets the
/// entries common to every system, with a note (returned). scpm never guesses a nearby system.
pub fn projection(lock: &Lockfile, system: &str) -> Result<(Lockfile, Option<String>), InstallError> {
    if lock.systems.iter().any(|s| s == system) {
        return Ok((lock.project(system), None));
    }
    let set = lock.systems.join(", ");
    let host = if system == crate::system::host().system { "the host system " } else { "" };
    if lock.has_system_specific_entries() {
        return Err(InstallError::Systems(format!(
            "{host}{system} is not one of the lock's systems ({set}), and the lock has entries that differ by system, \
             so its classpaths for {system} are unknown\n  help: add {system} to `lock.systems` in \
             scpm-workspace.yaml and run `scpm install`"
        )));
    }
    let note = format!(
        "{host}{system} is not one of the lock's systems ({set}); no entry differs by system, so {system} uses \
         the classpaths common to all of them"
    );
    Ok((lock.project(system), Some(note)))
}

/// CI mode (implied `--frozen`) is `CI=true` (any case: AppVeyor sets `True`).
pub fn in_ci() -> bool {
    std::env::var("CI").is_ok_and(|v| v.eq_ignore_ascii_case("true"))
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
    let mut warnings = machine.warnings.clone();
    let mut notes: Vec<String> = Vec::new();
    let mut previous = read_previous(&ws.root, opts.frozen, &mut warnings)?;
    let ctx = lock_context(ws, &previous, opts.reset_systems);
    migrate_catalog_keys(ws, &mut previous.catalogs, &mut warnings);

    let resolver = Resolver::from_env()?;
    let table = &crate::natives::BUILT_IN;
    let hashes: BTreeMap<String, String> =
        ws.variants().map(|v| (v.id.clone(), inputs_hash(ws, graph, v, &repos, &ctx))).collect();
    let mut stale: Vec<String> = hashes
        .iter()
        .filter(|(id, hash)| previous.variants.get(*id).is_none_or(|l| &l.inputs_hash != *hash))
        .map(|(id, _)| id.clone())
        .collect();
    // The sidecar re-locks what `cs` wrote (no `fullContext`), and, with a newer mapping table
    // than the lock's, every variant holding a module whose mapping changed since (design §5.4).
    let mut boot_unmarked = false;
    if resolver.full_context() && !opts.frozen {
        for (id, l) in &previous.variants {
            let mapping_changed = previous.native_mappings.is_some_and(|n| {
                n < table.version
                    && l.classpaths()
                        .iter()
                        .any(|c| c.artifact_keys().any(|k| crate::natives::touched(table, n, table.version, k)))
            });
            if hashes.contains_key(id) && !stale.contains(id) && (!l.full_context || mapping_changed) {
                stale.push(id.clone());
            }
        }
        stale.sort();
        boot_unmarked = !previous.boot.is_empty() && !previous.boot_full_context;
    }
    if opts.frozen
        && let Some(n) = previous.native_mappings.filter(|n| *n != table.version)
    {
        notes.push(format!("the lock uses native mapping table {n}, this scpm has {}", table.version));
    }
    // Compilers for every Scala version in use (Java-only variants drive Zinc with one too),
    // plus the JVM helpers.
    let scala_versions: BTreeSet<String> = ws.variants().map(|v| compiler_scala(ws, v)).collect();
    // `SCPM_TEST_SKIP_BOOT` is a hidden switch for tests whose fake repositories can't serve
    // these; commands that need a boot entry then say it is missing.
    let boot: Vec<BootRoot> = if std::env::var_os("SCPM_TEST_SKIP_BOOT").is_some() {
        Vec::new()
    } else {
        scala_versions.iter().flat_map(|v| boot_roots(v)).chain(helper_roots()).chain(semanticdb_roots(ws)?).collect()
    };
    let boot_inputs = (!boot.is_empty()).then(|| boot_inputs_hash(&repos, &ctx));
    let boot_inputs_changed = previous.boot_inputs != boot_inputs;
    // An unmarked boot section is re-resolved whole, whatever `bootInputs` says.
    let missing_boot: Vec<&BootRoot> =
        boot.iter().filter(|b| boot_inputs_changed || boot_unmarked || !previous.boot.contains_key(&b.name)).collect();
    let removed: Vec<String> = previous.variants.keys().filter(|id| !hashes.contains_key(*id)).cloned().collect();

    if opts.frozen && (!stale.is_empty() || !missing_boot.is_empty() || !removed.is_empty()) {
        let mut what = stale.clone();
        what.extend(missing_boot.iter().map(|b| match b.name.split_once('@') {
            Some(("worker", _)) => "the compile worker's dependencies (they change with scpm versions)".to_string(),
            Some(("test-runner", _)) => "the test runner's dependencies (they change with scpm versions)".to_string(),
            _ => b.name.clone(),
        }));
        what.extend(removed.iter().map(|id| format!("{id} (removed)")));
        return Err(InstallError::Stale(what));
    }
    let relocks = !stale.is_empty() || !missing_boot.is_empty();
    // `cs` resolves for the machine it runs on and takes only the JDK, so it can lock one
    // system only (design §5.4). A lock that already has several systems still restores.
    if relocks && resolver == Resolver::Cs && ctx.systems.len() > 1 {
        return Err(InstallError::Systems(format!(
            "the lock's system set has {} systems ({}), and `cs` can resolve for one system only; resolving for \
             several systems needs the resolver sidecar (design §5.2.1)\n  help: set {}=sidecar, or set \
             `lock.systems` to the one system to resolve for",
            ctx.systems.len(),
            ctx.systems.join(", "),
            crate::resolver::RESOLVER_ENV
        )));
    }
    // Never downgrade the mapping table: an older scpm can't re-lock what a newer one mapped.
    if relocks
        && resolver.full_context()
        && let Some(n) = previous.native_mappings.filter(|n| *n > table.version)
    {
        return Err(InstallError::Systems(format!(
            "{} uses native mapping table {n}, and this scpm has {}; re-locking with it would drop newer \
             mappings\n  help: upgrade scpm",
            crate::lockfile::LOCK_FILE,
            table.version
        )));
    }
    if opts.frozen {
        notes.extend(unmarked_note(&previous, resolver));
    }

    let started = Instant::now();
    let mut lock = Lockfile {
        lockfile_version: LOCKFILE_VERSION,
        systems: ctx.systems.clone(),
        default_systems: ctx.default_systems,
        // `cs` applies no native mappings: it keeps the header as it is (design §5.4).
        native_mappings: previous.native_mappings,
        jdk: ctx.jdk.clone(),
        boot_inputs,
        // One flag for the whole section: a boot entry resolved without the full context
        // unmarks it.
        boot_full_context: !boot.is_empty()
            && if missing_boot.is_empty() { previous.boot_full_context } else { resolver.full_context() },
        boot: previous.boot.clone(),
        catalogs: BTreeMap::new(),
        variants: BTreeMap::new(),
        artifacts: previous.artifacts.clone(),
    };
    lock.boot.retain(|name, _| boot.iter().any(|b| &b.name == name));

    let scratch = tempfile_dir()?;
    let sidecar = LazySidecar::new(resolver, machine, &repos);
    let selector = Selector {
        repos: &repos,
        proxy: &machine.proxy,
        cache: &cache,
        previous: &previous.catalogs,
        catalogs: Mutex::new(BTreeMap::new()),
        listed: Mutex::new(HashMap::new()),
        sidecar: &sidecar,
    };
    // Ranges are never re-selected implicitly (design §5.5): seed every dependency whose spec is
    // unchanged with the version the previous lock selected, stale variants included. Only new
    // or edited specs are selected. Catalog ranges go through the shared catalog selections.
    let mut chosen: HashMap<(String, String), String> = HashMap::new();
    for v in ws.variants() {
        let Some(prev) = previous.variants.get(&v.id) else { continue };
        for dep in versioned(v) {
            // Only plain ranges are seeded: exact versions need no selection (and an edited
            // catalog entry keeps the spec text `catalog`), and catalog ranges are shared
            // through the lock's catalog selections.
            if !matches!(dep.version, VersionSpec::Range { .. }) || catalog_key(v, dep).is_some() {
                continue;
            }
            let key = specifier_key(v, dep);
            if let Some(Specifier { spec, version: Some(version) }) = prev.specifiers.get(&key)
                && spec == &dep.spec
            {
                chosen.insert((v.id.clone(), key), version.clone());
            }
        }
    }
    let mut version_of = |owner: &Variant, dep: &Dependency| -> Result<String, InstallError> {
        let key = (owner.id.clone(), specifier_key(owner, dep));
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
        for dep in versioned(v) {
            let version = version_of(v, dep)?;
            specs.entry(specifier_key(v, dep)).or_insert(Specifier { spec: dep.spec.clone(), version: Some(version) });
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
                req.dependencies.extend(b.roots.iter().cloned())
            } else {
                req.intransitive.extend(b.roots.iter().cloned())
            }
            labels.push((req.clone(), format!("boot entry {}", b.name)));
            (b.name.clone(), req)
        })
        .collect();

    let all: Vec<&ResolveRequest> =
        requests.values().chain(plugin_requests.values()).chain(boot_requests.values()).collect();
    let results = if all.is_empty() {
        Ok(HashMap::new())
    } else if let Some(s) = sidecar.get()? {
        resolve_with_sidecar(s, &all, &labels, &ctx, &repos, &cache)
    } else {
        resolve_concurrently(&all, &labels, &ctx, &repos, &machine.proxy, &cache, scratch.path())
    }
    .map_err(|e| with_notice(e, &repos, true))?;

    record_artifacts(&mut lock, all.iter().flat_map(|req| results[*req].artifacts()), &repos)?;
    // Systems a mapped module got no native jar on (design §5.4): unmapped ones are a note, a
    // built-in mapping's missing jar a warning, a user-written mapping's an error (before the
    // lock is written).
    if resolver.full_context() {
        let mut errors: Vec<String> = Vec::new();
        for req in requests.values() {
            let found = native_notes(req, &results[req], &ctx.systems);
            for (into, items) in
                [(&mut notes, found.notes), (&mut warnings, found.warnings), (&mut errors, found.errors)]
            {
                for item in items {
                    if !into.contains(&item) {
                        into.push(item);
                    }
                }
            }
        }
        if !errors.is_empty() {
            return Err(InstallError::Resolve(errors.join("\n")));
        }
    }
    for (name, req) in &boot_requests {
        lock.boot.insert(name.clone(), results[req].classpath(Vec::new()));
    }

    for v in ws.variants() {
        let entry = if let Some(specs) = specifiers.remove(&v.id) {
            let classpath = |kind: ClasspathKind| -> Classpath {
                results[&requests[&(v.id.clone(), kind)]].classpath(upstream_variants(graph, &v.id, kind))
            };
            VariantLock {
                full_context: resolver.full_context(),
                specifiers: specs,
                inputs_hash: hashes[&v.id].clone(),
                compile_classpath: classpath(ClasspathKind::Compile),
                runtime_classpath: classpath(ClasspathKind::Runtime),
                test_compile_classpath: classpath(ClasspathKind::TestCompile),
                test_runtime_classpath: classpath(ClasspathKind::TestRuntime),
                compiler_plugins: plugin_requests
                    .get(&v.id)
                    .map(|r| results[r].classpath(Vec::new()))
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
        for dep in versioned(v) {
            if let (Some(key), VersionSpec::Range { .. }) = (catalog_key(v, dep), &dep.version) {
                used_catalogs.insert(key);
            }
        }
    }
    lock.catalogs = previous.catalogs.clone();
    lock.catalogs.extend(selector.catalogs.into_inner().expect("catalogs"));
    lock.prune(&used_catalogs);
    let resolve_ms = started.elapsed().as_millis();

    // Restores download only the host system's files (and those of systems asked for), and
    // only once the compile worker's JDK is known to be for the host system (design §5.4).
    // Checked before the lock is written, so a refused command leaves the lock as it was.
    crate::system::check_worker().map_err(InstallError::Systems)?;
    let host = crate::system::host();
    notes.extend(host.note.clone());
    let keys = restore_keys(&lock, &host.system, &opts.restore, &mut notes)?;
    // Every install, frozen included: a mapped module on the host's classpaths without its
    // native jar (the repository had none when it was locked) is said each time, not only on
    // the run that re-locked it (design §5.4, "Mapped modules contribute a jar").
    for w in host_native_warnings(ws, graph, &lock, &host.system, table) {
        if !warnings.contains(&w) {
            warnings.push(w);
        }
    }

    // Whenever a sidecar install writes the lock at all, every variant was re-locked against
    // this scpm's mapping table (affected ones in this run), so the header becomes its version;
    // a newer header (only possible when nothing re-locked) stays (design §5.4). A re-lock
    // always raises it, even one whose result equals the lock: otherwise it would repeat.
    if resolver.full_context()
        && !opts.frozen
        && previous.native_mappings.is_none_or(|n| n <= table.version)
        && (relocks || std::fs::read_to_string(ws.root.join(crate::lockfile::LOCK_FILE)).ok() != Some(lock.to_yaml()))
    {
        lock.native_mappings = Some(table.version);
    }
    let lockfile_written = if opts.frozen { false } else { lock.write(&ws.root)? };
    sidecar.finish();
    // `cs` applies only the JDK part of the context (design §5.4, "Phase 1 limit").
    let resolved_on =
        (resolver == Resolver::Cs && (!stale.is_empty() || !missing_boot.is_empty())).then(HostPlatform::current);
    if let Some(host) = &resolved_on
        && let [declared] = ctx.systems.as_slice()
        && crate::system::from_os_arch(&host.os, &host.arch).as_deref() != Some(declared.as_str())
    {
        warnings.push(format!(
            "resolved on {}-{}, but the lock declares {declared}; OS/arch-specific POM profiles follow the resolving machine until the resolver sidecar lands (design §5.4)",
            host.os, host.arch
        ));
    }

    // Re-locking contacted the repositories; with `cs`, which inherits more than scpm reads.
    let resolved_any = !stale.is_empty() || !missing_boot.is_empty();
    let ran_cs = resolved_any && resolver == Resolver::Cs;
    let fetch_started = Instant::now();
    let items = fetch_items_of(&lock, &keys, &repos, &CoursierCredentials::load(), &cache, &mut warnings)?;
    let fetch = fetch::fetch_all(items, FetchOptions { verify: opts.verify, proxy: machine.proxy.clone() })
        .map_err(|f| with_notice(InstallError::Fetch(f), &repos, ran_cs))?;
    let contacted = resolved_any || fetch.downloaded > 0;
    let notices: Vec<String> =
        if contacted { crate::foreign::take_notice(&repos, ran_cs).into_iter().collect() } else { Vec::new() };
    warnings.extend(fetch.warnings.iter().cloned());
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
        notes,
        notices,
        resolve_ms,
        fetch_ms: fetch_started.elapsed().as_millis(),
    })
}

/// The artifacts a restore downloads (design §5.4, "Restores"): the host system's projection of
/// every variant and boot entry, plus those of the systems `restore` asks for (or, for
/// [`RestoreSystems::Only`], that system's alone). A host outside the lock's set is checked
/// here, before anything is downloaded; its note goes to `notes`.
fn restore_keys(
    lock: &Lockfile,
    host: &str,
    restore: &RestoreSystems,
    notes: &mut Vec<String>,
) -> Result<BTreeSet<String>, InstallError> {
    let mut keys: BTreeSet<String> = BTreeSet::new();
    if !matches!(restore, RestoreSystems::Only(_)) {
        let (view, outside) = projection(lock, host)?;
        notes.extend(outside);
        keys.extend(view.referenced_artifacts().into_iter().map(str::to_string));
    }
    let others: Vec<&String> = match restore {
        RestoreSystems::Host => Vec::new(),
        RestoreSystems::Also(others) => others.iter().collect(),
        RestoreSystems::All => lock.systems.iter().collect(),
        RestoreSystems::Only(system) => vec![system],
    };
    for system in others {
        let (view, _) = projection(lock, system)?;
        keys.extend(view.referenced_artifacts().into_iter().map(str::to_string));
    }
    Ok(keys)
}

/// Adds the notice about ignored settings of other tools to an error from contacting the
/// repositories: one of them may be why it failed (design §5.9). `ran_cs`: whether the command ran `cs`.
pub(crate) fn with_notice(e: InstallError, repos: &[Repo], ran_cs: bool) -> InstallError {
    // A configuration error was found before any repository was contacted.
    if matches!(e, InstallError::Coursier(coursier::CoursierError::CredentialClash(_))) {
        return e;
    }
    match crate::foreign::take_notice(repos, ran_cs) {
        Some(notice) => InstallError::WithNotice { error: Box::new(e), notice },
        None => e,
    }
}

/// Which resolver re-locks (design §5.2.1, "Migration").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolver {
    /// The coursier CLI: one system, and only the JDK part of the context.
    Cs,
    /// The resolver sidecar (`SCPM_RESOLVER=sidecar`): every system, OS, arch and JDK.
    Sidecar,
}

impl Resolver {
    /// `SCPM_RESOLVER`: `cs` (the default in M1) or `sidecar`.
    pub fn from_env() -> Result<Resolver, InstallError> {
        match std::env::var(crate::resolver::RESOLVER_ENV).as_deref() {
            Err(_) | Ok("") | Ok("cs") => Ok(Resolver::Cs),
            Ok("sidecar") => Ok(Resolver::Sidecar),
            Ok(other) => Err(InstallError::Unsupported(format!(
                "{}={other}: expected `cs` or `sidecar`",
                crate::resolver::RESOLVER_ENV
            ))),
        }
    }

    /// Whether what it resolves carries `fullContext` (the OS, arch and JDK of every system).
    pub fn full_context(self) -> bool {
        match self {
            Resolver::Cs => false,
            Resolver::Sidecar => true,
        }
    }
}

/// What a re-lock found about mapped modules' native jars (design §5.4).
#[derive(Debug, Default, PartialEq, Eq)]
struct NativeFindings {
    /// No mapping row covers a system at the resolved version (a library that doesn't support
    /// it, or a version older than its native jars): expected, so a note.
    notes: Vec<String>,
    /// A built-in row maps the system, but the repository answered 404 or 403 for the jar (the
    /// sidecar leaves it out, as coursier skips an optional artifact on either). The table
    /// follows Maven Central, so this is a proxy's negative cache, a mirror without the
    /// classifiers, or the like: a warning.
    warnings: Vec<String>,
    /// The same for a mapping the user wrote (`natives:`, `lock.nativeClassifiers`), most
    /// likely a typo in a classifier: an error.
    errors: Vec<String>,
}

/// [`NativeFindings`] for one request's resolution. A module whose matching rows map nothing
/// (`natives: {}`, which turns mapping off) is never mentioned.
fn native_notes(req: &ResolveRequest, res: &Resolution, systems: &[String]) -> NativeFindings {
    let mut found = NativeFindings::default();
    if req.natives.is_empty() {
        return found;
    }
    // Keys on each system.
    let on: BTreeMap<&str, BTreeSet<&str>> = systems
        .iter()
        .map(|s| {
            let keys: BTreeSet<&str> = match res {
                Resolution::Merged(list) => list
                    .iter()
                    .filter(|(_, sys)| sys.as_ref().is_none_or(|l| l.contains(s)))
                    .map(|(a, _)| a.key.as_str())
                    .collect(),
                Resolution::BySystem(m) => m.get(s).into_iter().flatten().map(|a| a.key.as_str()).collect(),
            };
            (s.as_str(), keys)
        })
        .collect();
    let mains: BTreeSet<&str> =
        on.values().flatten().copied().filter(|k| k.split(':').count() == 3 && !k.contains('@')).collect();
    for key in mains {
        let mut parts = key.split(':');
        let (Some(g), Some(a), Some(v)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let module = format!("{g}:{a}");
        let rows: Vec<&crate::natives::NativeSpec> = req.natives.iter().filter(|n| n.module == module).collect();
        if rows.iter().all(|r| r.natives.is_empty()) {
            continue;
        }
        let mut unmapped = Vec::new();
        let (mut absent_built_in, mut absent_written) = (Vec::new(), Vec::new());
        for s in systems.iter().filter(|s| on[s.as_str()].contains(key)) {
            let mapped = rows
                .iter()
                .filter(|r| r.versions.as_deref().is_none_or(|i| interval_contains(i, v)))
                .find_map(|r| r.natives.get(s).map(|c| (c, r.written)));
            match mapped {
                None => unmapped.push(s.as_str()),
                Some((c, written)) if !on[s.as_str()].contains(format!("{key}:{c}").as_str()) => {
                    if written { &mut absent_written } else { &mut absent_built_in }.push(format!("{s} (`{c}`)"))
                }
                Some(_) => {}
            }
        }
        if !unmapped.is_empty() {
            found.notes.push(format!(
                "{key} gets no native jar on {}: no native mapping covers {} at this version",
                unmapped.join(", "),
                if unmapped.len() == 1 { "that system" } else { "those systems" }
            ));
        }
        if !absent_built_in.is_empty() {
            found.warnings.push(format!(
                "{key} gets no native jar on {}: scpm's mapping names that classifier, but the repository answered \
                 404 or 403 for it (a proxy's negative cache, or a mirror without the classifier jars?)",
                absent_built_in.join(", ")
            ));
        }
        if !absent_written.is_empty() {
            found.errors.push(format!(
                "{key}: the repository has no native jar for {} (it answered 404 or 403)\n  help: check the \
                 classifier in `natives:` or `lock.nativeClassifiers`; leave a system out for no native jar",
                absent_written.join(", ")
            ));
        }
    }
    found
}

/// Whether a Maven interval (`[a,b)`, `[a,)`, …) contains `version`.
fn interval_contains(interval: &str, version: &str) -> bool {
    use crate::semver::MavenVersion;
    let t = interval.trim();
    let (Some(open), Some(close)) = (t.chars().next(), t.chars().last()) else { return false };
    let Some((lo, hi)) = t.get(1..t.len() - 1).and_then(|inner| inner.split_once(',')) else { return false };
    let v = MavenVersion::parse(version);
    let lo_ok = lo.trim().is_empty() || {
        let b = MavenVersion::parse(lo.trim());
        if open == '[' { v >= b } else { v > b }
    };
    let hi_ok = hi.trim().is_empty() || {
        let b = MavenVersion::parse(hi.trim());
        if close == ']' { v <= b } else { v < b }
    };
    lo_ok && hi_ok
}

/// Resolves every request in one sidecar call (it runs them concurrently, each for every
/// system of the lock). `labels` names what each request is for, in errors.
fn resolve_with_sidecar<'a>(
    sidecar: &crate::resolver::Sidecar,
    requests: &[&'a ResolveRequest],
    labels: &[(ResolveRequest, String)],
    ctx: &LockContext,
    repos: &[Repo],
    cache: &Path,
) -> Result<HashMap<&'a ResolveRequest, Resolution>, InstallError> {
    let mut distinct: Vec<&ResolveRequest> = Vec::new();
    for r in requests {
        if !distinct.contains(r) {
            distinct.push(r);
        }
    }
    let results = sidecar.resolve(&distinct, &ctx.systems, &ctx.jdk, repos, cache)?;
    let mut out = HashMap::new();
    for (req, result) in distinct.into_iter().zip(results) {
        let resolution = result.map_err(|msg| {
            let what: Vec<&str> = labels.iter().filter(|(r, _)| r == req).map(|(_, l)| l.as_str()).collect();
            let what = if what.is_empty() { "a request".to_string() } else { what.join(", ") };
            InstallError::Resolve(format!("resolving {what} failed:\n{msg}"))
        })?;
        out.insert(req, resolution);
    }
    Ok(out)
}

/// The note a frozen run gives about variants (and a boot section) locked without the full
/// resolution context, which it uses as they are (design §5.4). Only for a lock that already
/// mixes them with sidecar-written ones, or when the sidecar resolves: under `cs` every variant
/// is unmarked, and a note on every frozen run would say nothing new.
fn unmarked_note(lock: &Lockfile, resolver: Resolver) -> Option<String> {
    let unmarked: Vec<&str> =
        lock.variants.iter().filter(|(_, v)| !v.full_context).map(|(id, _)| id.as_str()).collect();
    let boot = !lock.boot.is_empty() && !lock.boot_full_context;
    let sidecar_wrote =
        lock.native_mappings.is_some() || lock.boot_full_context || lock.variants.values().any(|v| v.full_context);
    if (unmarked.is_empty() && !boot) || !(sidecar_wrote || resolver.full_context()) {
        return None;
    }
    let mut what = Vec::new();
    if !unmarked.is_empty() {
        what.push(unmarked.join(", "));
    }
    if boot {
        what.push("the boot section".to_string());
    }
    Some(format!(
        "{} {} locked without the full resolution context (by `cs`, whose OS and arch profiles follow the machine it \
         ran on); used as locked, and a non-frozen install with the resolver sidecar re-locks {}",
        what.join(" and "),
        if unmarked.len() + usize::from(boot) == 1 { "was" } else { "were" },
        if unmarked.len() + usize::from(boot) == 1 { "it" } else { "them" },
    ))
}

/// One request's result for every system of the lock (design §5.2.1): a single merged list
/// whose entries name the systems they are on when not on every one, or, when the per-system
/// orders can't be merged, a list per system.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Merged(Vec<(ResolvedArtifact, Option<Vec<String>>)>),
    BySystem(BTreeMap<String, Vec<ResolvedArtifact>>),
}

impl Resolution {
    /// Every resolved file, on any system.
    fn artifacts(&self) -> Vec<&ResolvedArtifact> {
        match self {
            Resolution::Merged(list) => list.iter().map(|(a, _)| a).collect(),
            Resolution::BySystem(m) => m.values().flatten().collect(),
        }
    }

    /// The locked classpath: `modules` (upstream workspace variants, on every system) first.
    fn classpath(&self, modules: Vec<String>) -> Classpath {
        let module_entries = || modules.iter().map(|m| ClasspathEntry::Module { module: m.clone() });
        match self {
            Resolution::Merged(list) => {
                Classpath::List(
                    module_entries()
                        .map(LockEntry::plain)
                        .chain(list.iter().map(|(a, systems)| {
                            LockEntry::new(ClasspathEntry::Artifact(a.key.clone()), systems.clone())
                        }))
                        .collect(),
                )
            }
            Resolution::BySystem(m) => Classpath::BySystem(
                m.iter()
                    .map(|(system, list)| {
                        let entries =
                            module_entries().chain(list.iter().map(|a| ClasspathEntry::Artifact(a.key.clone())));
                        (system.clone(), entries.collect())
                    })
                    .collect(),
            ),
        }
    }
}

fn bom_string(v: &Variant, bom: &Dependency, version: &str) -> String {
    format!("{}:{}:{version}", bom.coord.org, artifact_name(v, &bom.coord))
}

/// Everything in a variant whose version is selected and locked: dependencies, compiler plugins
/// and BOMs.
fn versioned(v: &Variant) -> impl Iterator<Item = &Dependency> {
    v.dependencies.iter().chain(&v.compiler_plugins).chain(&v.boms)
}

/// The key of `dep`'s entry in its variant's lock `specifiers`: the coordinate, or
/// `bom:<coordinate>` for a BOM, so a BOM never shares an entry with a dependency.
pub(crate) fn specifier_key(owner: &Variant, dep: &Dependency) -> String {
    if owner.boms.iter().any(|b| std::ptr::eq(b, dep)) { format!("bom:{}", dep.coord) } else { dep.coord.to_string() }
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
    ctx: &LockContext,
    repos: &[Repo],
    proxy: &crate::machine::ProxyChoice,
    cache: &Path,
    scratch: &Path,
) -> Result<HashMap<&'a ResolveRequest, Resolution>, InstallError> {
    // Nothing to resolve: a lock with several systems that is up to date only restores.
    if requests.is_empty() {
        return Ok(HashMap::new());
    }
    // Checked by the caller: `cs` resolves one system.
    debug_assert!(ctx.systems.len() <= 1, "cs resolves a single system");
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
        out.insert(req, Resolution::Merged(resolved.into_iter().map(|a| (a, None)).collect()));
    }
    Ok(out)
}

/// Adds artifacts from this run's resolutions to the lock. An artifact's identity is its key and
/// sha256; its `repo` is a hint for restores (design §5.4), derived from where coursier
/// downloaded the file among the repositories allowed for its group: an existing entry keeps
/// its id if that is still one of them (repositories sharing a mirror can't be told apart by
/// URL), else the first of them in declared order. A file that came from a repository its group
/// may not come from (either way `groups` route) is refused, and nothing is locked. An existing entry
/// keeps its sha256 and size (artifacts are immutable) only if the resolved file has them: the
/// lock doesn't record repository URLs, so a repository whose URL changed under the same id
/// must be checked like one with a new id.
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
        let by_url = repos_for_url(repos, &a.url);
        let source = by_url.first().copied().ok_or_else(|| {
            InstallError::Resolve(format!(
                "{} was downloaded from {}, which matches no configured repository or mirror",
                a.key, a.url
            ))
        })?;
        let allowed = coursier::allowed_repos(repos, coursier::group_of(&a.key));
        let candidates: Vec<&Repo> = by_url.iter().copied().filter(|r| allowed.iter().any(|x| x.id == r.id)).collect();
        let group = coursier::group_of(&a.key);
        // A group restricted by `groups` that `cs` found elsewhere (its own repository lacks it):
        // locking those bytes would let a public repository supply an internal name.
        // When the bytes came through a user config mirror, that mirror (one machine's setting) is
        // what to fix, not the repository's URL in the workspace.
        let via = source.mirror.as_ref().map(|m| format!(" through the user config mirror {}", m.url));
        if candidates.is_empty() && coursier::group_is_restricted(repos, group) {
            let ids: Vec<String> = allowed.iter().map(|r| format!("`{}`", r.id)).collect();
            let help = match &source.mirror {
                Some(m) => format!(
                    "the copy came from the user config mirror {m_url}, which `{from}` is reached through on this \
                     machine; if that URL serves more than `{from}` (a combined group), point the mirror at one that \
                     serves only `{from}`, or don't mirror `{from}` with it; otherwise publish it to {ids}",
                    m_url = m.url,
                    from = source.id,
                    ids = ids.join(", "),
                ),
                None => format!(
                    "publish it to {ids}, or check the version; if `{from}` is meant to serve it, add the group to that \
                     repository's `groups`",
                    ids = ids.join(", "),
                    from = source.id,
                ),
            };
            return Err(InstallError::Resolve(format!(
                "{key} was found in repository `{from}`{via} ({url}), but group `{group}` may only come from {ids}, \
                 which didn't have it; it is not locked, since a copy of an internal name in another repository may be \
                 dependency confusion\n  help: {help}",
                key = a.key,
                from = source.id,
                via = via.as_deref().unwrap_or_default(),
                url = a.url,
                ids = ids.join(", "),
            )));
        }
        if allowed.is_empty() {
            return Err(InstallError::Resolve(format!("{}: {}", a.key, coursier::no_repository_for(group))));
        }
        // `groups` route both ways: a group no rule covers comes only from repositories without
        // `groups`, so bytes a grouped repository served for it (a combined URL that also proxies
        // Central) are not locked under another repository's name.
        if candidates.is_empty() {
            let ids: Vec<String> = allowed.iter().map(|r| format!("`{}`", r.id)).collect();
            let help = match &source.mirror {
                Some(m) => format!(
                    "`{from}` is reached through the user config mirror {m_url} on this machine, and that URL also \
                     serves public artifacts: point the mirror at an internal-only URL (such as \
                     …/repository/acme-releases/), or don't mirror `{from}` with it; to reach Central through the \
                     company server, mirror `central` instead",
                    m_url = m.url,
                    from = source.id,
                ),
                None => format!(
                    "point `{from}` at the internal-only repository URL (such as …/repository/acme-releases/), not a \
                     combined one that also proxies Central; to reach Central through the company server, add a mirror \
                     of `central` (in the user config)",
                    from = source.id,
                ),
            };
            return Err(InstallError::Resolve(format!(
                "{key} was served by repository `{from}`{via} ({url}), which has `groups`, but group `{group}` is \
                 covered by no `groups` rule, so it may only come from the repositories without `groups` ({ids}); it is \
                 not locked\n  help: {help}",
                key = a.key,
                from = source.id,
                via = via.as_deref().unwrap_or_default(),
                url = a.url,
                ids = ids.join(", "),
            )));
        }
        let kept = lock.artifacts.get(&a.key).and_then(|e| candidates.iter().find(|r| r.id == e.repo));
        let repo = kept.or(candidates.first()).copied().expect("candidates is not empty");
        let path = coursier::maven_path(&a.key)
            .ok_or_else(|| InstallError::Resolve(format!("coursier resolved an invalid artifact key `{}`", a.key)))?;
        let expected = format!("{}{path}", source.fetch_url());
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

/// The configured repositories serving `url`, matched against where they are downloaded from
/// (their mirror, if any), so a mirror maps back to repository ids. The longest prefix wins;
/// repositories sharing a mirror all match, in resolution order.
fn repos_for_url<'a>(repos: &'a [Repo], url: &str) -> Vec<&'a Repo> {
    let matching = repos.iter().filter(|r| url.starts_with(r.fetch_url()));
    let longest = matching.clone().map(|r| r.fetch_url().len()).max().unwrap_or(0);
    matching.filter(|r| r.fetch_url().len() == longest).collect()
}

#[cfg(test)]
fn repo_for_url<'a>(repos: &'a [Repo], url: &str) -> Option<&'a Repo> {
    repos_for_url(repos, url).first().copied()
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

/// `shared` is what `cs` would send too ([`coursier::shared_credentials`]): the repository's
/// own credentials, or another repository's on its origin.
fn repo_access(repo: &Repo, shared: Option<Auth>, creds: &CoursierCredentials) -> RepoAccess {
    if let Some(auth) = shared {
        let cache_user = CacheUser::Known(auth.user().to_string());
        return RepoAccess { auth: Ok(Some(auth)), challenge: Vec::new(), cache_user };
    }
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

/// Every artifact the lock references, as download instructions. For a projection
/// ([`Lockfile::project`]), that is one system's files.
pub fn fetch_items(
    lock: &Lockfile,
    repos: &[Repo],
    creds: &CoursierCredentials,
    cache: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<FetchItem>, InstallError> {
    let keys: BTreeSet<String> = lock.referenced_artifacts().into_iter().map(str::to_string).collect();
    fetch_items_of(lock, &keys, repos, creds, cache, warnings)
}

/// [`fetch_items`] for the artifacts `keys` names.
fn fetch_items_of(
    lock: &Lockfile,
    keys: &BTreeSet<String>,
    repos: &[Repo],
    creds: &CoursierCredentials,
    cache: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<FetchItem>, InstallError> {
    // Coursier's own credentials go over plain HTTP only to repositories that opt in.
    let creds = creds.restricted(&coursier::insecure_credential_origins(repos));
    // Origins with different credentials only stop `cs`; here each repository on such an origin
    // keeps its own, and every other origin still shares its credentials.
    let found = coursier::credential_sources(repos, &|var| std::env::var(var).ok(), &mut Vec::new());
    for clash in &found.clashes {
        warnings.push(format!("{}; restoring sends each repository there its own", clash.with_help_inline()));
    }
    let access: HashMap<&str, RepoAccess> = repos
        .iter()
        .zip(found.sources)
        .map(|(r, s)| (r.id.as_str(), repo_access(r, s.map(|(_, auth)| auth), &creds)))
        .collect();
    let mut items = Vec::new();
    for key in keys.iter().map(String::as_str) {
        let art = lock.artifacts.get(key).ok_or_else(|| {
            InstallError::Resolve(format!(
                "{} references {key}, which has no artifacts entry",
                crate::lockfile::LOCK_FILE
            ))
        })?;
        let path = coursier::maven_path(key).ok_or_else(|| {
            InstallError::Resolve(format!("{} has an invalid artifact key `{key}`", crate::lockfile::LOCK_FILE))
        })?;
        // The recorded repository is a hint (design §5.4): asked first if the artifact's group may
        // come from it, then the others allowed for the group, in declared order; the sha256
        // decides. An id that is no longer configured just isn't asked.
        let group = coursier::group_of(key);
        let allowed = coursier::allowed_repos(repos, group);
        if allowed.is_empty() {
            return Err(InstallError::Resolve(format!("{key}: {}", coursier::no_repository_for(group))));
        }
        let order = allowed.iter().filter(|r| r.id == art.repo).chain(allowed.iter().filter(|r| r.id != art.repo));
        let mut sources: Vec<FetchItem> = Vec::new();
        for repo in order {
            // A mirror only changes where the bytes come from; repositories sharing one are one source.
            let url = format!("{}{path}", repo.fetch_url());
            if sources.iter().any(|s| s.url == url) {
                continue;
            }
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
            sources.push(FetchItem {
                key: key.to_string(),
                url,
                dest,
                sha256: art.sha256.clone(),
                size: art.size,
                auth: access.auth.clone(),
                challenge: access.challenge.clone(),
                verify_cached,
                fallbacks: Vec::new(),
            });
        }
        // A copy already cached from any of them is the one used, and the one classpaths name.
        // One from a repository other than the first is used only once its sha256 matches:
        // another repository's file of the same name may differ, and a mismatching copy must
        // not displace the first source (its repair would ask the wrong repository).
        let usable = |i: usize, s: &FetchItem| {
            std::fs::metadata(&s.dest).is_ok_and(|m| m.len() == art.size)
                && (i == 0 || fetch::sha256_file(&s.dest).is_ok_and(|h| h == art.sha256))
        };
        if let Some(i) = sources.iter().enumerate().position(|(i, s)| usable(i, s)) {
            let cached = sources.remove(i);
            sources.insert(0, cached);
        }
        let mut item = sources.remove(0);
        item.fallbacks = sources;
        items.push(item);
    }
    Ok(items)
}

/// Absolute classpath for a variant from a projected lock ([`ensure`]): cached jars, and
/// upstream modules' class directories (`.scpm/out/<id>/classes`).
pub fn classpath_files(
    ws: &Workspace,
    lock: &Lockfile,
    id: &str,
    kind: ClasspathKind,
) -> Result<Vec<PathBuf>, InstallError> {
    let (repos, _) = repositories(ws)?;
    let cache = coursier::cache_dir();
    let variant = lock.variants.get(id).ok_or_else(|| InstallError::Stale(vec![id.to_string()]))?;
    let by_key: HashMap<String, FetchItem> =
        fetch_items(lock, &repos, &CoursierCredentials::load(), &cache, &mut Vec::new())?
            .into_iter()
            .map(|i| (i.key.clone(), i))
            .collect();
    Ok(kind
        .entries(variant)
        .into_iter()
        .map(|e| match e {
            ClasspathEntry::Module { module } => ws.root.join(".scpm/out").join(module).join("classes"),
            ClasspathEntry::Artifact(key) => by_key[key].dest.clone(),
        })
        .collect())
}

#[cfg(test)]
mod tests {

    #[test]
    fn upstreams_by_distance_not_global_order() {
        use crate::graph::{Edge, Graph};
        use crate::workspace::Scope;
        let e = |from: &str, to: &str| Edge { from: from.into(), to: to.into(), scope: Scope::Compile };
        // R depends on A and B; B depends on C. A is nearer than C whatever the global order.
        let graph = Graph {
            order: vec!["A".into(), "C".into(), "B".into(), "R".into()],
            edges: vec![e("R", "A"), e("R", "B"), e("B", "C")],
        };
        assert_eq!(upstream_by_distance(&graph, "R", ClasspathKind::Compile), vec!["A", "B", "C"]);
        // The old rule (reversed global order) put C before A.
        let rev: Vec<String> = upstream_variants(&graph, "R", ClasspathKind::Compile).into_iter().rev().collect();
        assert_eq!(rev, vec!["B", "C", "A"]);
    }
    use super::*;

    fn boot_list(keys: &[&str]) -> Classpath {
        Classpath::plain(keys.iter().map(|k| ClasspathEntry::Artifact(k.to_string())))
    }

    fn header(systems: &[&str], default_systems: Option<u32>) -> Lockfile {
        Lockfile {
            lockfile_version: LOCKFILE_VERSION,
            systems: systems.iter().map(|s| s.to_string()).collect(),
            default_systems,
            jdk: "25".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_system_set_comes_from_config_then_the_header_then_the_default() {
        let (dir, ws, _) = workspace(&[("a", "{}\n")]);
        // A new lock: the default set.
        let ctx = lock_context(&ws, &Lockfile::default(), false);
        assert_eq!((ctx.systems, ctx.default_systems), (vec!["linux-x86_64".to_string()], Some(1)));
        // A header from a default set governs, even another version's (a newer binary's).
        let newer = header(&["macos-aarch64", "linux-x86_64"], Some(2));
        let ctx = lock_context(&ws, &newer, false);
        assert_eq!(ctx.systems, ["linux-x86_64", "macos-aarch64"], "sorted");
        assert_eq!(ctx.default_systems, Some(2));
        // `--reset-systems`, and a header written from a since-removed `lock.systems`: the default.
        assert_eq!(lock_context(&ws, &newer, true).systems, ["linux-x86_64"]);
        assert_eq!(lock_context(&ws, &header(&["macos-aarch64"], None), false).systems, ["linux-x86_64"]);
        // `lock.systems` wins, and records no default version.
        std::fs::write(
            dir.path().join("scpm-workspace.yaml"),
            "modules: ['modules/*']\ntoolchain:\n  scala: 3.9.0\nlock:\n  systems: [macos-aarch64, linux-aarch64, macos-aarch64]\n  jdk: 21\n",
        )
        .unwrap();
        let ws = crate::workspace::load(dir.path()).unwrap();
        let ctx = lock_context(&ws, &newer, false);
        assert_eq!(
            ctx,
            LockContext {
                systems: vec!["linux-aarch64".into(), "macos-aarch64".into()],
                default_systems: None,
                jdk: "21".into()
            }
        );
    }

    #[test]
    fn native_classifiers_must_be_plain_names() {
        for bad in ["''", "'../x'", "'a/b'", "'a\\b'"] {
            let (dir, _, _) = workspace(&[("a", "{}\n")]);
            std::fs::write(
                dir.path().join("modules/a/scpm.yaml"),
                format!("dependencies:\n  g:x: {{version: '1', natives: {{linux-x86_64: {bad}}}}}\n"),
            )
            .unwrap();
            let err = crate::workspace::load(dir.path()).unwrap_err().to_string();
            assert!(err.contains("natives: invalid classifier"), "{bad}: {err}");
            std::fs::write(dir.path().join("modules/a/scpm.yaml"), "{}\n").unwrap();
            std::fs::write(
                dir.path().join("scpm-workspace.yaml"),
                format!(
                    "modules: ['modules/*']\ntoolchain:\n  scala: 3.9.0\nlock:\n  nativeClassifiers:\n    - {{module: 'g:x', natives: {{linux-x86_64: {bad}}}}}\n"
                ),
            )
            .unwrap();
            let err = crate::workspace::load(dir.path()).unwrap_err().to_string();
            assert!(err.contains("lock.nativeClassifiers[0].natives") && err.contains("invalid classifier"), "{err}");
        }
    }

    /// Systems a mapped module gets no native jar on are noted, whether no row maps them at the
    /// version or the repository lacked the mapped jar.
    #[test]
    fn missing_native_jars_are_noted() {
        let art = |k: &str| ResolvedArtifact { key: k.into(), file: PathBuf::from(k), url: String::new() };
        let systems: Vec<String> = ["linux-aarch64", "linux-x86_64", "macos-aarch64"].map(String::from).to_vec();
        let epoll = "io.netty:netty-transport-native-epoll";
        let req =
            ResolveRequest { natives: crate::natives::specs(&crate::natives::BUILT_IN, &[]), ..Default::default() };
        // 4.1.30: no aarch64 jar exists, and macOS has no epoll at all.
        let old = Resolution::Merged(vec![
            (art(&format!("{epoll}:4.1.30.Final")), None),
            (art(&format!("{epoll}:4.1.30.Final:linux-x86_64")), Some(vec!["linux-x86_64".into()])),
        ]);
        let found = native_notes(&req, &old, &systems);
        assert_eq!(
            found.notes,
            [format!(
                "{epoll}:4.1.30.Final gets no native jar on linux-aarch64, macos-aarch64: no native mapping covers those systems at this version"
            )]
        );
        assert!(found.warnings.is_empty() && found.errors.is_empty());
        // 4.1.115 maps linux-aarch_64, which this repository didn't serve: a built-in row's warning.
        let new = Resolution::Merged(vec![
            (art(&format!("{epoll}:4.1.115.Final")), Some(vec!["linux-aarch64".into(), "linux-x86_64".into()])),
            (art(&format!("{epoll}:4.1.115.Final:linux-x86_64")), Some(vec!["linux-x86_64".into()])),
        ]);
        let found = native_notes(&req, &new, &systems[..2]);
        assert!(found.notes.is_empty() && found.errors.is_empty(), "{found:?}");
        assert_eq!(found.warnings.len(), 1);
        assert!(
            found.warnings[0]
                .starts_with(&format!("{epoll}:4.1.115.Final gets no native jar on linux-aarch64 (`linux-aarch_64`)"))
        );
        assert!(found.warnings[0].contains("404 or 403"), "{found:?}");
        // The same for a mapping the user wrote: an error.
        let mine = BTreeMap::from([("linux-aarch64".to_string(), "linux-aarch_64".to_string())]);
        let written = ResolveRequest {
            natives: crate::natives::specs(&crate::natives::BUILT_IN, &[(epoll.into(), mine)]),
            ..Default::default()
        };
        let found = native_notes(&written, &new, &systems[..2]);
        assert_eq!(found.errors.len(), 1, "{found:?}");
        assert!(found.errors[0].contains("natives:"), "{found:?}");
        // `natives: {}` turns mapping off: nothing to say.
        let off = ResolveRequest {
            natives: crate::natives::specs(&crate::natives::BUILT_IN, &[(epoll.into(), BTreeMap::new())]),
            ..Default::default()
        };
        assert_eq!(native_notes(&off, &old, &systems), NativeFindings::default());
        assert_eq!(native_notes(&ResolveRequest::default(), &new, &systems), NativeFindings::default());
    }

    #[test]
    fn unknown_systems_are_config_errors() {
        let (dir, _, _) = workspace(&[("a", "{}\n")]);
        std::fs::write(
            dir.path().join("scpm-workspace.yaml"),
            "modules: ['modules/*']\ntoolchain:\n  scala: 3.9.0\nlock:\n  systems: [linux-amd64]\n  jdk: temurin\n",
        )
        .unwrap();
        let err = crate::workspace::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("unknown system `linux-amd64`") && err.contains("linux-x86_64"), "{err}");
        assert!(err.contains("`temurin` is not a JDK major version"), "{err}");
    }

    #[test]
    fn a_system_outside_the_set_needs_a_lock_without_system_specific_entries() {
        let mut lock = header(&["linux-aarch64", "linux-x86_64"], None);
        lock.boot.insert("b".into(), boot_list(&["g:a:1"]));
        let (view, note) = projection(&lock, "linux-x86_64").unwrap();
        assert_eq!((view.boot, note), (lock.boot.clone(), None));
        let (view, note) = projection(&lock, "windows-x86_64").unwrap();
        assert_eq!(view.boot["b"], boot_list(&["g:a:1"]), "the common projection");
        assert!(
            note.unwrap().contains("windows-x86_64 is not one of the lock's systems (linux-aarch64, linux-x86_64)")
        );
        let Some(Classpath::List(l)) = lock.boot.get_mut("b") else { unreachable!() };
        l.push(LockEntry {
            entry: ClasspathEntry::Artifact("g:n:1:linux-x86_64".into()),
            systems: Some(vec!["linux-x86_64".into()]),
        });
        let err = projection(&lock, "windows-x86_64").unwrap_err().to_string();
        assert!(err.contains("add windows-x86_64 to `lock.systems`"), "{err}");
        // A system in the set gets exactly its own entries.
        assert_eq!(projection(&lock, "linux-aarch64").unwrap().0.boot["b"], boot_list(&["g:a:1"]));
        assert_eq!(projection(&lock, "linux-x86_64").unwrap().0.boot["b"], boot_list(&["g:a:1", "g:n:1:linux-x86_64"]));
    }

    #[test]
    fn restores_download_only_the_host_systems_files() {
        let mut lock = header(&["linux-x86_64", "macos-aarch64"], None);
        let scoped = |k: &str, s: &str| LockEntry {
            entry: ClasspathEntry::Artifact(k.into()),
            systems: Some(vec![s.to_string()]),
        };
        let plain = |k: &str| LockEntry::plain(ClasspathEntry::Artifact(k.into()));
        lock.variants.insert(
            "a@jvm-3".into(),
            VariantLock {
                runtime_classpath: Classpath::List(vec![
                    plain("g:a:1"),
                    scoped("g:n:1:linux-x86_64", "linux-x86_64"),
                    scoped("g:n:1:osx", "macos-aarch64"),
                ]),
                test_runtime_classpath: Classpath::BySystem(
                    [
                        ("linux-x86_64".to_string(), vec![ClasspathEntry::Artifact("g:t:1".into())]),
                        ("macos-aarch64".to_string(), vec![ClasspathEntry::Artifact("g:t:2".into())]),
                    ]
                    .into(),
                ),
                ..Default::default()
            },
        );
        lock.boot
            .insert("b".into(), Classpath::List(vec![plain("g:c:1"), scoped("g:bn:1:linux-x86_64", "linux-x86_64")]));
        let keys = |host: &str, restore: RestoreSystems| -> Vec<String> {
            restore_keys(&lock, host, &restore, &mut Vec::new()).unwrap().into_iter().collect()
        };
        assert_eq!(keys("macos-aarch64", RestoreSystems::Host), ["g:a:1", "g:c:1", "g:n:1:osx", "g:t:2"]);
        assert_eq!(
            keys("linux-x86_64", RestoreSystems::Host),
            ["g:a:1", "g:bn:1:linux-x86_64", "g:c:1", "g:n:1:linux-x86_64", "g:t:1"]
        );
        assert_eq!(keys("macos-aarch64", RestoreSystems::All).len(), 7);
        assert_eq!(keys("macos-aarch64", RestoreSystems::Also(vec!["linux-x86_64".into()])).len(), 7);
        // A host outside the set is refused before anything is downloaded.
        let err = restore_keys(&lock, "windows-x86_64", &RestoreSystems::Host, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("the lock has entries that differ by system"), "{err}");
        // Unless only another system is asked for (`classpath --system`, packaging for Linux).
        assert_eq!(
            keys("windows-x86_64", RestoreSystems::Only("linux-x86_64".into())),
            keys("linux-x86_64", RestoreSystems::Host)
        );
    }

    /// An up-to-date lock with several systems resolves nothing, so `cs` (one system only) is
    /// never asked and the install only restores, frozen or not.
    #[test]
    fn an_up_to_date_lock_with_several_systems_only_restores() {
        let (dir, _, _) = workspace(&[("a", "scala: none\n")]);
        std::fs::write(
            dir.path().join("scpm-workspace.yaml"),
            "modules: ['modules/*']\nlock:\n  systems: [linux-x86_64, macos-aarch64]\n",
        )
        .unwrap();
        let ws = crate::workspace::load(dir.path()).unwrap();
        let graph = Graph::build(&ws).unwrap();
        let (repos, _) = repositories(&ws).unwrap();
        let ctx = lock_context(&ws, &Lockfile::default(), false);
        assert_eq!(ctx.systems.len(), 2);
        let mut lock = header(&["linux-x86_64", "macos-aarch64"], None);
        lock.boot_inputs = Some(boot_inputs_hash(&repos, &ctx));
        let boot: Vec<BootRoot> = boot_roots(JAVA_ONLY_SCALA).into_iter().chain(helper_roots()).collect();
        for b in &boot {
            lock.boot.insert(b.name.clone(), Classpath::default());
        }
        let v = ws.variant("a@jvm").unwrap();
        lock.variants.insert(
            v.id.clone(),
            VariantLock { inputs_hash: inputs_hash(&ws, &graph, v, &repos, &ctx), ..Default::default() },
        );
        lock.write(dir.path()).unwrap();
        let before = std::fs::read_to_string(dir.path().join(crate::lockfile::LOCK_FILE)).unwrap();
        for frozen in [true, false] {
            let summary = install(&ws, &graph, InstallOptions { frozen, ..Default::default() }).unwrap();
            assert!(summary.relocked.is_empty() && summary.boot_relocked.is_empty(), "{summary:?}");
            assert_eq!(summary.fetch.downloaded, 0);
        }
        assert_eq!(std::fs::read_to_string(dir.path().join(crate::lockfile::LOCK_FILE)).unwrap(), before);
    }

    /// A refused restore leaves the lock as it was: the checks run before it is written.
    #[test]
    fn a_refused_restore_writes_no_lock() {
        let (dir, ws2, graph) = workspace(&[("a", "scala: none\n")]);
        let (repos, _) = repositories(&ws2).unwrap();
        let ctx = lock_context(&ws2, &Lockfile::default(), false);
        // An up-to-date lock whose header differs from what this install writes (no
        // `defaultSystems`), with a system-specific boot entry for another system.
        let mut lock = header(&["linux-x86_64"], None);
        lock.boot_inputs = Some(boot_inputs_hash(&repos, &ctx));
        for b in boot_roots(JAVA_ONLY_SCALA).into_iter().chain(helper_roots()) {
            lock.boot.insert(b.name, Classpath::default());
        }
        lock.boot.insert(
            "compiler-bridge@2.13.18+zinc-2.1.0".into(),
            Classpath::List(vec![LockEntry {
                entry: ClasspathEntry::Artifact("g:n:1".into()),
                systems: Some(vec!["linux-x86_64".into()]),
            }]),
        );
        lock.artifacts.insert("g:n:1".into(), ArtifactLock { repo: "central".into(), sha256: "0".into(), size: 1 });
        let v = ws2.variant("a@jvm").unwrap();
        lock.variants.insert(
            v.id.clone(),
            VariantLock { inputs_hash: inputs_hash(&ws2, &graph, v, &repos, &ctx), ..Default::default() },
        );
        lock.write(dir.path()).unwrap();
        let before = std::fs::read_to_string(dir.path().join(crate::lockfile::LOCK_FILE)).unwrap();
        let opts =
            InstallOptions { restore: RestoreSystems::Also(vec!["windows-x86_64".into()]), ..Default::default() };
        let err = install(&ws2, &graph, opts).unwrap_err().to_string();
        assert!(err.contains("is not one of the lock's systems"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(crate::lockfile::LOCK_FILE)).unwrap(),
            before,
            "not rewritten"
        );
    }

    #[test]
    fn frozen_runs_note_unmarked_variants_only_in_mixed_locks() {
        let mut lock = header(&["linux-x86_64"], Some(1));
        lock.variants.insert("a@jvm-3".into(), VariantLock::default());
        lock.boot.insert("b".into(), boot_list(&["g:a:1"]));
        assert_eq!(unmarked_note(&lock, Resolver::Cs), None, "a lock `cs` wrote alone");
        lock.variants.insert("b@jvm-3".into(), VariantLock { full_context: true, ..Default::default() });
        let note = unmarked_note(&lock, Resolver::Cs).unwrap();
        assert!(
            note.starts_with("a@jvm-3 and the boot section were locked without the full resolution context"),
            "{note}"
        );
        lock.boot_full_context = true;
        let note = unmarked_note(&lock, Resolver::Cs).unwrap();
        assert!(note.starts_with("a@jvm-3 was locked") && note.ends_with("re-locks it"), "{note}");
    }

    #[test]
    fn resolutions_become_locked_classpaths() {
        let art = |k: &str| ResolvedArtifact { key: k.into(), file: PathBuf::from(k), url: String::new() };
        let merged = Resolution::Merged(vec![
            (art("g:a:1"), None),
            (art("g:n:1:linux-x86_64"), Some(vec!["linux-x86_64".into()])),
        ]);
        let cp = merged.classpath(vec!["util@jvm-3".into()]);
        assert_eq!(cp.project("linux-x86_64").len(), 3);
        assert_eq!(cp.project("linux-aarch64").len(), 2);
        assert_eq!(cp.common().len(), 2, "the module and g:a:1");
        let by = Resolution::BySystem(
            [("linux-x86_64".to_string(), vec![art("g:a:1")]), ("macos-aarch64".to_string(), vec![])].into(),
        );
        let cp = by.classpath(vec!["util@jvm-3".into()]);
        assert_eq!(cp.project("macos-aarch64"), [ClasspathEntry::Module { module: "util@jvm-3".into() }]);
        assert_eq!(by.artifacts().len(), 1);
    }

    #[test]
    fn semanticdb_versions_are_published_ones() {
        assert_eq!(semanticdb_version("2.13.18"), Some("4.17.4"));
        assert_eq!(semanticdb_version("2.13.14"), Some("4.14.1"));
        assert_eq!(semanticdb_version("2.12.17"), Some("4.14.2"));
        assert_eq!(semanticdb_version("2.12.3"), None);
        assert_eq!(semanticdb_version("2.13.99"), None, "newer than the table");
        assert_eq!(semanticdb_version("3.9.0"), None);
    }

    #[test]
    fn jdk_majors() {
        assert_eq!(jdk_major("temurin:25.0.1").as_deref(), Some("25"));
        assert_eq!(jdk_major("21").as_deref(), Some("21"));
        assert_eq!(jdk_major("zulu:").as_deref(), None);
    }

    #[test]
    fn boot_roots_per_scala_version() {
        let s3 = boot_roots("3.9.0");
        assert_eq!(s3[0].roots[0], "org.scala-lang:scala3-compiler_3:3.9.0");
        assert_eq!(s3[1].roots[0], "org.scala-lang:scala3-sbt-bridge:3.9.0");
        assert_eq!(s3[1].name, "compiler-bridge@3.9.0");
        let s2 = boot_roots("2.13.18");
        assert_eq!(s2[1].roots[0], format!("org.scala-sbt:compiler-bridge_2.13:{ZINC_VERSION}"));
        assert_eq!(s2[1].name, format!("compiler-bridge@2.13.18+zinc-{ZINC_VERSION}"), "a Zinc upgrade re-resolves it");
    }

    #[test]
    fn boot_inputs_track_repositories_and_context() {
        let ctx = LockContext { systems: vec!["linux-x86_64".into()], default_systems: Some(1), jdk: "25".into() };
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
        assert_ne!(base, boot_inputs_hash(&central, &LockContext { jdk: "21".into(), ..ctx.clone() }));
        assert_eq!(
            base,
            boot_inputs_hash(&central, &LockContext { default_systems: None, ..ctx.clone() }),
            "where the set came from is not an input"
        );
        // `groups` are an input; without them the hash is what it was before they existed.
        let grouped = vec![Repo { groups: vec!["com.acme".into()], ..central[0].clone() }];
        assert_ne!(base, boot_inputs_hash(&grouped, &ctx));
        assert_eq!(
            repo_inputs(&central),
            [serde_json::json!(["central", coursier::CENTRAL_URL])],
            "the shape locks without `groups` were hashed with"
        );
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
        let ctx = lock_context(&ws, &Lockfile::default(), false);
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
            inputs_hash(
                &ws,
                &graph,
                ws.variant("a@jvm-3").unwrap(),
                &[],
                &lock_context(&ws, &Lockfile::default(), false),
            )
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
    fn restores_try_the_hint_then_the_allowed_repositories() {
        let repo = |id: &str, groups: &[&str]| Repo {
            id: id.into(),
            url: format!("https://{id}.example/m/"),
            groups: groups.iter().map(|g| g.to_string()).collect(),
            ..Default::default()
        };
        let repos = vec![repo("central", &[]), repo("other", &[]), repo("acme", &["com.acme"])];
        let mut lock = Lockfile::default();
        let entry = |repo: &str| ArtifactLock { repo: repo.into(), sha256: "0".into(), size: 1 };
        lock.boot.insert("x".into(), boot_list(&["g:x:1", "com.acme:y:1", "g:z:1"]));
        lock.artifacts.insert("g:x:1".into(), entry("other"));
        lock.artifacts.insert("com.acme:y:1".into(), entry("central"));
        lock.artifacts.insert("g:z:1".into(), entry("removed"));
        let dir = tempfile::tempdir().unwrap();
        let items = fetch_items(&lock, &repos, &CoursierCredentials::default(), dir.path(), &mut Vec::new()).unwrap();
        let urls = |key: &str| -> Vec<String> {
            let item = items.iter().find(|i| i.key == key).unwrap();
            std::iter::once(item).chain(&item.fallbacks).map(|i| i.url.clone()).collect()
        };
        // The recorded repository first, then the others in declared order; never `acme`, whose
        // `groups` don't cover `g`.
        assert_eq!(urls("g:x:1"), ["https://other.example/m/g/x/1/x-1.jar", "https://central.example/m/g/x/1/x-1.jar"]);
        // An internal artifact is restored only from the repository its group belongs to, whatever
        // the hint says.
        assert_eq!(urls("com.acme:y:1"), ["https://acme.example/m/com/acme/y/1/y-1.jar"]);
        // A repository that is no longer configured is simply not asked.
        assert_eq!(urls("g:z:1"), ["https://central.example/m/g/z/1/z-1.jar", "https://other.example/m/g/z/1/z-1.jar"]);
        // Every repository restricted, none covering the group: an error.
        let err =
            fetch_items(&lock, &repos[2..], &CoursierCredentials::default(), dir.path(), &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("no repository serves group `g`"), "{err}");
    }

    #[test]
    fn another_repositorys_cached_copy_is_used_only_if_its_sha256_matches() {
        let repos: Vec<Repo> = ["central", "other"]
            .iter()
            .map(|id| Repo { id: id.to_string(), url: format!("https://{id}.example/"), ..Default::default() })
            .collect();
        let cache = tempfile::tempdir().unwrap();
        let good = cache.path().join("good");
        std::fs::write(&good, b"right").unwrap();
        let mut lock = Lockfile::default();
        lock.boot.insert("x".into(), boot_list(&["g:x:1"]));
        lock.artifacts.insert(
            "g:x:1".into(),
            ArtifactLock { repo: "central".into(), sha256: fetch::sha256_file(&good).unwrap(), size: 5 },
        );
        let other_copy = coursier::cache_path(cache.path(), "https://other.example/g/x/1/x-1.jar", None).unwrap();
        std::fs::create_dir_all(other_copy.parent().unwrap()).unwrap();
        let first_url = |lock: &Lockfile| {
            fetch_items(lock, &repos, &CoursierCredentials::default(), cache.path(), &mut Vec::new()).unwrap()[0]
                .url
                .clone()
        };
        // Same size, other bytes: not used; the hint stays first (and is what a download asks).
        std::fs::write(&other_copy, b"wrong").unwrap();
        assert_eq!(first_url(&lock), "https://central.example/g/x/1/x-1.jar");
        std::fs::write(&other_copy, b"right").unwrap();
        assert_eq!(first_url(&lock), "https://other.example/g/x/1/x-1.jar");
    }

    #[test]
    fn an_internal_artifact_found_elsewhere_is_refused() {
        let repos = vec![
            Repo { id: "public".into(), url: "https://public.example/".into(), ..Default::default() },
            Repo {
                id: "acme".into(),
                url: "https://acme.example/".into(),
                groups: vec!["com.acme".into()],
                ..Default::default()
            },
        ];
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.jar");
        std::fs::write(&file, b"jar").unwrap();
        let from = |host: &str, key: &str, path: &str| ResolvedArtifact {
            key: key.into(),
            file: file.clone(),
            url: format!("https://{host}.example/{path}"),
        };
        let mut lock = Lockfile::default();
        // `cs` missed at acme and took com.acme:x from public: refused, not locked.
        let err = record_artifacts(&mut lock, &[from("public", "com.acme:x:1", "com/acme/x/1/x-1.jar")], &repos)
            .unwrap_err()
            .to_string();
        assert!(err.contains("com.acme:x:1 was found in repository `public`"), "{err}");
        assert!(err.contains("may only come from `acme`") && err.contains("dependency confusion"), "{err}");
        assert!(lock.artifacts.is_empty());
        // The other direction: an uncovered group served by a repository with `groups` (a combined
        // URL that also proxies Central) is refused too, not relabelled.
        let err = record_artifacts(&mut lock, &[from("acme", "org.x:y:1", "org/x/y/1/y-1.jar")], &repos)
            .unwrap_err()
            .to_string();
        assert!(err.contains("org.x:y:1 was served by repository `acme`"), "{err}");
        assert!(err.contains("may only come from the repositories without `groups` (`public`)"), "{err}");
        assert!(err.contains("internal-only repository URL") && err.contains("mirror of `central`"), "{err}");
        assert!(lock.artifacts.is_empty());

        // The same through a user config mirror: the mirror, one machine's setting, is to blame,
        // not the repository's URL in the workspace.
        let mirror = |url: &str| {
            Some(coursier::Mirror { url: url.into(), matched: "acme".into(), allow_insecure_credentials: false })
        };
        let mirrored =
            vec![repos[0].clone(), Repo { mirror: mirror("https://nexus.example/group/"), ..repos[1].clone() }];
        let err = record_artifacts(&mut lock, &[from("nexus", "org.x:y:1", "group/org/x/y/1/y-1.jar")], &mirrored)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("served by repository `acme` through the user config mirror https://nexus.example/group/"),
            "{err}"
        );
        assert!(
            err.contains("point the mirror at an internal-only URL") && err.contains("don't mirror `acme`"),
            "{err}"
        );
        assert!(!err.contains("point `acme` at"), "the workspace URL is fine: {err}");
        let mirrored =
            vec![Repo { mirror: mirror("https://nexus.example/group/"), ..repos[0].clone() }, repos[1].clone()];
        let err =
            record_artifacts(&mut lock, &[from("nexus", "com.acme:x:1", "group/com/acme/x/1/x-1.jar")], &mirrored)
                .unwrap_err()
                .to_string();
        assert!(err.contains("found in repository `public` through the user config mirror"), "{err}");
        assert!(err.contains("dependency confusion") && err.contains("don't mirror `public` with it"), "{err}");
        assert!(lock.artifacts.is_empty());
    }

    #[test]
    fn groups_pick_repositories_by_longest_prefix() {
        let repo = |id: &str, groups: &[&str]| Repo {
            id: id.into(),
            url: format!("https://{id}.example/"),
            groups: groups.iter().map(|g| g.to_string()).collect(),
            ..Default::default()
        };
        let repos = vec![
            repo("central", &[]),
            repo("acme", &["com.acme"]),
            repo("internal", &["com.acme.internal"]),
            repo("mirror-of-internal", &["com.acme.internal", "org.x"]),
        ];
        let ids = |group: &str| -> Vec<String> {
            coursier::allowed_repos(&repos, group).into_iter().map(|r| r.id.clone()).collect()
        };
        assert_eq!(ids("com.acme"), ["acme"]);
        assert_eq!(ids("com.acme.util"), ["acme"]);
        assert_eq!(ids("com.acme.internal.db"), ["internal", "mirror-of-internal"], "longest prefix; ties keep order");
        assert_eq!(ids("com.acmes"), ["central"], "prefixes end at a dot");
        assert_eq!(ids("org.x"), ["mirror-of-internal"]);
        assert_eq!(ids("org.typelevel"), ["central"]);
    }

    #[test]
    fn invalid_lock_keys_are_errors() {
        let mut lock = Lockfile::default();
        lock.boot.insert("x".into(), boot_list(&["g:..:1"]));
        lock.artifacts.insert("g:..:1".into(), ArtifactLock { repo: "central".into(), sha256: "0".into(), size: 1 });
        let repos = vec![Repo {
            id: "central".into(),
            url: coursier::CENTRAL_URL.into(),
            credentials: None,
            allow_insecure_credentials: false,
            ..Default::default()
        }];
        let err =
            fetch_items(&lock, &repos, &CoursierCredentials::default(), Path::new("/c"), &mut Vec::new()).unwrap_err();
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
        let mirror = |url: &str| {
            Some(coursier::Mirror { url: url.into(), matched: "*".into(), allow_insecure_credentials: false })
        };
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
        lock.boot.insert("b".into(), boot_list(&["g:a:1"]));
        let items =
            fetch_items(&lock, &repos, &CoursierCredentials::default(), Path::new("/c"), &mut Vec::new()).unwrap();
        assert_eq!(items[0].url, "https://m/all/own/g/a/1/a-1.jar");
        assert_eq!(items[0].dest, Path::new("/c/https/m/all/own/g/a/1/a-1.jar"));
    }

    #[test]
    fn the_fetcher_sends_an_origins_credentials_like_cs() {
        let token = coursier::RepoCredentials::Bearer { token: coursier::Secret::Literal("t0k".into()) };
        let repos = vec![
            Repo {
                id: "acme".into(),
                url: "https://acme.example/libs/".into(),
                credentials: Some(token),
                ..Default::default()
            },
            Repo {
                id: "central".into(),
                url: coursier::CENTRAL_URL.into(),
                mirror: Some(coursier::Mirror {
                    url: "https://acme.example:443/remote/".into(),
                    matched: "*".into(),
                    allow_insecure_credentials: false,
                }),
                ..Default::default()
            },
        ];
        let mut lock = Lockfile::default();
        lock.artifacts.insert("g:a:1".into(), ArtifactLock { repo: "central".into(), sha256: "0".into(), size: 1 });
        lock.boot.insert("b".into(), boot_list(&["g:a:1"]));
        let items =
            fetch_items(&lock, &repos, &CoursierCredentials::default(), Path::new("/c"), &mut Vec::new()).unwrap();
        assert_eq!(items[0].auth, Ok(Some(Auth::Bearer("t0k".into()))));
        assert!(
            items[0].dest.starts_with("/c/https/token%40acme.example%3A443/remote/"),
            "{}",
            items[0].dest.display()
        );
    }

    #[test]
    fn a_notice_keeps_the_error_it_is_added_to() {
        let failure = fetch::FetchFailure {
            errors: vec![fetch::FetchError::Proxy("HTTPS_PROXY: bad".into())],
            attempted: BTreeMap::new(),
        };
        let e = InstallError::WithNotice {
            error: Box::new(InstallError::Fetch(failure)),
            notice: "settings.xml: …".into(),
        };
        assert_eq!(e.to_string(), "HTTPS_PROXY: bad\n  note: settings.xml: …");
        let InstallError::WithNotice { error, .. } = e else { unreachable!() };
        assert!(matches!(*error, InstallError::Fetch(ref f) if f.errors.len() == 1));
    }

    #[test]
    fn relocking_through_a_shared_mirror_keeps_existing_repository_ids() {
        let mirror = Some(coursier::Mirror {
            url: "https://m/all/".into(),
            matched: "*".into(),
            allow_insecure_credentials: false,
        });
        let repos = vec![
            Repo { id: "central".into(), url: "https://c/".into(), mirror: mirror.clone(), ..Default::default() },
            Repo { id: "acme".into(), url: "https://a/".into(), mirror, ..Default::default() },
        ];
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x-1.jar");
        std::fs::write(&file, b"jar").unwrap();
        let (size, sha256) = (3, fetch::sha256_file(&file).unwrap());
        // Locked without the mirror: `g:x:1` exists only in acme.
        let mut lock = Lockfile::default();
        lock.artifacts.insert("g:x:1".into(), ArtifactLock { repo: "acme".into(), sha256: sha256.clone(), size });
        lock.artifacts.insert("g:y:1".into(), ArtifactLock { repo: "central".into(), sha256: sha256.clone(), size });
        let via_mirror = |key: &str, path: &str| ResolvedArtifact {
            key: key.into(),
            file: file.clone(),
            url: format!("https://m/all/{path}"),
        };
        let resolved = [via_mirror("g:x:1", "g/x/1/x-1.jar"), via_mirror("g:y:1", "g/y/1/y-1.jar")];
        record_artifacts(&mut lock, &resolved, &repos).unwrap();
        assert_eq!(lock.artifacts["g:x:1"].repo, "acme", "a restore without the mirror still finds it");
        assert_eq!(lock.artifacts["g:y:1"].repo, "central");
        // A new artifact gets the first sharer: the id is a hint, and a restore without the
        // mirror that misses there asks the others (no lookup, no error).
        record_artifacts(&mut lock, &[via_mirror("g:z:1", "g/z/1/z-1.jar")], &repos).unwrap();
        assert_eq!(lock.artifacts["g:z:1"], ArtifactLock { repo: "central".into(), sha256: sha256.clone(), size });
        // `groups` decide among the sharers of one mirror.
        let grouped = vec![repos[0].clone(), Repo { groups: vec!["com.acme".into()], ..repos[1].clone() }];
        record_artifacts(&mut lock, &[via_mirror("com.acme:w:1", "com/acme/w/1/w-1.jar")], &grouped).unwrap();
        assert_eq!(lock.artifacts["com.acme:w:1"].repo, "acme");
        record_artifacts(&mut lock, &[via_mirror("g:v:1", "g/v/1/v-1.jar")], &grouped).unwrap();
        assert_eq!(lock.artifacts["g:v:1"].repo, "central");
        let only_acme = vec![Repo { groups: vec!["com.acme".into()], ..repos[1].clone() }];
        let err = record_artifacts(&mut lock, &[via_mirror("g:u:1", "g/u/1/u-1.jar")], &only_acme).unwrap_err();
        assert!(err.to_string().contains("no repository serves group `g`"), "{err}");
        // An id that no longer shares the mirror is re-derived.
        let unshared = vec![repos[0].clone(), Repo { mirror: None, ..repos[1].clone() }];
        record_artifacts(&mut lock, &[via_mirror("g:x:1", "g/x/1/x-1.jar")], &unshared).unwrap();
        assert_eq!(lock.artifacts["g:x:1"].repo, "central");
    }

    /// A v1 lock converts with every hash kept (design §5.4): these are the values Phase 1's
    /// code computed for this workspace with its default resolution context.
    #[test]
    fn single_system_hashes_equal_version_1s() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("scpm-workspace.yaml"),
            "modules: ['modules/*']\ntoolchain:\n  scala: 3.9.0\n  jdk: temurin:21\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("modules/a")).unwrap();
        std::fs::write(
            dir.path().join("modules/a/scpm.yaml"),
            "dependencies:\n  org.typelevel::cats-core: 2.13.0\n  g:x: ^1.0\n",
        )
        .unwrap();
        let ws = crate::workspace::load(dir.path()).unwrap();
        let graph = Graph::build(&ws).unwrap();
        let repos = vec![Repo { id: "central".into(), url: coursier::CENTRAL_URL.into(), ..Default::default() }];
        let ctx = lock_context(&ws, &Lockfile::default(), false);
        assert_eq!((ctx.systems.as_slice(), ctx.jdk.as_str()), (&["linux-x86_64".to_string()][..], "21"));
        assert_eq!(
            inputs_hash(&ws, &graph, ws.variant("a@jvm-3").unwrap(), &repos, &ctx),
            "sha256:aac1e06bcd0f976d7ff803e015880b4b3de8765f9da6c27ca246e81394c1ecd3"
        );
        assert_eq!(
            boot_inputs_hash(&repos, &ctx),
            "sha256:af971f6bd121ad25ba9a2705973f9a3719646fa531bacbeff61581ea668b953d"
        );
        // Another system set is another context.
        let two = LockContext { systems: vec!["linux-x86_64".into(), "macos-aarch64".into()], ..ctx.clone() };
        assert_ne!(boot_inputs_hash(&repos, &two), boot_inputs_hash(&repos, &ctx));
    }
}
