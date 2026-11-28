//! `scpm init`: starts a workspace, or adds a module to the workspace around the directory.
//!
//! Outside a workspace it writes a workspace that builds, tests and runs as it is: a
//! `scpm-workspace.yaml` with the Scala version and munit in the catalog, and one app module in
//! `modules/<name>` with a main class and a test. Inside one it adds a library module at the
//! directory, which the workspace's `modules:` globs must match. It never overwrites a file and
//! needs no network: the first `scpm install` (or `run`, `test`, …) resolves.
//!
//! What it wrote is loaded back as a workspace before it returns. If that fails, everything it
//! created is removed again and the error says why.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{MODULE_FILE, WORKSPACE_FILE};
use crate::diag::Diagnostics;
use crate::graph::Graph;
use crate::workspace::{self, ScalaVersion};

/// The Scala version and munit version a new workspace starts with: the ones scpm is tested with.
pub const DEFAULT_SCALA: &str = "3.9.0";
pub const DEFAULT_MUNIT: &str = "1.2.1";
const MUNIT: &str = "org.scalameta::munit";
const DEFAULT_APP: &str = "app";

#[derive(Debug, Default)]
pub struct InitOptions {
    /// The module's name; default `app` for a new workspace, else the directory's name.
    pub name: Option<String>,
    /// A full Scala version: the workspace's (new workspace) or the module's own.
    pub scala: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitReport {
    /// `workspace` (a new workspace) or `module` (a module added to an existing one).
    pub kind: &'static str,
    /// The workspace root.
    #[serde(serialize_with = "crate::diag::ser_path")]
    pub root: PathBuf,
    pub module: String,
    /// The module's variant ids, e.g. `app@jvm-3`.
    pub variants: Vec<String>,
    /// Files created, relative to `root`, in the order written.
    pub created: Vec<String>,
    /// Files changed (only `.gitignore`), relative to `root`.
    pub updated: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// Bad arguments, or a directory that can't be initialized.
    #[error("{0}")]
    Usage(String),
    #[error("{0}")]
    Io(String),
    /// The files were written but don't load as a workspace; they have been removed again.
    #[error("the new files do not load as a workspace, so they were removed again")]
    Invalid(Diagnostics),
    /// The workspace a module was to be added to does not load as it is.
    #[error("the workspace does not load, so no module was added")]
    Workspace(Diagnostics),
}

/// Initializes `dir` (created if missing).
pub fn init(dir: &Path, opts: &InitOptions) -> Result<InitReport, InitError> {
    if dir.is_file() {
        return Err(InitError::Usage(format!("{} is a file, not a directory", dir.display())));
    }
    if let Some(v) = &opts.scala {
        check_scala(v)?;
    }
    let mut w = Writer::default();
    let result = init_in(dir, opts, &mut w);
    if result.is_err() {
        w.undo();
    }
    result
}

fn init_in(dir: &Path, opts: &InitOptions, w: &mut Writer) -> Result<InitReport, InitError> {
    w.create_dir_all(dir)?;
    let dir = dir.canonicalize().map_err(|e| io_error(dir, e))?;
    for (file, what) in [(WORKSPACE_FILE, "a workspace"), (MODULE_FILE, "a module")] {
        if dir.join(file).exists() {
            return Err(InitError::Usage(format!("{} is already {what}: it has an {file}", dir.display())));
        }
    }
    let enclosing = dir.ancestors().skip(1).find(|a| a.join(WORKSPACE_FILE).is_file()).map(Path::to_path_buf);
    let (kind, root, module) = match enclosing {
        None => ("workspace", dir.clone(), new_workspace(&dir, opts, w)?),
        Some(root) => ("module", root.clone(), new_module(&root, &dir, opts, w)?),
    };

    let ws = workspace::load(&root).map_err(InitError::Invalid)?;
    Graph::build(&ws).map_err(InitError::Invalid)?;
    let Some(m) = ws.modules.iter().find(|m| m.name == module) else {
        let rel = rel(&root, &dir);
        return Err(InitError::Usage(format!(
            "{rel} is not one of the workspace's modules: no pattern in `modules:` of {} matches it\n  \
             help: add `{rel}` (or a glob matching it) to `modules:`, then run `scpm init` again",
            root.join(WORKSPACE_FILE).display()
        )));
    };
    Ok(InitReport {
        kind,
        root: root.clone(),
        module: m.name.clone(),
        variants: m.variants.iter().map(|v| v.id.clone()).collect(),
        created: w.created.iter().filter(|p| p.is_file()).map(|p| rel(&root, p)).collect(),
        updated: w.appended.iter().map(|p| rel(&root, p)).collect(),
    })
}

/// A new workspace at `root` with one app module; returns the module's name.
fn new_workspace(root: &Path, opts: &InitOptions, w: &mut Writer) -> Result<String, InitError> {
    let name = opts.name.clone().unwrap_or_else(|| DEFAULT_APP.to_string());
    check_name(&name, None)?;
    let scala = opts.scala.as_deref().unwrap_or(DEFAULT_SCALA);
    let module_dir = root.join("modules").join(&name);
    // Checked up front so nothing is written when the module's files are in the way.
    for file in [WORKSPACE_FILE, MODULE_FILE] {
        let path = if file == MODULE_FILE { module_dir.join(file) } else { root.join(file) };
        refuse_existing(&path)?;
    }
    w.write(
        &root.join(WORKSPACE_FILE),
        &format!(
            "# The workspace: settings shared by every module. Modules are the directories with an\n\
             # scpm.yaml that `modules:` matches.\n\
             modules:\n  - modules/*\n\n\
             toolchain:\n  scala: catalog          # every module's Scala version, from the catalog\n\n\
             # Versions, kept in one place. A module refers to one as `catalog`.\n\
             catalog:\n  scala: {scala}\n  {MUNIT}: {DEFAULT_MUNIT}\n"
        ),
    )?;
    let pkg = package_name(&name);
    w.write(
        &module_dir.join(MODULE_FILE),
        &format!(
            "mainClass: {pkg}.Main\n\n\
             # dependencies:\n\
             #   org.typelevel::cats-core: 2.13.0   # `::` adds the Scala suffix (cats-core_3)\n\
             #   util: workspace:*                  # another module of this workspace\n\n\
             testDependencies:\n  {MUNIT}: catalog\n"
        ),
    )?;
    write_sources(&module_dir, &pkg, "Main", true, true, w)?;
    gitignore(root, w)?;
    Ok(name)
}

/// A library module at `dir` in the workspace at `root`; returns the module's name.
fn new_module(root: &Path, dir: &Path, opts: &InitOptions, w: &mut Writer) -> Result<String, InitError> {
    let dir_name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = opts.name.clone().unwrap_or_else(|| dir_name.clone());
    check_name(&name, opts.name.is_none().then_some(dir))?;
    let ws = workspace::load(root).map_err(InitError::Workspace)?;
    if let Some(m) = ws.modules.iter().find(|m| m.name == name) {
        return Err(InitError::Usage(format!(
            "the workspace already has a module named `{name}`, in {}\n  help: pass --name to choose another name",
            m.dir
        )));
    }
    let has_munit = ws.config.catalog.contains_key(MUNIT);
    let mut yaml = String::new();
    if name != dir_name {
        yaml.push_str(&format!("name: {name}\n"));
    }
    if let Some(v) = &opts.scala {
        yaml.push_str(&format!("scala: {v}\n"));
    }
    if !yaml.is_empty() {
        yaml.push('\n');
    }
    yaml.push_str(
        "# dependencies:\n\
         #   org.typelevel::cats-core: 2.13.0   # `::` adds the Scala suffix (cats-core_3)\n\
         #   util: workspace:*                  # another module of this workspace\n",
    );
    if has_munit {
        yaml.push_str(&format!("\ntestDependencies:\n  {MUNIT}: catalog\n"));
    }
    w.write(&dir.join(MODULE_FILE), &yaml)?;
    let pkg = package_name(&name);
    write_sources(dir, &pkg, &object_name(&name), false, has_munit, w)?;
    Ok(name)
}

/// One object (an app's `Main`, or a library's object) and, with munit, a suite for it. The
/// code is valid Scala 2.13 and Scala 3.
fn write_sources(
    module_dir: &Path,
    pkg: &str,
    object: &str,
    app: bool,
    test: bool,
    w: &mut Writer,
) -> Result<(), InitError> {
    let pkg_dir = pkg.trim_matches('`');
    let main = module_dir.join("src/main/scala").join(pkg_dir).join(format!("{object}.scala"));
    let suite = module_dir.join("src/test/scala").join(pkg_dir).join(format!("{object}Suite.scala"));
    refuse_existing(&main)?;
    if test {
        refuse_existing(&suite)?;
    }
    let body = if app {
        "  def greeting(name: String): String = s\"Hello, $name!\"\n\n  \
         def main(args: Array[String]): Unit =\n    println(greeting(args.headOption.getOrElse(\"world\")))\n"
    } else {
        "  def greeting(name: String): String = s\"Hello, $name!\"\n"
    };
    w.write(&main, &format!("package {pkg}\n\nobject {object} {{\n{body}}}\n"))?;
    if test {
        w.write(
            &suite,
            &format!(
                "package {pkg}\n\nclass {object}Suite extends munit.FunSuite {{\n  \
                 test(\"greets by name\") {{\n    assertEquals({object}.greeting(\"scpm\"), \"Hello, scpm!\")\n  }}\n}}\n"
            ),
        )?;
    }
    Ok(())
}

/// Adds `.scpm/` (build output, run logs) to the workspace's `.gitignore` unless it is there.
fn gitignore(root: &Path, w: &mut Writer) -> Result<(), InitError> {
    let path = root.join(".gitignore");
    match fs::read_to_string(&path) {
        Ok(text) => {
            let ignored = text.lines().map(str::trim).any(|l| matches!(l, ".scpm" | ".scpm/" | "/.scpm" | "/.scpm/"));
            if !ignored {
                let sep = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
                w.append(&path, &text, &format!("{sep}.scpm/\n"))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => w.write(&path, ".scpm/\n"),
        Err(e) => Err(io_error(&path, e)),
    }
}

fn check_scala(v: &str) -> Result<(), InitError> {
    let parsed = ScalaVersion::parse(v).map_err(|e| InitError::Usage(format!("--scala: {e}")))?;
    if parsed.binary == "2.12" {
        return Err(InitError::Usage(format!("--scala {v}: scpm compiles Scala 2.13 and 3, not 2.12")));
    }
    Ok(())
}

fn check_name(name: &str, from_dir: Option<&Path>) -> Result<(), InitError> {
    if workspace::valid_module_name(name) {
        return Ok(());
    }
    let rules = "module names use lowercase letters, digits, `-`, `_` and `.`, starting with a letter or digit";
    Err(InitError::Usage(match from_dir {
        Some(dir) => format!(
            "`{name}` (the name of {}) is not a valid module name\n  help: {rules}; pass --name to choose one",
            dir.display()
        ),
        None => format!("`{name}` is not a valid module name\n  help: {rules}"),
    }))
}

fn refuse_existing(path: &Path) -> Result<(), InitError> {
    if path.exists() {
        return Err(InitError::Usage(format!("{} already exists; scpm init never overwrites a file", path.display())));
    }
    Ok(())
}

/// The Scala package for a module: its name with `-` and `.` as `_`, back-quoted if that is a
/// keyword, and `_`-prefixed if it starts with a digit.
fn package_name(module: &str) -> String {
    let mut pkg: String = module.chars().map(|c| if c == '-' || c == '.' { '_' } else { c }).collect();
    if pkg.starts_with(|c: char| c.is_ascii_digit()) {
        pkg.insert(0, '_');
    }
    if KEYWORDS.contains(&pkg.as_str()) { format!("`{pkg}`") } else { pkg }
}

/// The object a library module starts with: its name in PascalCase (`json-codec` → `JsonCodec`).
fn object_name(module: &str) -> String {
    let name: String = module
        .split(['-', '_', '.'])
        .map(|part| {
            let mut c = part.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        })
        .collect();
    if name.starts_with(|c: char| c.is_ascii_alphabetic()) { name } else { format!("Lib{name}") }
}

const KEYWORDS: &[&str] = &[
    "abstract",
    "case",
    "catch",
    "class",
    "def",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "final",
    "finally",
    "for",
    "forSome",
    "given",
    "if",
    "implicit",
    "import",
    "lazy",
    "macro",
    "match",
    "new",
    "null",
    "object",
    "override",
    "package",
    "private",
    "protected",
    "return",
    "sealed",
    "super",
    "then",
    "this",
    "throw",
    "trait",
    "true",
    "try",
    "type",
    "val",
    "var",
    "while",
    "with",
    "yield",
];

fn rel(root: &Path, path: &Path) -> String {
    let p = path.strip_prefix(root).unwrap_or(path);
    let s = p.to_string_lossy().replace('\\', "/");
    if s.is_empty() { ".".to_string() } else { s }
}

fn io_error(path: &Path, e: io::Error) -> InitError {
    InitError::Io(format!("cannot write {}: {e}", path.display()))
}

/// Writes files and records what it did, so a failed init can be taken back.
#[derive(Default)]
struct Writer {
    /// Directories and files created, in order.
    created: Vec<PathBuf>,
    /// Files appended to, with their previous content.
    appended: Vec<PathBuf>,
    originals: Vec<String>,
}

impl Writer {
    fn create_dir_all(&mut self, dir: &Path) -> Result<(), InitError> {
        let missing: Vec<PathBuf> =
            dir.ancestors().take_while(|a| !a.exists()).map(Path::to_path_buf).collect::<Vec<_>>();
        fs::create_dir_all(dir).map_err(|e| io_error(dir, e))?;
        self.created.extend(missing.into_iter().rev());
        Ok(())
    }

    fn write(&mut self, path: &Path, content: &str) -> Result<(), InitError> {
        if let Some(parent) = path.parent() {
            self.create_dir_all(parent)?;
        }
        // `create_new`: a file that appeared since the check is not overwritten either.
        let mut f = fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| io_error(path, e))?;
        self.created.push(path.to_path_buf());
        io::Write::write_all(&mut f, content.as_bytes()).map_err(|e| io_error(path, e))
    }

    fn append(&mut self, path: &Path, original: &str, extra: &str) -> Result<(), InitError> {
        fs::write(path, format!("{original}{extra}")).map_err(|e| io_error(path, e))?;
        self.appended.push(path.to_path_buf());
        self.originals.push(original.to_string());
        Ok(())
    }

    /// Removes what was created (newest first) and restores appended files. Best effort: a
    /// directory that is no longer empty stays.
    fn undo(&mut self) {
        for (path, original) in self.appended.drain(..).zip(self.originals.drain(..)) {
            let _ = fs::write(path, original);
        }
        for path in self.created.drain(..).rev() {
            if path.is_dir() {
                let _ = fs::remove_dir(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_and_object_names() {
        assert_eq!(package_name("app"), "app");
        assert_eq!(package_name("json-codec"), "json_codec");
        assert_eq!(package_name("my.lib"), "my_lib");
        assert_eq!(package_name("2d"), "_2d");
        assert_eq!(package_name("type"), "`type`");
        assert_eq!(object_name("json-codec"), "JsonCodec");
        assert_eq!(object_name("core"), "Core");
        assert_eq!(object_name("2d"), "Lib2d");
    }

    #[test]
    fn a_new_workspace_loads_and_has_one_app_module() {
        let tmp = tempfile::tempdir().unwrap();
        let report = init(&tmp.path().join("hello"), &InitOptions::default()).unwrap();
        assert_eq!(report.kind, "workspace");
        assert_eq!(report.variants, ["app@jvm-3"]);
        assert_eq!(
            report.created,
            [
                "scpm-workspace.yaml",
                "modules/app/scpm.yaml",
                "modules/app/src/main/scala/app/Main.scala",
                "modules/app/src/test/scala/app/MainSuite.scala",
                ".gitignore"
            ]
        );
        assert_eq!(fs::read_to_string(report.root.join(".gitignore")).unwrap(), ".scpm/\n");
    }

    #[test]
    fn scala_213_and_a_name() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = InitOptions { name: Some("tool".into()), scala: Some("2.13.18".into()) };
        let report = init(tmp.path(), &opts).unwrap();
        assert_eq!(report.variants, ["tool@jvm-2.13"]);
        let main = fs::read_to_string(report.root.join("modules/tool/src/main/scala/tool/Main.scala")).unwrap();
        assert!(main.starts_with("package tool\n"), "{main}");
    }

    #[test]
    fn an_existing_gitignore_is_appended_to_once() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target").unwrap();
        let report = init(tmp.path(), &InitOptions::default()).unwrap();
        assert_eq!(report.updated, [".gitignore"]);
        assert_eq!(fs::read_to_string(tmp.path().join(".gitignore")).unwrap(), "target\n.scpm/\n");

        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join(".gitignore"), "/.scpm/\n").unwrap();
        assert!(init(other.path(), &InitOptions::default()).unwrap().updated.is_empty());
    }

    #[test]
    fn a_module_inside_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path(), &InitOptions::default()).unwrap();
        let report = init(&tmp.path().join("modules/json-codec"), &InitOptions::default()).unwrap();
        assert_eq!(report.kind, "module");
        assert_eq!(report.module, "json-codec");
        assert_eq!(report.variants, ["json-codec@jvm-3"]);
        assert_eq!(
            report.created,
            [
                "modules/json-codec/scpm.yaml",
                "modules/json-codec/src/main/scala/json_codec/JsonCodec.scala",
                "modules/json-codec/src/test/scala/json_codec/JsonCodecSuite.scala"
            ]
        );
        let yaml = fs::read_to_string(tmp.path().join("modules/json-codec/scpm.yaml")).unwrap();
        assert!(!yaml.contains("name:") && yaml.contains("org.scalameta::munit: catalog"), "{yaml}");
    }

    #[test]
    fn a_module_outside_the_globs_is_refused_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path(), &InitOptions::default()).unwrap();
        let err = init(&tmp.path().join("tools/gen"), &InitOptions::default()).unwrap_err().to_string();
        assert!(err.contains("tools/gen is not one of the workspace's modules"), "{err}");
        assert!(!tmp.path().join("tools").exists(), "created directories are removed again");
    }

    #[test]
    fn refusals() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path(), &InitOptions::default()).unwrap();
        let again = init(tmp.path(), &InitOptions::default()).unwrap_err().to_string();
        assert!(again.contains("is already a workspace"), "{again}");
        let module = init(&tmp.path().join("modules/app"), &InitOptions::default()).unwrap_err().to_string();
        assert!(module.contains("is already a module"), "{module}");
        let taken = init(&tmp.path().join("modules/other"), &InitOptions { name: Some("app".into()), scala: None })
            .unwrap_err()
            .to_string();
        assert!(taken.contains("already has a module named `app`"), "{taken}");
        assert!(!tmp.path().join("modules/other").exists());

        let bad = tempfile::tempdir().unwrap();
        let name = init(&bad.path().join("Bad"), &InitOptions { name: Some("Bad".into()), scala: None });
        assert!(name.unwrap_err().to_string().contains("not a valid module name"));
        assert!(!bad.path().join("Bad").exists(), "the directory it made is removed again");
        let scala = init(bad.path(), &InitOptions { name: None, scala: Some("2.12.20".into()) });
        assert!(scala.unwrap_err().to_string().contains("not 2.12"));
        let partial = init(bad.path(), &InitOptions { name: None, scala: Some("3".into()) });
        assert!(partial.unwrap_err().to_string().contains("not a full Scala version"));

        let blocked = tempfile::tempdir().unwrap();
        fs::create_dir_all(blocked.path().join("modules/app/src/main/scala/app")).unwrap();
        fs::write(blocked.path().join("modules/app/src/main/scala/app/Main.scala"), "// mine").unwrap();
        let err = init(blocked.path(), &InitOptions::default()).unwrap_err().to_string();
        assert!(err.contains("never overwrites"), "{err}");
        assert!(!blocked.path().join(WORKSPACE_FILE).exists(), "nothing is left behind");
        assert_eq!(
            fs::read_to_string(blocked.path().join("modules/app/src/main/scala/app/Main.scala")).unwrap(),
            "// mine"
        );
    }
}
