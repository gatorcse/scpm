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
    Config {
        /// Module names or variant ids; shorthand for `-F`.
        targets: Vec<String>,
    },
    /// Print the module graph.
    Graph {
        #[arg(long, value_enum, default_value_t = GraphFormat::Text)]
        format: GraphFormat,
    },
    /// Print the JSON Schema of a configuration file.
    Schema {
        #[arg(value_enum)]
        file: SchemaFile,
    },
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
}

/// Exit codes: 0 success, 1 the command failed, 2 invalid configuration or usage.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Config(diags)) => {
            if json {
                print_json(&serde_json::json!({"ok": false, "error": {"kind": "config", "diagnostics": diags}}));
            } else {
                eprintln!("{diags}");
            }
            ExitCode::from(2)
        }
        Err(Failure::Usage(message)) => {
            if json {
                print_json(&serde_json::json!({"ok": false, "error": {"kind": "usage", "message": message}}));
            } else {
                eprintln!("error: {message}");
            }
            ExitCode::from(2)
        }
    }
}

enum Failure {
    Config(Diagnostics),
    Usage(String),
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
    let start = match &cli.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir().map_err(|e| Failure::Usage(e.to_string()))?,
    };
    match cli.command {
        Command::Schema { file } => {
            let schema = match file {
                SchemaFile::Workspace => schemars::schema_for!(WorkspaceFile),
                SchemaFile::Module => schemars::schema_for!(ModuleFile),
            };
            print_json(&schema);
            Ok(())
        }
        Command::Ls => {
            let (ws, graph) = load(&start)?;
            let selected = filter::select(&ws, &graph, &cli.filter)?;
            let rows: Vec<LsRow> = ws
                .modules
                .iter()
                .flat_map(|m| m.variants.iter().map(move |v| (m, v)))
                .filter(|(_, v)| selected.contains(&v.id))
                .map(|(m, v)| LsRow { id: v.id.clone(), module: m.name.clone(), dir: m.dir.clone() })
                .collect();
            if cli.json {
                print_json(&rows);
            } else {
                let width = rows.iter().map(|r| r.id.len()).max().unwrap_or(0);
                let mut out = io::stdout().lock();
                for r in rows {
                    let _ = writeln!(out, "{:width$}  {}", r.id, r.dir);
                }
            }
            Ok(())
        }
        Command::Config { targets } => {
            let (ws, graph) = load(&start)?;
            let mut selectors = cli.filter.clone();
            selectors.extend(targets);
            let selected = filter::select(&ws, &graph, &selectors)?;
            let variants: Vec<_> = ws.variants().filter(|v| selected.contains(&v.id)).collect();
            if cli.json {
                print_json(&variants);
            } else {
                print!("{}", serde_yaml_ng::to_string(&variants).expect("variants serialize"));
            }
            Ok(())
        }
        Command::Graph { format } => {
            let (ws, graph) = load(&start)?;
            let selected = filter::select(&ws, &graph, &cli.filter)?;
            let order: Vec<&String> = graph.order.iter().filter(|id| selected.contains(*id)).collect();
            let edges: Vec<_> =
                graph.edges.iter().filter(|e| selected.contains(&e.from) && selected.contains(&e.to)).collect();
            let format = if cli.json { GraphFormat::Json } else { format };
            let mut out = io::stdout().lock();
            match format {
                GraphFormat::Json => print_json(&serde_json::json!({"order": order, "edges": edges})),
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
                            let _ = writeln!(out, "{id}");
                        } else {
                            let _ = writeln!(out, "{id} → {}", deps.join(", "));
                        }
                    }
                }
                GraphFormat::Dot => {
                    let _ = writeln!(out, "digraph scpm {{");
                    for id in order {
                        let _ = writeln!(out, "  \"{id}\";");
                    }
                    for e in edges {
                        let style = if e.scope == workspace::Scope::Test { " [style=dashed]" } else { "" };
                        let _ = writeln!(out, "  \"{}\" -> \"{}\"{style};", e.from, e.to);
                    }
                    let _ = writeln!(out, "}}");
                }
            }
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

fn load(start: &std::path::Path) -> Result<(Workspace, Graph), Failure> {
    let ws = workspace::load(start)?;
    let graph = Graph::build(&ws)?;
    Ok((ws, graph))
}

fn print_json<T: Serialize + ?Sized>(value: &T) {
    let mut out = io::stdout().lock();
    let _ = serde_json::to_writer_pretty(&mut out, value);
    let _ = writeln!(out);
}
