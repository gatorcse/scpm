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
use crate::install::{ClasspathKind, InstallError, classpath_request, upstream_variants};
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
    let repos = coursier::repositories(&ws.repositories())?;
    let cache = coursier::cache_dir();
    let mut report = WhyReport::default();
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
                .and_then(|l| l.specifiers.get(&dep.coord.to_string()))
                .and_then(|s| s.version.clone());
            match (locked, &dep.version) {
                (Some(v), _) => Ok(v),
                (None, VersionSpec::Exact { version }) => Ok(version.clone()),
                (None, _) => Err(InstallError::Stale(vec![owner.id.clone()])),
            }
        };
        let req = classpath_request(
            ws,
            graph,
            variant,
            ClasspathKind::TestRuntime,
            &lock.resolution_context.jdk,
            &mut locked_version,
        )?;
        let modules: BTreeSet<String> =
            found.iter().map(|k| k.split(':').take(2).collect::<Vec<_>>().join(":")).collect();
        let mut owners = vec![id.clone()];
        owners.extend(upstream_variants(graph, id, ClasspathKind::TestRuntime));
        let mut explained = false;
        for module in &modules {
            for chain in coursier::what_depends_on(&req, module, &repos, &cache)? {
                explained = true;
                let root = chain[0].split(':').take(2).collect::<Vec<_>>().join(":");
                let declared_by = owners
                    .iter()
                    .find(|o| {
                        ws.variant(o).is_some_and(|ov| {
                            ov.dependencies.iter().any(|d| format!("{}:{}", d.coord.org, artifact_name(ov, d)) == root)
                        })
                    })
                    .unwrap_or(id)
                    .clone();
                let path = WhyPath { variant: id.clone(), declared_by, chain };
                if !report.paths.contains(&path) {
                    report.paths.push(path);
                }
            }
        }
        if !explained {
            report
                .unexplained
                .push(Unexplained { variant: id.clone(), artifacts: found.iter().map(|k| k.to_string()).collect() });
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

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
