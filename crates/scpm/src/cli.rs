//! Command-line interface.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::config::{ModuleFile, WorkspaceFile};
use crate::diag::Diagnostics;
use crate::filter;
use crate::graph::Graph;
use crate::install::{self, ClasspathKind, InstallOptions, InstallSummary};
use crate::lockfile::Lockfile;
use crate::workspace::{self, Workspace};

#[derive(Debug, Parser)]
#[command(name = "scpm", version, about = "A fast, package-manager-first workspace tool for Scala")]
pub struct Cli {
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "dir", global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Machine-readable JSON output (including errors).
    #[arg(long, global = true)]
    pub json: bool,
    /// Select modules/variants with pnpm-style selectors (repeatable): `core`, `core...`,
    /// `...core`, `core@jvm-3`, `./modules/**`, `[origin/main]`, `!core`.
    #[arg(short = 'F', long = "filter", global = true, value_name = "SELECTOR")]
    pub filter: Vec<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List modules and their variants.
    Ls,
    /// Print the resolved configuration of modules or variants.
    #[command(args_conflicts_with_subcommands = true)]
    Config {
        #[command(subcommand)]
        action: Option<ConfigCommand>,
        /// Module names or variant ids; shorthand for `-F`.
        targets: Vec<String>,
    },
    /// Print the module graph.
    Graph {
        #[arg(long, value_enum, default_value_t = GraphFormat::Text)]
        format: GraphFormat,
    },
    /// Lock stale variants and restore every locked artifact into the coursier cache.
    Install {
        /// Fail if the lockfile is out of date instead of updating it (default when CI=true).
        #[arg(long)]
        frozen: bool,
        /// Re-hash cached artifacts against the lock and re-download any that don't match.
        #[arg(long)]
        verify: bool,
    },
    /// Explain why an artifact is on the classpath (`org::name`, `org:name_3` or `name`).
    Why { artifact: String },
    /// Print a variant's classpath (installs first if the lockfile is stale).
    Classpath {
        /// Variant id, e.g. `core@jvm-3`, or a module with a single variant.
        target: String,
        #[arg(long, value_enum, default_value_t = ClasspathKind::Compile)]
        kind: ClasspathKind,
        #[arg(long)]
        frozen: bool,
    },
    /// Print the JSON Schema of a configuration file.
    Schema {
        #[arg(value_enum)]
        file: SchemaFile,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show where each repository's credentials, mirror and proxy come from (machine-local
    /// settings), and which settings of other tools are ignored.
    Explain,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum GraphFormat {
    Text,
    Json,
    Dot,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SchemaFile {
    Workspace,
    Module,
    /// The user config with machine-local settings (design §5.9).
    UserConfig,
}

/// Exit codes: 0 success, 1 the command failed (including writing its output), 2 invalid
/// configuration or usage.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Config(diags)) => {
            if json {
                let error = serde_json::json!({"ok": false, "error": {"kind": "config", "diagnostics": diags}});
                print_json(&error).unwrap_or_else(report_write_error);
            } else {
                eprintln!("{diags}");
            }
            ExitCode::from(2)
        }
        Err(Failure::Usage(message)) => {
            if json {
                let error = serde_json::json!({"ok": false, "error": {"kind": "usage", "message": message}});
                print_json(&error).unwrap_or_else(report_write_error);
            } else {
                eprintln!("error: {message}");
            }
            ExitCode::from(2)
        }
        // A closed pipe (`scpm ls | head`) is not an error.
        Err(Failure::Output(e)) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(Failure::Output(e)) => {
            report_write_error(e);
            ExitCode::FAILURE
        }
        Err(Failure::Command(message)) => {
            if json {
                let _ = print_json(&serde_json::json!({"ok": false, "error": {"kind": "command", "message": message}}));
            } else {
                eprintln!("error: {message}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Reports a failed stdout write on stderr (a closed pipe is not reported). When the output was
/// a JSON error, the exit code stays that error's.
fn report_write_error(e: io::Error) {
    if e.kind() != io::ErrorKind::BrokenPipe {
        eprintln!("error: cannot write output: {e}");
    }
}

enum Failure {
    Config(Diagnostics),
    Usage(String),
    /// The command ran and failed (resolution, download, stale lock): exit code 1.
    Command(String),
    /// Writing to stdout failed.
    Output(io::Error),
}

impl From<install::InstallError> for Failure {
    fn from(e: install::InstallError) -> Self {
        match e {
            install::InstallError::MachineConfig(diags) => Failure::Config(diags),
            other => Failure::Command(other.to_string()),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::Output(e)
    }
}

impl From<Diagnostics> for Failure {
    fn from(d: Diagnostics) -> Self {
        Failure::Config(d)
    }
}

impl From<filter::FilterError> for Failure {
    fn from(e: filter::FilterError) -> Self {
        Failure::Usage(e.to_string())
    }
}

fn run(cli: Cli) -> Result<(), Failure> {
    let cwd = std::env::current_dir().map_err(|e| Failure::Usage(e.to_string()))?;
    let start = match &cli.dir {
        Some(d) => cwd.join(d),
        None => cwd,
    };
    match cli.command {
        Command::Schema { file } => {
            let schema = match file {
                SchemaFile::Workspace => schemars::schema_for!(WorkspaceFile),
                SchemaFile::Module => schemars::schema_for!(ModuleFile),
                SchemaFile::UserConfig => schemars::schema_for!(crate::machine::UserConfigFile),
            };
            Ok(print_json(&schema)?)
        }
        Command::Ls => {
            let (ws, graph) = load(&start)?;
            let selected = filter::select(&ws, &graph, &cli.filter, &invocation_dir(&start)?)?;
            let rows: Vec<LsRow> = ws
                .modules
                .iter()
                .flat_map(|m| m.variants.iter().map(move |v| (m, v)))
                .filter(|(_, v)| selected.contains(&v.id))
                .map(|(m, v)| LsRow { id: v.id.clone(), module: m.name.clone(), dir: m.dir.clone() })
                .collect();
            if cli.json {
                print_json(&rows)?;
            } else {
                let width = rows.iter().map(|r| r.id.len()).max().unwrap_or(0);
                let mut out = io::stdout().lock();
                for r in rows {
                    writeln!(out, "{:width$}  {}", r.id, r.dir)?;
                }
                out.flush()?;
            }
            Ok(())
        }
        Command::Config { action: Some(ConfigCommand::Explain), .. } => {
            let (ws, _) = load(&start)?;
            let explanation = crate::explain::explain(&ws)?;
            if cli.json {
                print_json(&explanation)?;
            } else {
                let mut out = io::stdout().lock();
                out.write_all(crate::explain::render(&explanation).as_bytes())?;
                out.flush()?;
            }
            Ok(())
        }
        Command::Config { action: None, targets } => {
            let (ws, graph) = load(&start)?;
            let mut selectors = cli.filter.clone();
            selectors.extend(targets);
            let selected = filter::select(&ws, &graph, &selectors, &invocation_dir(&start)?)?;
            let variants: Vec<_> = ws.variants().filter(|v| selected.contains(&v.id)).collect();
            if cli.json {
                print_json(&variants)?;
            } else {
                let mut out = io::stdout().lock();
                out.write_all(serde_yaml_ng::to_string(&variants).expect("variants serialize").as_bytes())?;
                out.flush()?;
            }
            Ok(())
        }
        Command::Install { frozen, verify } => {
            let (ws, graph) = load(&start)?;
            let summary = install::install(&ws, &graph, InstallOptions { frozen: frozen || in_ci(), verify })?;
            if cli.json {
                print_json(&summary)?;
            } else {
                print_install_summary(&summary)?;
            }
            Ok(())
        }
        Command::Why { artifact } => {
            let (ws, graph) = load(&start)?;
            let lock = ensure_installed(&ws, &graph, false)?;
            let selected: Vec<String> =
                filter::select(&ws, &graph, &cli.filter, &invocation_dir(&start)?)?.into_iter().collect();
            let report = crate::why::why(&ws, &graph, &lock, &artifact, &selected)?;
            if cli.json {
                print_json(&report)?;
            } else {
                // `report.warnings` were printed by `ensure_installed` already.
                for n in &report.notices {
                    eprintln!("note: {n}");
                }
                let mut out = io::stdout().lock();
                if report.paths.is_empty() && report.unexplained.is_empty() {
                    writeln!(out, "`{artifact}` is not on any selected classpath")?;
                }
                for p in &report.paths {
                    let via =
                        if p.declared_by == p.variant { String::new() } else { format!(" (via {})", p.declared_by) };
                    writeln!(out, "{}{via}: {}", p.variant, p.chain.join(" → "))?;
                }
                for u in &report.unexplained {
                    writeln!(
                        out,
                        "{}: {} is on the classpath, but coursier reported no dependency chain to it",
                        u.variant,
                        u.artifacts.join(", ")
                    )?;
                }
                out.flush()?;
            }
            Ok(())
        }
        Command::Classpath { target, kind, frozen } => {
            let (ws, graph) = load(&start)?;
            let cwd = invocation_dir(&start)?;
            let mut selected = filter::select(&ws, &graph, std::slice::from_ref(&target), &cwd)?;
            // The global `-F` selectors narrow what the target may name.
            if !cli.filter.is_empty() {
                let allowed = filter::select(&ws, &graph, &cli.filter, &cwd)?;
                selected.retain(|id| allowed.contains(id));
                if selected.is_empty() {
                    return Err(Failure::Usage(format!("`{target}` is excluded by --filter")));
                }
            }
            let [id] = selected.iter().collect::<Vec<_>>()[..] else {
                return Err(Failure::Usage(format!(
                    "`{target}` matches {} variants; name one: {}",
                    selected.len(),
                    selected.iter().cloned().collect::<Vec<_>>().join(", ")
                )));
            };
            let lock = ensure_installed(&ws, &graph, frozen)?;
            let files = install::classpath_files(&ws, &lock, id, kind)?;
            if cli.json {
                print_json(&files)?;
            } else {
                let sep = if cfg!(windows) { ";" } else { ":" };
                let mut out = io::stdout().lock();
                writeln!(out, "{}", files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(sep))?;
                out.flush()?;
            }
            Ok(())
        }
        Command::Graph { format } => {
            let (ws, graph) = load(&start)?;
            let selected = filter::select(&ws, &graph, &cli.filter, &invocation_dir(&start)?)?;
            let order: Vec<&String> = graph.order.iter().filter(|id| selected.contains(*id)).collect();
            let edges: Vec<_> =
                graph.edges.iter().filter(|e| selected.contains(&e.from) && selected.contains(&e.to)).collect();
            let format = if cli.json { GraphFormat::Json } else { format };
            let mut out = io::stdout().lock();
            match format {
                GraphFormat::Json => print_json(&serde_json::json!({"order": order, "edges": edges}))?,
                GraphFormat::Text => {
                    for id in order {
                        let deps: Vec<String> = graph
                            .dependencies_of(id)
                            .filter(|e| selected.contains(&e.to))
                            .map(|e| match e.scope {
                                workspace::Scope::Compile => e.to.clone(),
                                scope => format!(
                                    "{} ({})",
                                    e.to,
                                    serde_json::to_value(scope).expect("scope").as_str().unwrap_or("")
                                ),
                            })
                            .collect();
                        if deps.is_empty() {
                            writeln!(out, "{id}")?;
                        } else {
                            writeln!(out, "{id} → {}", deps.join(", "))?;
                        }
                    }
                }
                GraphFormat::Dot => {
                    writeln!(out, "digraph scpm {{")?;
                    for id in order {
                        writeln!(out, "  \"{id}\";")?;
                    }
                    for e in edges {
                        let style = if e.scope == workspace::Scope::Test { " [style=dashed]" } else { "" };
                        writeln!(out, "  \"{}\" -> \"{}\"{style};", e.from, e.to)?;
                    }
                    writeln!(out, "}}")?;
                }
            }
            out.flush()?;
            Ok(())
        }
    }
}

#[derive(Serialize)]
struct LsRow {
    id: String,
    module: String,
    dir: String,
}

/// The canonical directory scpm runs in; path selectors are relative to it.
fn invocation_dir(start: &std::path::Path) -> Result<PathBuf, Failure> {
    start.canonicalize().map_err(|e| Failure::Usage(format!("cannot access {}: {e}", start.display())))
}

fn load(start: &std::path::Path) -> Result<(Workspace, Graph), Failure> {
    let ws = workspace::load(start)?;
    let graph = Graph::build(&ws)?;
    Ok((ws, graph))
}

/// Install-before-run (design §5.4): re-lock and fetch as needed (frozen in CI), then read the lock.
fn ensure_installed(ws: &Workspace, graph: &Graph, frozen: bool) -> Result<Lockfile, Failure> {
    let summary = install::install(ws, graph, InstallOptions { frozen: frozen || in_ci(), verify: false })?;
    for w in &summary.warnings {
        eprintln!("warning: {w}");
    }
    for n in &summary.notices {
        eprintln!("note: {n}");
    }
    if !summary.relocked.is_empty() || summary.fetch.downloaded > 0 {
        eprintln!("installed: {} re-locked, {} downloaded", summary.relocked.len(), summary.fetch.downloaded);
    }
    Lockfile::read(&ws.root)
        .map_err(|e| Failure::Command(e.to_string()))?
        .ok_or_else(|| Failure::Command("no scpm-lock.yaml; run `scpm install`".into()))
}

/// CI mode (implied `--frozen`) is `CI=true` (any case: AppVeyor sets `True`).
fn in_ci() -> bool {
    std::env::var("CI").is_ok_and(|v| v.eq_ignore_ascii_case("true"))
}

fn print_install_summary(s: &InstallSummary) -> io::Result<()> {
    for w in &s.warnings {
        eprintln!("warning: {w}");
    }
    for n in &s.notices {
        eprintln!("note: {n}");
    }
    let mut out = io::stdout().lock();
    let removed = if s.removed.is_empty() { String::new() } else { format!("removed {}", s.removed.join(", ")) };
    if !s.relocked.is_empty() || !s.boot_relocked.is_empty() {
        let mut what = Vec::new();
        if !s.relocked.is_empty() {
            what.push(format!("{} variant{}", s.relocked.len(), if s.relocked.len() == 1 { "" } else { "s" }));
        }
        if !s.boot_relocked.is_empty() {
            what.push(format!(
                "{} boot entr{}",
                s.boot_relocked.len(),
                if s.boot_relocked.len() == 1 { "y" } else { "ies" }
            ));
        }
        writeln!(
            out,
            "Resolved {} in {:.1}s{}{}",
            what.join(" and "),
            s.resolve_ms as f64 / 1000.0,
            if removed.is_empty() { String::new() } else { format!("; {removed}") },
            if s.lockfile_written { "; wrote scpm-lock.yaml" } else { "" }
        )?;
    } else if s.lockfile_written {
        if removed.is_empty() {
            writeln!(out, "Lockfile updated")?;
        } else {
            writeln!(out, "Lockfile updated ({removed})")?;
        }
    } else {
        writeln!(out, "Lockfile up to date ({} variants)", s.up_to_date)?;
    }
    if s.fetch.verified > 0 {
        writeln!(out, "Verified {} cached artifacts; {} re-downloaded", s.fetch.verified, s.fetch.repaired)?;
    }
    if s.fetch.downloaded > 0 {
        writeln!(
            out,
            "Downloaded {} artifact{} ({:.1} MB) in {:.1}s; {} already cached",
            s.fetch.downloaded,
            if s.fetch.downloaded == 1 { "" } else { "s" },
            s.fetch.bytes as f64 / 1_000_000.0,
            s.fetch_ms as f64 / 1000.0,
            s.fetch.present
        )?;
    } else {
        writeln!(out, "All {} artifacts cached", s.fetch.present)?;
    }
    out.flush()
}

fn print_json<T: Serialize + ?Sized>(value: &T) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    out.flush()
}
