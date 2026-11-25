//! pnpm-style selectors for `-F/--filter` (design §8), extended with variants.
//!
//! | Selector | Selects |
//! |---|---|
//! | `core` | every variant of `core` (`*` globs allowed) |
//! | `core@jvm-3`, `core@2.13`, `core@*-2.13` | matching variants (selector grammar: design §4.4) |
//! | `core...` / `core^...` | core and its dependencies / only its dependencies |
//! | `...core` / `...^core` | core and its dependents / only its dependents |
//! | `./modules/**`, `{modules/core}`, `.`, `../util` | modules by directory, relative to the invocation directory |
//! | `[origin/main]` | modules changed since the merge base with that ref |
//! | `!sel` | excludes |

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use globset::GlobBuilder;

use crate::graph::Graph;
use crate::selector::VariantSelector;
use crate::workspace::{Variant, Workspace};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FilterError {
    #[error("invalid selector `{0}`: {1}")]
    Invalid(String, String),
    #[error("selector `{0}` matches nothing")]
    NoMatch(String),
    #[error("git: {0}")]
    Git(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Core {
    /// Module name glob, optional variant selector (validated at parse time).
    Name {
        module: String,
        variant: Option<String>,
    },
    /// A directory pattern as written, resolved against the invocation directory.
    Path(String),
    Since(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Selector {
    raw: String,
    exclude: bool,
    core: Core,
    include_self: bool,
    dependencies: bool,
    dependents: bool,
}

fn parse(raw: &str) -> Result<Selector, FilterError> {
    let invalid = |why: &str| FilterError::Invalid(raw.to_string(), why.to_string());
    let (exclude, mut s) = match raw.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let mut include_self = true;
    let mut dependents = false;
    let mut dependencies = false;
    if let Some(rest) = s.strip_prefix("...") {
        dependents = true;
        s = rest;
        if let Some(rest) = s.strip_prefix('^') {
            include_self = false;
            s = rest;
        }
    }
    if let Some(rest) = s.strip_suffix("...") {
        dependencies = true;
        s = rest;
        if let Some(rest) = s.strip_suffix('^') {
            include_self = false;
            s = rest;
        }
    }
    if s.is_empty() {
        return Err(invalid("empty selector"));
    }
    let core = if let Some(r) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        Core::Since(r.to_string())
    } else if let Some(p) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        Core::Path(p.to_string())
    } else if s.starts_with('.') || s.starts_with('/') {
        Core::Path(s.to_string())
    } else {
        let (module, variant) = match s.split_once('@') {
            Some((m, v)) => (m.to_string(), Some(v.to_string())),
            None => (s.to_string(), None),
        };
        if module.is_empty() {
            return Err(invalid("missing module name before `@`"));
        }
        if let Some(v) = &variant {
            VariantSelector::parse_glob(v).map_err(|e| invalid(&e.to_string()))?;
        }
        Core::Name { module, variant }
    };
    Ok(Selector { raw: raw.to_string(), exclude, core, include_self, dependencies, dependents })
}

/// Applies `selectors` and returns matching variant ids. No selectors (or only exclusions)
/// start from everything. Path selectors are relative to `cwd`, the canonical directory scpm
/// was invoked in (the current directory or `-C`).
pub fn select(
    ws: &Workspace,
    graph: &Graph,
    selectors: &[String],
    cwd: &Path,
) -> Result<BTreeSet<String>, FilterError> {
    let parsed: Vec<Selector> = selectors.iter().map(|s| parse(s)).collect::<Result<_, _>>()?;
    let all: BTreeSet<String> = ws.variants().map(|v| v.id.clone()).collect();
    let (excludes, includes): (Vec<_>, Vec<_>) = parsed.into_iter().partition(|s| s.exclude);
    let mut selected = if includes.is_empty() { all } else { BTreeSet::new() };
    for sel in &includes {
        let set = expand(ws, graph, sel, cwd)?;
        selected.extend(set);
    }
    for sel in &excludes {
        let set = expand(ws, graph, sel, cwd)?;
        selected.retain(|id| !set.contains(id));
    }
    Ok(selected)
}

fn expand(ws: &Workspace, graph: &Graph, sel: &Selector, cwd: &Path) -> Result<BTreeSet<String>, FilterError> {
    let base = match_core(ws, &sel.core, &sel.raw, cwd)?;
    if base.is_empty() && !matches!(sel.core, Core::Since(_)) {
        return Err(FilterError::NoMatch(sel.raw.clone()));
    }
    let mut out = BTreeSet::new();
    if sel.include_self {
        out.extend(base.iter().cloned());
    }
    if sel.dependencies {
        out.extend(graph.closure(&base, true));
    }
    if sel.dependents {
        out.extend(graph.closure(&base, false));
    }
    Ok(out)
}

fn match_core(ws: &Workspace, core: &Core, raw: &str, cwd: &Path) -> Result<BTreeSet<String>, FilterError> {
    let invalid = |why: String| FilterError::Invalid(raw.to_string(), why);
    let glob = |pattern: &str| {
        GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map(|g| g.compile_matcher())
            .map_err(|e| invalid(e.to_string()))
    };
    let ids = |pred: &dyn Fn(&Variant, &str) -> bool| -> BTreeSet<String> {
        ws.modules.iter().flat_map(|m| m.variants.iter().filter(|v| pred(v, &m.dir)).map(|v| v.id.clone())).collect()
    };
    match core {
        Core::Name { module, variant } => {
            let module_glob = glob(module)?;
            let selector =
                variant.as_deref().map(VariantSelector::parse_glob).transpose().map_err(|e| invalid(e.to_string()))?;
            Ok(ids(&|v, _| {
                module_glob.is_match(&v.module)
                    && selector.as_ref().is_none_or(|s| s.matches(v.platform, v.scala.as_ref()))
            }))
        }
        Core::Path(pattern) => {
            let Some(relative) = workspace_relative(&ws.root, cwd, pattern) else {
                return Err(invalid(format!("path is outside the workspace ({})", ws.root.display())));
            };
            let path_glob = glob(&relative)?;
            Ok(ids(&|_, dir| path_glob.is_match(dir)))
        }
        Core::Since(git_ref) => {
            let changed = changed_files(&ws.root, git_ref)?;
            let workspace_changed = ws
                .workspace_file
                .as_ref()
                .and_then(|f| f.strip_prefix(&ws.root).ok())
                .is_some_and(|f| changed.iter().any(|c| Path::new(c) == f));
            // Each changed file belongs to the module with the longest matching directory.
            let owners: BTreeSet<&str> = changed
                .iter()
                .filter_map(|f| {
                    ws.modules
                        .iter()
                        .filter(|m| m.dir == "." || Path::new(f).starts_with(&m.dir))
                        .max_by_key(|m| if m.dir == "." { 0 } else { m.dir.len() })
                        .map(|m| m.dir.as_str())
                })
                .collect();
            Ok(ids(&|_, dir| workspace_changed || owners.contains(dir)))
        }
    }
}

/// Resolves a path pattern against `cwd` and makes it relative to `root`, `/`-separated (`.`
/// for the root itself). Glob components are kept as written. `None` if it leaves the root.
fn workspace_relative(root: &Path, cwd: &Path, pattern: &str) -> Option<String> {
    let has_glob = |s: &str| s.contains(['*', '?', '[', ']', '{', '}']);
    let joined = cwd.join(pattern);
    // Canonicalize the literal prefix (so `/var/…` and `/private/var/…` agree on macOS, and
    // symlinked paths resolve), then append the glob remainder.
    let mut prefix = PathBuf::new();
    let mut rest: Vec<String> = Vec::new();
    for component in normalize(&joined).components() {
        let text = component.as_os_str().to_string_lossy();
        if rest.is_empty() && !has_glob(&text) {
            prefix.push(component);
        } else {
            rest.push(text.into_owned());
        }
    }
    let prefix = prefix.canonicalize().unwrap_or(prefix);
    let relative = prefix.strip_prefix(root).ok()?;
    let mut parts: Vec<String> = relative.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    parts.extend(rest);
    Some(if parts.is_empty() { ".".to_string() } else { parts.join("/") })
}

/// Lexically removes `.` and `..` components.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Files changed since the merge base of `git_ref` and HEAD, including uncommitted and
/// untracked (non-ignored) files. Paths are relative to `root`. Renames count as a deletion
/// of the old path and an addition of the new one, so both owners are affected.
fn changed_files(root: &Path, git_ref: &str) -> Result<Vec<String>, FilterError> {
    let git = |args: &[&str]| -> Result<String, FilterError> {
        let out = Command::new("git")
            .args(["-c", "core.quotePath=false"])
            .args(args)
            .current_dir(root)
            .output()
            .map_err(|e| FilterError::Git(e.to_string()))?;
        if !out.status.success() {
            return Err(FilterError::Git(String::from_utf8_lossy(&out.stderr).trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let base = git(&["merge-base", git_ref, "HEAD"])?;
    let nul_separated =
        |s: String| -> Vec<String> { s.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect() };
    let mut files = nul_separated(git(&["diff", "--name-only", "-z", "--no-renames", "--relative", base.trim()])?);
    files.extend(nul_separated(git(&["ls-files", "--others", "--exclude-standard", "-z"])?));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forms() {
        let s = parse("core...").unwrap();
        assert!(s.dependencies && s.include_self && !s.dependents);
        let s = parse("...^core").unwrap();
        assert!(s.dependents && !s.include_self);
        let s = parse("!core@jvm-*").unwrap();
        assert!(s.exclude);
        assert_eq!(s.core, Core::Name { module: "core".into(), variant: Some("jvm-*".into()) });
        assert_eq!(parse("[origin/main]").unwrap().core, Core::Since("origin/main".into()));
        assert_eq!(parse("./modules/**").unwrap().core, Core::Path("./modules/**".into()));
        assert_eq!(parse("{modules/core}").unwrap().core, Core::Path("modules/core".into()));
        assert!(parse("...").is_err());
        assert!(matches!(parse("core@foo"), Err(FilterError::Invalid(..))));
        assert!(matches!(parse("core@jvm-x"), Err(FilterError::Invalid(..))));
        for ok in ["core@3", "core@js", "core@2.13", "core@3.9.0", "core@jvm-3", "core@3.8.0-RC1", "*@*-2.13"] {
            assert!(parse(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn resolves_paths_against_the_invocation_directory() {
        let root = Path::new("/ws");
        let cwd = Path::new("/ws/modules/core");
        assert_eq!(workspace_relative(root, cwd, ".").as_deref(), Some("modules/core"));
        assert_eq!(workspace_relative(root, cwd, "./").as_deref(), Some("modules/core"));
        assert_eq!(workspace_relative(root, cwd, "../util").as_deref(), Some("modules/util"));
        assert_eq!(workspace_relative(root, cwd, "../*").as_deref(), Some("modules/*"));
        assert_eq!(workspace_relative(root, cwd, "/ws/apps/**").as_deref(), Some("apps/**"));
        assert_eq!(workspace_relative(root, root, ".").as_deref(), Some("."));
        assert_eq!(workspace_relative(root, cwd, "../../..").as_deref(), None);
    }
}
