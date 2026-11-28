//! `scpm-lock.yaml` (design §5.4).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const LOCK_FILE: &str = "scpm-lock.yaml";
pub const LOCKFILE_VERSION: u32 = 2;

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lockfile {
    pub lockfile_version: u32,
    /// The system set (sorted): every classpath is locked for each of these (design §5.4, "Systems").
    pub systems: Vec<String>,
    /// The version of scpm's default set the header came from; absent when `lock.systems` set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_systems: Option<u32>,
    /// The built-in native mapping table this lock was written with (only the sidecar writes it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_mappings: Option<u32>,
    /// The JDK major version profiles are activated for (`lock.jdk`).
    pub jdk: String,
    /// sha256 over what boot resolutions depend on (repositories, system set, JDK); when it
    /// changes every boot entry is re-resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_inputs: Option<String>,
    /// The boot section was resolved with the full resolution context (OS, arch and JDK of every
    /// system), which only the resolver sidecar applies. One flag for every boot entry.
    #[serde(default, skip_serializing_if = "is_false")]
    pub boot_full_context: bool,
    /// Closures needed before a JVM starts: compiler, bridge, worker, tools.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub boot: BTreeMap<String, Classpath>,
    /// Selections for catalog ranges, keyed `<catalog>/<key>@<platform>-<scala binary>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub catalogs: BTreeMap<String, CatalogSelection>,
    #[serde(default)]
    pub variants: BTreeMap<String, VariantLock>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactLock>,
}

/// An OS and architecture (the machine that ran the resolver; reported, not locked).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

impl Platform {
    pub fn current() -> Platform {
        Platform { os: std::env::consts::OS.to_string(), arch: std::env::consts::ARCH.to_string() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSelection {
    pub range: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VariantLock {
    /// Resolved with the full resolution context (OS, arch and JDK of every system): written
    /// only by the resolver sidecar (design §5.4).
    #[serde(default, skip_serializing_if = "is_false")]
    pub full_context: bool,
    pub specifiers: BTreeMap<String, Specifier>,
    pub inputs_hash: String,
    pub compile_classpath: Classpath,
    pub runtime_classpath: Classpath,
    pub test_compile_classpath: Classpath,
    pub test_runtime_classpath: Classpath,
    #[serde(default, skip_serializing_if = "Classpath::is_empty")]
    pub compiler_plugins: Classpath,
}

impl VariantLock {
    /// The four classpaths and the compiler plugins.
    pub fn classpaths(&self) -> [&Classpath; 5] {
        [
            &self.compile_classpath,
            &self.runtime_classpath,
            &self.test_compile_classpath,
            &self.test_runtime_classpath,
            &self.compiler_plugins,
        ]
    }

    fn classpaths_mut(&mut self) -> [&mut Classpath; 5] {
        [
            &mut self.compile_classpath,
            &mut self.runtime_classpath,
            &mut self.test_compile_classpath,
            &mut self.test_runtime_classpath,
            &mut self.compiler_plugins,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Specifier {
    pub spec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// An artifact key, or a reference to an upstream workspace variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClasspathEntry {
    Artifact(String),
    Module { module: String },
}

/// A classpath entry and, when it isn't on every system of the lock, the systems it is on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LockEntry {
    pub entry: ClasspathEntry,
    /// `None`: on every system. Otherwise sorted.
    pub systems: Option<Vec<String>>,
}

impl LockEntry {
    pub fn plain(entry: ClasspathEntry) -> LockEntry {
        LockEntry { entry, systems: None }
    }

    /// An entry on `systems` (sorted and de-duplicated here), or on every system for `None`.
    /// Workspace modules are on every system, so only artifacts may name systems.
    pub fn new(entry: ClasspathEntry, systems: Option<Vec<String>>) -> LockEntry {
        debug_assert!(
            systems.is_none() || matches!(entry, ClasspathEntry::Artifact(_)),
            "a workspace module is on every system: {entry:?}"
        );
        LockEntry { entry, systems: systems.map(crate::system::normalized) }
    }

    pub fn applies_to(&self, system: &str) -> bool {
        self.systems.as_ref().is_none_or(|s| s.iter().any(|x| x == system))
    }
}

/// A locked classpath (design §5.4, "Lock format"): one ordered list whose entries carry
/// `systems` when they aren't on every system, or, only when the per-system orders can't be
/// merged into one, a list per system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ClasspathRepr", into = "ClasspathRepr")]
pub enum Classpath {
    List(Vec<LockEntry>),
    BySystem(BTreeMap<String, Vec<ClasspathEntry>>),
}

impl Default for Classpath {
    fn default() -> Self {
        Classpath::List(Vec::new())
    }
}

impl Classpath {
    /// A classpath that is the same on every system.
    pub fn plain(entries: impl IntoIterator<Item = ClasspathEntry>) -> Classpath {
        Classpath::List(entries.into_iter().map(LockEntry::plain).collect())
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Classpath::List(l) => l.is_empty(),
            Classpath::BySystem(m) => m.values().all(Vec::is_empty),
        }
    }

    /// The entries on every system, in order. On a projection ([`Lockfile::project`], which is
    /// what `install::ensure` returns) that is the whole classpath.
    pub fn common(&self) -> Vec<&ClasspathEntry> {
        match self {
            Classpath::List(l) => l.iter().filter(|e| e.systems.is_none()).map(|e| &e.entry).collect(),
            Classpath::BySystem(_) => Vec::new(),
        }
    }

    /// One system's classpath: the entries without `systems` or listing it, in list order.
    pub fn project(&self, system: &str) -> Vec<ClasspathEntry> {
        match self {
            Classpath::List(l) => l.iter().filter(|e| e.applies_to(system)).map(|e| e.entry.clone()).collect(),
            Classpath::BySystem(m) => m.get(system).cloned().unwrap_or_default(),
        }
    }

    /// Whether any entry is on only some systems.
    pub fn is_system_specific(&self) -> bool {
        match self {
            Classpath::List(l) => l.iter().any(|e| e.systems.is_some()),
            Classpath::BySystem(_) => true,
        }
    }

    /// Every entry of every system.
    pub fn all_entries(&self) -> Vec<&ClasspathEntry> {
        match self {
            Classpath::List(l) => l.iter().map(|e| &e.entry).collect(),
            Classpath::BySystem(m) => m.values().flatten().collect(),
        }
    }

    /// Artifact keys of every system.
    pub fn artifact_keys(&self) -> impl Iterator<Item = &str> {
        self.all_entries().into_iter().filter_map(|e| match e {
            ClasspathEntry::Artifact(k) => Some(k.as_str()),
            ClasspathEntry::Module { .. } => None,
        })
    }
}

/// How a classpath is written: a list, or `{bySystem: {<system>: [...]}}`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ClasspathRepr {
    List(Vec<EntryRepr>),
    BySystem(BySystemRepr),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BySystemRepr {
    by_system: BTreeMap<String, Vec<ClasspathEntry>>,
}

/// An entry as written: a plain key, `{module}`, or `{artifact, systems}`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum EntryRepr {
    Artifact(String),
    Module(ModuleRepr),
    Scoped(ScopedRepr),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleRepr {
    module: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedRepr {
    artifact: String,
    systems: Vec<String>,
}

impl From<ClasspathRepr> for Classpath {
    fn from(r: ClasspathRepr) -> Self {
        match r {
            ClasspathRepr::List(l) => Classpath::List(
                l.into_iter()
                    .map(|e| match e {
                        EntryRepr::Artifact(k) => LockEntry::plain(ClasspathEntry::Artifact(k)),
                        EntryRepr::Module(m) => LockEntry::plain(ClasspathEntry::Module { module: m.module }),
                        EntryRepr::Scoped(s) => LockEntry::new(ClasspathEntry::Artifact(s.artifact), Some(s.systems)),
                    })
                    .collect(),
            ),
            ClasspathRepr::BySystem(b) => Classpath::BySystem(b.by_system),
        }
    }
}

impl From<Classpath> for ClasspathRepr {
    fn from(c: Classpath) -> Self {
        match c {
            Classpath::List(l) => ClasspathRepr::List(
                l.into_iter()
                    .map(|e| match (e.entry, e.systems) {
                        (ClasspathEntry::Artifact(k), None) => EntryRepr::Artifact(k),
                        (ClasspathEntry::Artifact(k), Some(systems)) => {
                            EntryRepr::Scoped(ScopedRepr { artifact: k, systems })
                        }
                        // Workspace modules are on every system (`LockEntry::new` checks).
                        (ClasspathEntry::Module { module }, systems) => {
                            debug_assert!(systems.is_none(), "module {module} with systems");
                            EntryRepr::Module(ModuleRepr { module })
                        }
                    })
                    .collect(),
            ),
            Classpath::BySystem(by_system) => ClasspathRepr::BySystem(BySystemRepr { by_system }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactLock {
    /// Repository id from the workspace config (never a mirror).
    pub repo: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("cannot read {LOCK_FILE}: {0}")]
    Io(#[from] std::io::Error),
    #[error("{LOCK_FILE} is invalid: {0}")]
    Parse(String),
    #[error("{LOCK_FILE} contains merge conflict markers (line {0})")]
    Conflict(usize),
    #[error("{LOCK_FILE} has lockfileVersion {0}; this scpm understands versions 1 and {LOCKFILE_VERSION}")]
    Version(u32),
    #[error("cannot write {}: {source}", .path.display())]
    Write { path: std::path::PathBuf, source: std::io::Error },
}

impl LockError {
    /// A lock that can't be used as it is, but can be rebuilt by re-locking from scratch.
    pub fn is_corrupt(&self) -> bool {
        matches!(self, LockError::Parse(_) | LockError::Conflict(_))
    }
}

/// Phase 1's format (version 1): one `resolutionContext` instead of `systems` and `jdk`, and
/// plain classpath lists. Read only, to convert (design §5.4, "Converting a v1 lock").
mod v1 {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct Lockfile {
        #[allow(dead_code)]
        pub lockfile_version: u32,
        pub resolution_context: ResolutionContext,
        #[serde(default)]
        pub boot_inputs: Option<String>,
        #[serde(default)]
        pub boot: BTreeMap<String, Vec<String>>,
        #[serde(default)]
        pub catalogs: BTreeMap<String, CatalogSelection>,
        #[serde(default)]
        pub variants: BTreeMap<String, VariantLock>,
        #[serde(default)]
        pub artifacts: BTreeMap<String, ArtifactLock>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ResolutionContext {
        pub os: String,
        pub arch: String,
        pub jdk: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct VariantLock {
        pub specifiers: BTreeMap<String, Specifier>,
        pub inputs_hash: String,
        pub compile_classpath: Vec<ClasspathEntry>,
        pub runtime_classpath: Vec<ClasspathEntry>,
        pub test_compile_classpath: Vec<ClasspathEntry>,
        pub test_runtime_classpath: Vec<ClasspathEntry>,
        #[serde(default)]
        pub compiler_plugins: Vec<String>,
    }

    /// Phase 1's default resolution context, which converts to `defaultSystems: 1`.
    const DEFAULT: (&str, &str) = ("linux", "x86_64");

    impl Lockfile {
        /// `systems: [<os>-<arch>]` and `jdk`, plus `defaultSystems: 1` for Phase 1's default
        /// context. Every `inputsHash` is kept: a single-system set hashes as v1's context did.
        /// No variant (and not the boot section) carries `fullContext`.
        pub fn convert(self) -> super::Lockfile {
            let ctx = self.resolution_context;
            let artifacts = |list: Vec<String>| Classpath::plain(list.into_iter().map(ClasspathEntry::Artifact));
            // Phase 1 took any spelling (`amd64`, `Darwin`); the system is the canonical name.
            // Only canonical spellings (`linux`/`x86_64`) hash as before, so others re-lock once.
            let system =
                crate::system::from_os_arch(&ctx.os, &ctx.arch).unwrap_or_else(|| format!("{}-{}", ctx.os, ctx.arch));
            super::Lockfile {
                lockfile_version: LOCKFILE_VERSION,
                // Phase 1's default context was default set version 1.
                default_systems: (system == format!("{}-{}", DEFAULT.0, DEFAULT.1)).then_some(1),
                systems: vec![system],
                native_mappings: None,
                jdk: ctx.jdk,
                boot_inputs: self.boot_inputs,
                boot_full_context: false,
                boot: self.boot.into_iter().map(|(k, v)| (k, artifacts(v))).collect(),
                catalogs: self.catalogs,
                variants: self
                    .variants
                    .into_iter()
                    .map(|(id, v)| {
                        let lock = super::VariantLock {
                            full_context: false,
                            specifiers: v.specifiers,
                            inputs_hash: v.inputs_hash,
                            compile_classpath: Classpath::plain(v.compile_classpath),
                            runtime_classpath: Classpath::plain(v.runtime_classpath),
                            test_compile_classpath: Classpath::plain(v.test_compile_classpath),
                            test_runtime_classpath: Classpath::plain(v.test_runtime_classpath),
                            compiler_plugins: artifacts(v.compiler_plugins),
                        };
                        (id, lock)
                    })
                    .collect(),
                artifacts: self.artifacts,
            }
        }
    }
}

impl Lockfile {
    /// Reads the lockfile at `root`, or `None` if there is none. A version 1 lock is converted.
    pub fn read(root: &Path) -> Result<Option<Lockfile>, LockError> {
        let path = root.join(LOCK_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Lockfile::parse(&text).map(Some)
    }

    pub fn parse(text: &str) -> Result<Lockfile, LockError> {
        if let Some(line) = text.lines().position(|l| {
            l.starts_with("<<<<<<< ") || l == "=======" || l.starts_with(">>>>>>> ") || l.starts_with("||||||| ")
        }) {
            return Err(LockError::Conflict(line + 1));
        }
        // The version first, leniently: a newer format may add fields this version's strict
        // schema rejects, and such a lock must not be mistaken for a corrupt (rebuildable) one.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Header {
            lockfile_version: Option<u32>,
        }
        let version = match serde_yaml_ng::from_str::<Header>(text) {
            Ok(Header { lockfile_version: Some(version) }) => version,
            _ => LOCKFILE_VERSION,
        };
        let parse_err = |e: serde_yaml_ng::Error| LockError::Parse(e.to_string());
        match version {
            1 => Ok(serde_yaml_ng::from_str::<v1::Lockfile>(text).map_err(parse_err)?.convert()),
            LOCKFILE_VERSION => {
                let lock: Lockfile = serde_yaml_ng::from_str(text).map_err(parse_err)?;
                if lock.lockfile_version != LOCKFILE_VERSION {
                    return Err(LockError::Version(lock.lockfile_version));
                }
                Ok(lock)
            }
            other => Err(LockError::Version(other)),
        }
    }

    pub fn to_yaml(&self) -> String {
        let body = serde_yaml_ng::to_string(self).expect("lockfile serializes");
        format!("# Generated by scpm. Do not edit by hand; run `scpm install`.\n{body}")
    }

    /// Writes the lockfile if its content changed. Returns whether it was written.
    pub fn write(&self, root: &Path) -> Result<bool, LockError> {
        let path = root.join(LOCK_FILE);
        let new = self.to_yaml();
        if fs::read_to_string(&path).is_ok_and(|old| old == new) {
            return Ok(false);
        }
        // A unique temporary file in the same directory, renamed into place: concurrent
        // writers never see (or clobber) each other's partial files.
        let write_err = |source: std::io::Error| LockError::Write { path: path.clone(), source };
        let mut tmp = tempfile::Builder::new()
            .prefix(&format!(".{LOCK_FILE}."))
            .suffix(".tmp")
            .tempfile_in(root)
            .map_err(write_err)?;
        tmp.write_all(new.as_bytes()).and_then(|()| tmp.as_file().sync_all()).map_err(write_err)?;
        tmp.persist(&path).map_err(|e| write_err(e.error))?;
        Ok(true)
    }

    fn classpaths(&self) -> impl Iterator<Item = &Classpath> {
        self.boot.values().chain(self.variants.values().flat_map(|v| v.classpaths()))
    }

    /// Every artifact key referenced by a variant or the boot section, on any system.
    pub fn referenced_artifacts(&self) -> BTreeSet<&str> {
        self.classpaths().flat_map(Classpath::artifact_keys).collect()
    }

    /// Whether any classpath or boot entry is on only some systems.
    pub fn has_system_specific_entries(&self) -> bool {
        self.classpaths().any(Classpath::is_system_specific)
    }

    /// The lock as `system` sees it: every classpath and boot entry is that system's projection,
    /// with no `systems` left. For a system outside the set, that is the entries on every system.
    pub fn project(&self, system: &str) -> Lockfile {
        let mut lock = self.clone();
        let project = |c: &mut Classpath| *c = Classpath::plain(c.project(system));
        lock.boot.values_mut().for_each(project);
        for v in lock.variants.values_mut() {
            v.classpaths_mut().into_iter().for_each(project);
        }
        lock
    }

    /// Drops artifacts and catalog selections that nothing references any more.
    pub fn prune(&mut self, used_catalog_keys: &BTreeSet<String>) {
        let used: BTreeSet<String> = self.referenced_artifacts().into_iter().map(str::to_string).collect();
        self.artifacts.retain(|k, _| used.contains(k));
        self.catalogs.retain(|k, _| used_catalog_keys.contains(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Lockfile {
        Lockfile {
            lockfile_version: LOCKFILE_VERSION,
            systems: vec!["linux-x86_64".into()],
            jdk: "25".into(),
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_through_yaml() {
        let mut lock = header();
        lock.variants.insert(
            "core@jvm-3".into(),
            VariantLock {
                specifiers: [("util".to_string(), Specifier { spec: "workspace:*".into(), version: None })].into(),
                inputs_hash: "sha256:abc".into(),
                compile_classpath: Classpath::plain([
                    ClasspathEntry::Module { module: "util@jvm-3".into() },
                    ClasspathEntry::Artifact("org.typelevel:cats-core_3:2.13.0".into()),
                ]),
                ..Default::default()
            },
        );
        lock.artifacts.insert(
            "org.typelevel:cats-core_3:2.13.0".into(),
            ArtifactLock { repo: "central".into(), sha256: "00".into(), size: 1 },
        );
        let yaml = lock.to_yaml();
        assert!(yaml.contains("- module: util@jvm-3"), "{yaml}");
        assert!(!yaml.contains("fullContext") && !yaml.contains("compilerPlugins"), "{yaml}");
        let back = Lockfile::parse(&yaml).unwrap();
        assert_eq!(back, lock);
        assert_eq!(
            back.referenced_artifacts().into_iter().collect::<Vec<_>>(),
            vec!["org.typelevel:cats-core_3:2.13.0"]
        );
    }

    /// The lock format of design §5.4: plain strings for entries on every system, `{artifact,
    /// systems}` for the others, and `bySystem` replacing a whole classpath.
    #[test]
    fn system_specific_entries_and_projections() {
        let yaml = "lockfileVersion: 2
systems: [linux-aarch64, linux-x86_64, macos-aarch64]
nativeMappings: 1
jdk: '25'
bootFullContext: true
boot:
  b: [g:common:1, {artifact: 'g:n:1:linux-x86_64', systems: [linux-x86_64]}]
variants:
  server@jvm-3:
    fullContext: true
    specifiers: {}
    inputsHash: sha256:x
    compileClasspath:
      - module: util@jvm-3
      - g:common:1
      - {artifact: 'g:n:1:linux-aarch_64', systems: [linux-aarch64]}
      - {artifact: 'g:n:1:linux-x86_64', systems: [linux-x86_64]}
    runtimeClasspath:
      bySystem:
        linux-aarch64: [g:a:1, g:b:1]
        linux-x86_64: [g:b:1, g:a:1]
        macos-aarch64: [g:a:1]
    testCompileClasspath: []
    testRuntimeClasspath: []
";
        let lock = Lockfile::parse(yaml).unwrap();
        assert!(lock.boot_full_context && lock.variants["server@jvm-3"].full_context);
        assert!(lock.has_system_specific_entries());
        let v = &lock.variants["server@jvm-3"];
        let keys = |l: Vec<ClasspathEntry>| -> Vec<String> {
            l.into_iter()
                .map(|e| match e {
                    ClasspathEntry::Artifact(k) => k,
                    ClasspathEntry::Module { module } => format!("module:{module}"),
                })
                .collect()
        };
        assert_eq!(
            keys(v.compile_classpath.project("linux-x86_64")),
            ["module:util@jvm-3", "g:common:1", "g:n:1:linux-x86_64"]
        );
        assert_eq!(keys(v.compile_classpath.project("macos-aarch64")), ["module:util@jvm-3", "g:common:1"]);
        assert_eq!(keys(v.runtime_classpath.project("linux-x86_64")), ["g:b:1", "g:a:1"]);
        assert_eq!(keys(v.runtime_classpath.project("windows-x86_64")), Vec::<String>::new());
        let projected = lock.project("linux-aarch64");
        assert!(!projected.has_system_specific_entries());
        assert_eq!(
            projected.referenced_artifacts().into_iter().collect::<Vec<_>>(),
            ["g:a:1", "g:b:1", "g:common:1", "g:n:1:linux-aarch_64"]
        );
        // Written back exactly as read.
        let again = Lockfile::parse(&lock.to_yaml()).unwrap();
        assert_eq!(again, lock);
        assert!(lock.to_yaml().contains("bySystem:"));
    }

    #[test]
    fn entry_systems_are_normalized() {
        let cp: Classpath =
            serde_yaml_ng::from_str("[{artifact: 'g:a:1', systems: [macos-aarch64, linux-x86_64, macos-aarch64]}]")
                .unwrap();
        let Classpath::List(l) = &cp else { unreachable!() };
        assert_eq!(l[0].systems.as_deref(), Some(&["linux-x86_64".to_string(), "macos-aarch64".to_string()][..]));
        let built = LockEntry::new(ClasspathEntry::Artifact("g:a:1".into()), Some(vec!["b".into(), "a".into()]));
        assert_eq!(built.systems, Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    #[should_panic(expected = "a workspace module is on every system")]
    #[cfg(debug_assertions)]
    fn modules_carry_no_systems() {
        LockEntry::new(ClasspathEntry::Module { module: "a@jvm".into() }, Some(vec!["linux-x86_64".into()]));
    }

    #[test]
    fn version_1_locks_are_converted_keeping_their_hashes() {
        let v1 = "lockfileVersion: 1
resolutionContext: {os: linux, arch: x86_64, jdk: '21'}
bootInputs: sha256:boot
boot:
  scala-compiler@3.9.0: [g:c:1]
variants:
  a@jvm-3:
    specifiers: {}
    inputsHash: sha256:h
    compileClasspath: [{module: b@jvm-3}, g:x:1]
    runtimeClasspath: [g:x:1]
    testCompileClasspath: [g:x:1]
    testRuntimeClasspath: [g:x:1]
    compilerPlugins: [g:p:1]
artifacts:
  g:x:1: {repo: central, sha256: '00', size: 1}
";
        let lock = Lockfile::parse(v1).unwrap();
        assert_eq!(lock.lockfile_version, 2);
        assert_eq!(lock.systems, ["linux-x86_64"]);
        assert_eq!(lock.default_systems, Some(1), "Phase 1's default context");
        assert_eq!((lock.jdk.as_str(), lock.boot_inputs.as_deref()), ("21", Some("sha256:boot")));
        let a = &lock.variants["a@jvm-3"];
        assert_eq!(a.inputs_hash, "sha256:h");
        assert!(!a.full_context && !lock.boot_full_context && lock.native_mappings.is_none());
        assert_eq!(
            a.compile_classpath,
            Classpath::plain([
                ClasspathEntry::Module { module: "b@jvm-3".into() },
                ClasspathEntry::Artifact("g:x:1".into())
            ])
        );
        assert_eq!(a.compiler_plugins, Classpath::plain([ClasspathEntry::Artifact("g:p:1".into())]));
        let other = Lockfile::parse(&v1.replace("os: linux, arch: x86_64", "os: macos, arch: aarch64")).unwrap();
        assert_eq!((other.systems.as_slice(), other.default_systems), (&["macos-aarch64".to_string()][..], None));
        // Other spellings convert to the canonical system; Linux on amd64 was Phase 1's default.
        let amd64 = Lockfile::parse(&v1.replace("os: linux, arch: x86_64", "os: Linux, arch: amd64")).unwrap();
        assert_eq!((amd64.systems.as_slice(), amd64.default_systems), (&["linux-x86_64".to_string()][..], Some(1)));
        let mac = Lockfile::parse(&v1.replace("os: linux, arch: x86_64", "os: Darwin, arch: arm64")).unwrap();
        assert_eq!(mac.systems, ["macos-aarch64"]);
    }

    #[test]
    fn conflicts_and_garbage_are_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE);
        fs::write(&path, "lockfileVersion: 2\n<<<<<<< HEAD\nvariants: {}\n=======\nvariants: {}\n>>>>>>> main\n")
            .unwrap();
        let err = Lockfile::read(dir.path()).unwrap_err();
        assert!(matches!(err, LockError::Conflict(2)) && err.is_corrupt(), "{err}");
        fs::write(&path, "lockfileVersion: [").unwrap();
        assert!(Lockfile::read(dir.path()).unwrap_err().is_corrupt());
        fs::write(&path, "lockfileVersion: 99\nsystems: [linux-x86_64]\njdk: '25'\n").unwrap();
        assert!(!Lockfile::read(dir.path()).unwrap_err().is_corrupt());
        // A newer format with fields this version doesn't know is a version error, not corruption.
        fs::write(&path, "lockfileVersion: 3\nsystems: [linux-x86_64]\njdk: '25'\nnewField: {}\n").unwrap();
        let err = Lockfile::read(dir.path()).unwrap_err();
        assert!(matches!(err, LockError::Version(3)) && !err.is_corrupt(), "{err}");
        // An unknown field in a v2 (or v1) lock is still corruption.
        fs::write(&path, "lockfileVersion: 2\nsystems: [linux-x86_64]\njdk: '25'\nnewField: {}\n").unwrap();
        assert!(Lockfile::read(dir.path()).unwrap_err().is_corrupt());
        fs::write(&path, "lockfileVersion: 1\nresolutionContext: {os: linux, arch: x86_64, jdk: '25'}\nnewField: {}\n")
            .unwrap();
        assert!(Lockfile::read(dir.path()).unwrap_err().is_corrupt());
    }

    #[test]
    fn writes_are_atomic_and_report_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let lock = header();
        assert!(lock.write(dir.path()).unwrap());
        assert!(!lock.write(dir.path()).unwrap(), "unchanged content is not rewritten");
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [LOCK_FILE], "no temporary files are left behind");
        let missing = dir.path().join("missing");
        let err = lock.write(&missing).unwrap_err().to_string();
        assert!(err.starts_with("cannot write") && err.contains("missing"), "{err}");
    }
}
