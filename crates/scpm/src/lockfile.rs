//! `scpm-lock.yaml` (design §5.4).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const LOCK_FILE: &str = "scpm-lock.yaml";
pub const LOCKFILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lockfile {
    pub lockfile_version: u32,
    pub resolution_context: ResolutionContextLock,
    /// sha256 over what boot resolutions depend on (repositories, resolution context); when it
    /// changes every boot entry is re-resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_inputs: Option<String>,
    /// Closures needed before a JVM starts: compiler, bridge, worker, tools.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub boot: BTreeMap<String, Vec<String>>,
    /// Selections for catalog ranges, keyed `<catalog>/<key>@<platform>-<scala binary>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub catalogs: BTreeMap<String, CatalogSelection>,
    #[serde(default)]
    pub variants: BTreeMap<String, VariantLock>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactLock>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolutionContextLock {
    pub os: String,
    pub arch: String,
    pub jdk: String,
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
    pub specifiers: BTreeMap<String, Specifier>,
    pub inputs_hash: String,
    pub compile_classpath: Vec<ClasspathEntry>,
    pub runtime_classpath: Vec<ClasspathEntry>,
    pub test_compile_classpath: Vec<ClasspathEntry>,
    pub test_runtime_classpath: Vec<ClasspathEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compiler_plugins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Specifier {
    pub spec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// An artifact key, or a reference to an upstream workspace variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClasspathEntry {
    Artifact(String),
    Module { module: String },
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
    #[error("{LOCK_FILE} has lockfileVersion {0}; this scpm understands version {LOCKFILE_VERSION}")]
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

impl Lockfile {
    /// Reads the lockfile at `root`, or `None` if there is none.
    pub fn read(root: &Path) -> Result<Option<Lockfile>, LockError> {
        let path = root.join(LOCK_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
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
        if let Ok(Header { lockfile_version: Some(version) }) = serde_yaml_ng::from_str::<Header>(&text)
            && version != LOCKFILE_VERSION
        {
            return Err(LockError::Version(version));
        }
        let lock: Lockfile = serde_yaml_ng::from_str(&text).map_err(|e| LockError::Parse(e.to_string()))?;
        if lock.lockfile_version != LOCKFILE_VERSION {
            return Err(LockError::Version(lock.lockfile_version));
        }
        Ok(Some(lock))
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

    /// Every artifact key referenced by a variant or the boot section.
    pub fn referenced_artifacts(&self) -> BTreeSet<&str> {
        let mut keys: BTreeSet<&str> = self.boot.values().flatten().map(String::as_str).collect();
        for v in self.variants.values() {
            for list in
                [&v.compile_classpath, &v.runtime_classpath, &v.test_compile_classpath, &v.test_runtime_classpath]
            {
                keys.extend(list.iter().filter_map(|e| match e {
                    ClasspathEntry::Artifact(k) => Some(k.as_str()),
                    ClasspathEntry::Module { .. } => None,
                }));
            }
            keys.extend(v.compiler_plugins.iter().map(String::as_str));
        }
        keys
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

    #[test]
    fn round_trips_through_yaml() {
        let mut lock = Lockfile { lockfile_version: 1, ..Default::default() };
        lock.variants.insert(
            "core@jvm-3".into(),
            VariantLock {
                specifiers: [("util".to_string(), Specifier { spec: "workspace:*".into(), version: None })].into(),
                inputs_hash: "sha256:abc".into(),
                compile_classpath: vec![
                    ClasspathEntry::Module { module: "util@jvm-3".into() },
                    ClasspathEntry::Artifact("org.typelevel:cats-core_3:2.13.0".into()),
                ],
                ..Default::default()
            },
        );
        lock.artifacts.insert(
            "org.typelevel:cats-core_3:2.13.0".into(),
            ArtifactLock { repo: "central".into(), sha256: "00".into(), size: 1 },
        );
        let yaml = lock.to_yaml();
        assert!(yaml.contains("- module: util@jvm-3"), "{yaml}");
        let back: Lockfile = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back, lock);
        assert_eq!(
            back.referenced_artifacts().into_iter().collect::<Vec<_>>(),
            vec!["org.typelevel:cats-core_3:2.13.0"]
        );
    }

    #[test]
    fn conflicts_and_garbage_are_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE);
        fs::write(&path, "lockfileVersion: 1\n<<<<<<< HEAD\nvariants: {}\n=======\nvariants: {}\n>>>>>>> main\n")
            .unwrap();
        let err = Lockfile::read(dir.path()).unwrap_err();
        assert!(matches!(err, LockError::Conflict(2)) && err.is_corrupt(), "{err}");
        fs::write(&path, "lockfileVersion: [").unwrap();
        assert!(Lockfile::read(dir.path()).unwrap_err().is_corrupt());
        fs::write(&path, "lockfileVersion: 99\nresolutionContext: {os: linux, arch: x86_64, jdk: '25'}\n").unwrap();
        assert!(!Lockfile::read(dir.path()).unwrap_err().is_corrupt());
        // A newer format with fields this version doesn't know is a version error, not corruption.
        fs::write(&path, "lockfileVersion: 2\nresolutionContext: {os: linux, arch: x86_64, jdk: '25'}\nnewField: {}\n")
            .unwrap();
        let err = Lockfile::read(dir.path()).unwrap_err();
        assert!(matches!(err, LockError::Version(2)) && !err.is_corrupt(), "{err}");
        // An unknown field in a v1 lock is still corruption.
        fs::write(&path, "lockfileVersion: 1\nresolutionContext: {os: linux, arch: x86_64, jdk: '25'}\nnewField: {}\n")
            .unwrap();
        assert!(Lockfile::read(dir.path()).unwrap_err().is_corrupt());
    }

    #[test]
    fn writes_are_atomic_and_report_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let lock = Lockfile { lockfile_version: 1, ..Default::default() };
        assert!(lock.write(dir.path()).unwrap());
        assert!(!lock.write(dir.path()).unwrap(), "unchanged content is not rewritten");
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [LOCK_FILE], "no temporary files are left behind");
        let missing = dir.path().join("missing");
        let err = lock.write(&missing).unwrap_err().to_string();
        assert!(err.starts_with("cannot write") && err.contains("missing"), "{err}");
    }
}
