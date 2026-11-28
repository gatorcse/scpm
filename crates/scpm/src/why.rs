//! `scpm why`: explain how an artifact ends up on a variant's classpath.
//!
//! The lock records classpaths, not the dependency graph (which would depend on the order
//! variants were locked in), so `why` asks coursier: it rebuilds the variant's widest
//! resolution request (the test runtime classpath) at the locked versions and runs
//! `cs resolve --what-depends-on` (design §5.4).

use std::collections::BTreeSet;

use serde::Serialize;

use crate::coursier;
use crate::graph::Graph;
use crate::install::{ClasspathKind, InstallError, classpath_request, specifier_key, upstream_variants};
use crate::lockfile::{ClasspathEntry, Lockfile};
use crate::version::VersionSpec;
use crate::workspace::{Dependency, Variant, Workspace};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WhyPath {
    pub variant: String,
    /// The workspace variant whose direct dependency starts the chain (itself or upstream).
    pub declared_by: String,
    /// Coordinates from the direct dependency to the match, as coursier reports them.
    pub chain: Vec<String>,
}

/// A variant whose classpath holds a matching artifact that coursier found no chain to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Unexplained {
    pub variant: String,
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct WhyReport {
    pub paths: Vec<WhyPath>,
    pub unexplained: Vec<Unexplained>,
    /// Settings of other tools that touch the repositories and that scpm ignored (design §5.9).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
    /// Problems with the machine settings (design §5.9), e.g. a user config others can read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Does `key` (`g:a:v[...]`) match `pattern` (`org:name`, `org::name`, or an artifact name)?
fn matches(key: &str, pattern: &str) -> bool {
    let mut parts = key.split(':');
    let (Some(group), Some(artifact)) = (parts.next(), parts.next()) else { return false };
    let name_matches = |name: &str| artifact == name || artifact.starts_with(&format!("{name}_"));
    match pattern.split_once(':') {
        Some((org, name)) => group == org && name_matches(name.trim_start_matches(':')),
        None => name_matches(pattern),
    }
}

fn artifact_name(variant: &Variant, dep: &Dependency) -> String {
    match &variant.scala {
        Some(s) => dep.coord.artifact_name(&s.full, variant.platform),
        None => dep.coord.name.clone(),
    }
}

pub fn why(
    ws: &Workspace,
    graph: &Graph,
    lock: &Lockfile,
    pattern: &str,
    selected: &[String],
) -> Result<WhyReport, InstallError> {
    let (repos, machine) = crate::install::repositories(ws)?;
    let cache = coursier::cache_dir();
    let mut report = WhyReport { warnings: machine.warnings.clone(), ..Default::default() };
    let mut ran_cs = false;
    for id in selected {
        let (Some(variant), Some(vlock)) = (ws.variant(id), lock.variants.get(id)) else { continue };
        let found: Vec<&str> = ClasspathKind::TestRuntime
            .entries(vlock)
            .iter()
            .filter_map(|e| match e {
                ClasspathEntry::Artifact(k) if matches(k, pattern) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        if found.is_empty() {
            continue;
        }
        // The variant's widest request, at the versions the lock selected.
        let mut locked_version = |owner: &Variant, dep: &Dependency| -> Result<String, InstallError> {
            let locked = lock
                .variants
                .get(&owner.id)
                .and_then(|l| l.specifiers.get(&specifier_key(owner, dep)))
                .and_then(|s| s.version.clone());
            match (locked, &dep.version) {
                (Some(v), _) => Ok(v),
                (None, VersionSpec::Exact { version }) => Ok(version.clone()),
                (None, _) => Err(InstallError::Stale(vec![owner.id.clone()])),
            }
        };
        let req = classpath_request(ws, graph, variant, ClasspathKind::TestRuntime, &lock.jdk, &mut locked_version)?;
        let modules: BTreeSet<String> =
            found.iter().map(|k| k.split(':').take(2).collect::<Vec<_>>().join(":")).collect();
        let mut owners = vec![id.clone()];
        owners.extend(upstream_variants(graph, id, ClasspathKind::TestRuntime));
        let mut unexplained: Vec<String> = Vec::new();
        for module in &modules {
            let chains = coursier::what_depends_on(&req, module, &repos, &machine.proxy, &cache)
                .map_err(|e| crate::install::with_notice(e.into(), &repos, true))?;
            ran_cs = true;
            if chains.is_empty() {
                unexplained.extend(found.iter().filter(|k| module_of(k) == *module).map(|k| k.to_string()));
            }
            for chain in chains {
                let declared_by = declared_by(ws, id, &owners, &module_of(&chain[0]));
                let path = WhyPath { variant: id.clone(), declared_by, chain };
                if !report.paths.contains(&path) {
                    report.paths.push(path);
                }
            }
        }
        if !unexplained.is_empty() {
            report.unexplained.push(Unexplained { variant: id.clone(), artifacts: unexplained });
        }
    }
    if ran_cs {
        report.notices.extend(crate::foreign::take_notice(&repos, true));
    }
    Ok(report)
}

/// The first of `owners` (the variant `id`, then its upstream variants) whose declaration of
/// `root` (`org:name`) is part of `id`'s test runtime request: the variant's own in every scope,
/// an upstream variant's only in the scopes it passes downstream (never provided or test).
fn declared_by(ws: &Workspace, id: &str, owners: &[String], root: &str) -> String {
    owners
        .iter()
        .find(|o| {
            let scopes = if o.as_str() == id {
                ClasspathKind::TestRuntime.own_scopes()
            } else {
                ClasspathKind::TestRuntime.upstream_scopes()
            };
            ws.variant(o).is_some_and(|ov| {
                ov.dependencies
                    .iter()
                    .any(|d| scopes.contains(&d.scope) && format!("{}:{}", d.coord.org, artifact_name(ov, d)) == root)
            })
        })
        .map_or(id, String::as_str)
        .to_string()
}

/// `org:name` of a key or coordinate (`org:name:version…`).
fn module_of(key: &str) -> String {
    key.split(':').take(2).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_follows_the_scopes_on_the_classpath() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("scpm-workspace.yaml"), "modules: ['modules/*']\ntoolchain:\n  scala: 3.9.0\n")
            .unwrap();
        for (name, manifest) in [
            ("a", "providedDependencies:\n  g:x: '1.0'\ntestDependencies:\n  g:y: '1.0'\n"),
            ("b", "dependencies:\n  g:x: '1.0'\n  g:y: '1.0'\n"),
            ("c", "dependencies:\n  a: workspace:*\n  b: workspace:*\n"),
        ] {
            std::fs::create_dir_all(dir.path().join("modules").join(name)).unwrap();
            std::fs::write(dir.path().join("modules").join(name).join("scpm.yaml"), manifest).unwrap();
        }
        let ws = crate::workspace::load(dir.path()).unwrap();
        let owners = ["c@jvm-3".to_string(), "a@jvm-3".into(), "b@jvm-3".into()];
        // a's provided and test dependencies never reach c's classpath; b's do.
        assert_eq!(declared_by(&ws, "c@jvm-3", &owners, "g:x"), "b@jvm-3");
        assert_eq!(declared_by(&ws, "c@jvm-3", &owners, "g:y"), "b@jvm-3");
        // A variant's own provided and test dependencies are on its own test classpath.
        assert_eq!(declared_by(&ws, "a@jvm-3", &["a@jvm-3".to_string()], "g:y"), "a@jvm-3");
    }

    #[test]
    fn pattern_matching() {
        let key = "org.typelevel:cats-kernel_3:2.13.0";
        assert!(matches(key, "org.typelevel::cats-kernel"));
        assert!(matches(key, "org.typelevel:cats-kernel_3"));
        assert!(matches(key, "cats-kernel"));
        assert!(!matches(key, "cats"));
        assert!(!matches(key, "org.other::cats-kernel"));
    }
}
