//! The variant graph: edges from `workspace:*` dependencies, matched by platform and Scala
//! binary version (design §4.5).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

use crate::diag::{Diagnostic, Diagnostics};
use crate::platform::Platform;
use crate::workspace::{Scope, Variant, Workspace, module_file};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub scope: Scope,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Graph {
    /// Variant ids in dependency order: every variant appears after the variants it depends
    /// on (ignoring test-scope edges, which may point "backwards").
    pub order: Vec<String>,
    pub edges: Vec<Edge>,
}

impl Graph {
    pub fn build(ws: &Workspace) -> Result<Graph, Diagnostics> {
        let mut diags = Diagnostics::default();
        let mut edges = BTreeSet::new();
        for module in &ws.modules {
            for variant in &module.variants {
                for dep in &variant.workspace_dependencies {
                    let Some(target) = ws.module(&dep.module) else { continue };
                    match match_variant(variant, &target.variants) {
                        Some(to) => {
                            edges.insert(Edge { from: variant.id.clone(), to: to.id.clone(), scope: dep.scope });
                        }
                        None => {
                            let available: Vec<_> = target.variants.iter().map(|v| v.id.as_str()).collect();
                            diags.push(
                                Diagnostic::new(format!(
                                    "{} depends on `{}`, which has no matching variant",
                                    variant.id, dep.module
                                ))
                                .in_file(module_file(module))
                                .at_key(format!("{}.{}", dep.scope.key(), dep.module))
                                .with_help(format!("available: {}", available.join(", "))),
                            );
                        }
                    }
                }
            }
        }
        let edges: Vec<Edge> = edges.into_iter().collect();
        let ids: Vec<String> = ws.variants().map(|v| v.id.clone()).collect();
        let order = match topo_order(&ids, &edges) {
            Ok(order) => order,
            Err(cycle) => {
                let mut d = Diagnostic::new(format!("dependency cycle: {}", cycle.join(" → ")))
                    .with_help("test-scope dependencies may form cycles; compile and runtime ones may not");
                // Point at the file of the first module in the cycle.
                if let Some(module) = ws.modules.iter().find(|m| m.variants.iter().any(|v| v.id == cycle[0])) {
                    d = d.in_file(module_file(module));
                }
                diags.push(d);
                Vec::new()
            }
        };
        diags.dedup();
        diags.into_result(Graph { order, edges })
    }

    /// Direct dependencies of `id` (all scopes).
    pub fn dependencies_of<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a Edge> + 'a {
        self.edges.iter().filter(move |e| e.from == id)
    }

    /// Transitive closure following edges forward (`dependencies`) or backward (`dependents`).
    pub fn closure(&self, start: &BTreeSet<String>, forward: bool) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<&str> = start.iter().map(String::as_str).collect();
        while let Some(id) = queue.pop_front() {
            for e in &self.edges {
                let (src, dst) = if forward { (&e.from, &e.to) } else { (&e.to, &e.from) };
                if src == id && seen.insert(dst.clone()) {
                    queue.push_back(dst);
                }
            }
        }
        seen
    }
}

/// The variant of a dependency that a variant uses: same platform and Scala binary version.
/// A Java-only module (one `@jvm` variant) serves every JVM variant.
fn match_variant<'a>(from: &Variant, candidates: &'a [Variant]) -> Option<&'a Variant> {
    candidates.iter().find(|c| {
        c.platform == from.platform
            && match (&c.scala, &from.scala) {
                (None, _) => c.platform == Platform::Jvm,
                (Some(a), Some(b)) => a.binary == b.binary,
                (Some(_), None) => false,
            }
    })
}

/// Kahn's algorithm over non-test edges, ties broken by id for stable output.
fn topo_order(ids: &[String], edges: &[Edge]) -> Result<Vec<String>, Vec<String>> {
    let structural: Vec<&Edge> = edges.iter().filter(|e| e.scope != Scope::Test).collect();
    let mut indegree: BTreeMap<&str, usize> = ids.iter().map(|id| (id.as_str(), 0)).collect();
    for e in &structural {
        *indegree.get_mut(e.from.as_str()).expect("edge source is a variant") += 1;
    }
    let mut ready: BTreeSet<&str> = indegree.iter().filter(|(_, d)| **d == 0).map(|(id, _)| *id).collect();
    let mut order = Vec::with_capacity(ids.len());
    while let Some(id) = ready.pop_first() {
        order.push(id.to_string());
        for e in structural.iter().filter(|e| e.to == id) {
            let d = indegree.get_mut(e.from.as_str()).expect("variant");
            *d -= 1;
            if *d == 0 {
                ready.insert(&e.from);
            }
        }
    }
    if order.len() == ids.len() {
        return Ok(order);
    }
    // Report one cycle among the remaining nodes.
    let remaining: BTreeSet<&str> = indegree.iter().filter(|(_, d)| **d > 0).map(|(id, _)| *id).collect();
    let start = *remaining.first().expect("non-empty when a cycle exists");
    let mut path = vec![start.to_string()];
    let mut current = start;
    loop {
        let next = structural
            .iter()
            .find(|e| e.from == current && remaining.contains(e.to.as_str()))
            .map(|e| e.to.as_str())
            .expect("every remaining node has a remaining dependency");
        if let Some(pos) = path.iter().position(|p| p == next) {
            let mut cycle = path.split_off(pos);
            cycle.push(next.to_string());
            return Err(cycle);
        }
        path.push(next.to_string());
        current = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: &str, to: &str, scope: Scope) -> Edge {
        Edge { from: from.into(), to: to.into(), scope }
    }

    #[test]
    fn orders_dependencies_first() {
        let ids = vec!["app".to_string(), "core".to_string(), "util".to_string()];
        let edges = vec![edge("app", "core", Scope::Compile), edge("core", "util", Scope::Compile)];
        assert_eq!(topo_order(&ids, &edges).unwrap(), vec!["util", "core", "app"]);
    }

    #[test]
    fn test_edges_may_cycle() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let edges = vec![edge("a", "b", Scope::Compile), edge("b", "a", Scope::Test)];
        assert_eq!(topo_order(&ids, &edges).unwrap(), vec!["b", "a"]);
    }

    #[test]
    fn reports_cycles() {
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let edges =
            vec![edge("a", "b", Scope::Compile), edge("b", "c", Scope::Runtime), edge("c", "a", Scope::Provided)];
        assert_eq!(topo_order(&ids, &edges).unwrap_err(), vec!["a", "b", "c", "a"]);
    }
}
