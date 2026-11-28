//! Command-line interface.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::compile;
use crate::config::{ModuleFile, WorkspaceFile};
use crate::diag::Diagnostics;
use crate::filter;
use crate::graph::Graph;
use crate::ide;
use crate::install::{self, ClasspathKind, InstallOptions, InstallSummary};
use crate::lockfile::Lockfile;
use crate::report::{Event, Mode, Reporter};
use crate::testing;
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
    /// Don't use the shared compile daemon; run a worker for this command only (also
    /// `SCPM_NO_DAEMON=1`; always the case in CI).
    #[arg(long, global = true)]
    pub no_daemon: bool,
    /// Select modules/variants with pnpm-style selectors (repeatable): `core`, `core...`,
    /// `...core`, `core@jvm-3`, `./modules/**`, `[origin/main]`, `!core`.
    #[arg(short = 'F', long = "filter", global = true, value_name = "SELECTOR")]
    pub filter: Vec<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a workspace with one app module, or add a module to the workspace around DIR. Never
    /// overwrites a file; needs no network.
    Init {
        /// The directory to initialize (created if missing). Inside a workspace, it becomes a
        /// new library module, and the workspace's `modules:` must match it.
        // Not `dir`: that id is the global `-C/--dir`, and clap would fill both.
        #[arg(default_value = ".", value_name = "DIR")]
        path: PathBuf,
        /// The module's name: default `app` in a new workspace, else the directory's name.
        #[arg(long)]
        name: Option<String>,
        /// A full Scala version, e.g. `2.13.18`: the new workspace's (default `3.9.0`), or the
        /// new module's own.
        #[arg(long, value_name = "VERSION")]
        scala: Option<String>,
    },
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
        /// Also restore these systems' files (e.g. `linux-x86_64`), besides the host system's.
        #[arg(long = "system", value_name = "SYSTEM", value_delimiter = ',', conflicts_with = "all_systems")]
        systems: Vec<String>,
        /// Restore every system's files, not just the host system's.
        #[arg(long)]
        all_systems: bool,
        /// Lock for scpm's current default system set, replacing the one the lock's header
        /// recorded (only when `lock.systems` is unset).
        #[arg(long)]
        reset_systems: bool,
    },
    /// Compile modules (and what they depend on). Unchanged variants are skipped without a JVM.
    Compile {
        /// Module names or variant ids; shorthand for `-F`.
        targets: Vec<String>,
        /// Also compile test sources.
        #[arg(long)]
        tests: bool,
        #[arg(long)]
        frozen: bool,
    },
    /// Compile and run tests (forked JVM per variant; JUnit XML in .scpm/out/<id>/test-reports).
    /// Tests run with the module directory as their working directory.
    Test {
        /// Module names or variant ids; shorthand for `-F`.
        targets: Vec<String>,
        /// Only suites whose fully qualified name matches this glob (repeatable).
        #[arg(long = "suite", value_name = "GLOB")]
        suites: Vec<String>,
        /// Only test cases with this name (repeatable).
        #[arg(short = 't', long = "test", value_name = "NAME")]
        tests: Vec<String>,
        /// Kill a variant's test JVM after this many seconds.
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
        #[arg(long)]
        frozen: bool,
    },
    /// Compile a module and run its main class with the remaining arguments. The program runs
    /// in the current directory (not the module's, and not `-C`'s), like `java` would.
    Run {
        /// Variant id or a module with a single variant.
        target: String,
        /// Main class (default: the module's `mainClass`).
        #[arg(long)]
        main: Option<String>,
        #[arg(long)]
        frozen: bool,
        /// Arguments for the program (after `--`).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Manage the shared compile daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Serve the Model Context Protocol on stdio for agents (keeps a compile worker warm).
    Mcp,
    /// Serve the Build Server Protocol on stdio, for IntelliJ, Metals v1 and other BSP clients
    /// (they start it through `.bsp/scpm.json`; `--setup` writes that file).
    Bsp {
        /// Write `.bsp/scpm.json` (pointing at this scpm binary) instead of serving, so BSP
        /// clients find scpm. A non-frozen `scpm install` keeps it up to date afterwards.
        #[arg(long)]
        setup: bool,
        /// With --setup: replace a `.bsp/scpm.json` that scpm didn't write.
        #[arg(long, requires = "setup")]
        force: bool,
    },
    /// Explain why an artifact is on the classpath (`org::name`, `org:name_3` or `name`).
    Why {
        artifact: String,
        /// Explain this system's classpath instead of the host system's.
        #[arg(long, value_name = "SYSTEM")]
        system: Option<String>,
    },
    /// Print a variant's classpath (installs first if the lockfile is stale).
    Classpath {
        /// Variant id, e.g. `core@jvm-3`, or a module with a single variant.
        target: String,
        #[arg(long, value_enum, default_value_t = ClasspathKind::Compile)]
        kind: ClasspathKind,
        #[arg(long)]
        frozen: bool,
        /// Print this system's classpath (e.g. `linux-x86_64`) instead of the host system's;
        /// its files are restored too.
        #[arg(long, value_name = "SYSTEM")]
        system: Option<String>,
    },
    /// Print the JSON Schema of a configuration file.
    Schema {
        #[arg(value_enum)]
        file: SchemaFile,
    },
    /// Generate IDE project files from the workspace and the lockfile (installs first if the
    /// lock is stale). A non-frozen `scpm install` keeps generated files up to date.
    Ide {
        #[command(subcommand)]
        target: IdeTarget,
    },
    /// Generate native IntelliJ project files in `.idea/` (same as `scpm ide idea`).
    Idea(IdeaArgs),
}

#[derive(Debug, Subcommand)]
pub enum IdeTarget {
    /// Write `.metals/mbt.json`, the build description Metals v2 reads.
    Metals {
        /// Write the file here instead (e.g. `"$MBT_OUTPUT_FILE"` in a Metals `*.mbt.sh`
        /// importer script).
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
        #[command(flatten)]
        sources: SourcesArgs,
        #[arg(long)]
        frozen: bool,
    },
    /// Write native IntelliJ project files in `.idea/`: one module per scpm module (its
    /// `ide.primary` variant), libraries, Scala SDKs and the project JDK.
    Idea(IdeaArgs),
}

#[derive(Debug, Clone, Copy, clap::Args)]
pub struct SourcesArgs {
    /// Don't download missing sources jars; use only those already in the coursier cache.
    #[arg(long)]
    no_sources: bool,
    /// Ask again for sources jars of modules recorded as publishing none.
    #[arg(long, conflicts_with = "no_sources")]
    refresh_sources: bool,
}

impl SourcesArgs {
    fn options(self) -> ide::SourcesOptions {
        ide::SourcesOptions { fetch: !self.no_sources, refresh: self.refresh_sources }
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct IdeaArgs {
    #[command(flatten)]
    sources: SourcesArgs,
    /// Also replace IntelliJ files that scpm didn't generate (by default they are left alone).
    #[arg(long)]
    force: bool,
    #[arg(long)]
    frozen: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show where each repository's credentials, mirror and proxy come from (machine-local
    /// settings), and which settings of other tools are ignored.
    Explain,
}

#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// List running daemons.
    Ls,
    /// Stop every running daemon (waiting up to 30 s for active compiles).
    Stop {
        /// Cancel active compiles instead of waiting for them.
        #[arg(long)]
        force: bool,
    },
    /// Print the log of the most recently active daemon.
    Logs {
        /// Only the last N lines.
        #[arg(long, value_name = "N")]
        tail: Option<usize>,
    },
    /// Run a daemon in the foreground (started automatically by other commands).
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        key: String,
        /// A worker classpath entry (repeated).
        #[arg(long = "cp")]
        classpath: Vec<PathBuf>,
        /// A worker JVM option (repeated), e.g. `--jvm-opt=-Xmx4g`.
        #[arg(long = "jvm-opt", allow_hyphen_values = true)]
        jvm_options: Vec<String>,
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
        Err(Failure::Silent) => ExitCode::FAILURE,
        Err(Failure::Exit(code)) => ExitCode::from(code),
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
    /// Failed, and the reporter already said why: exit code 1, nothing more to print.
    Silent,
    /// Pass through a child process's exit code (`scpm run`).
    Exit(u8),
    /// Writing to stdout failed.
    Output(io::Error),
}

impl From<install::InstallError> for Failure {
    fn from(e: install::InstallError) -> Self {
        match e {
            install::InstallError::MachineConfig(diags) => Failure::Config(diags),
            install::InstallError::Coursier(crate::coursier::CoursierError::CredentialClash(d)) => {
                Failure::Config(d.into())
            }
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
    if cli.no_daemon {
        crate::daemon::disable();
    }
    let cwd = std::env::current_dir().map_err(|e| Failure::Usage(e.to_string()))?;
    let start = match &cli.dir {
        Some(d) => cwd.join(d),
        None => cwd,
    };
    match cli.command {
        Command::Init { path, name, scala } => {
            let target = start.join(&path);
            let report = crate::init::init(&target, &crate::init::InitOptions { name, scala }).map_err(|e| {
                let message = e.to_string();
                match e {
                    // Said first, then the diagnostics that explain it.
                    crate::init::InitError::Invalid(d) | crate::init::InitError::Workspace(d) => {
                        eprintln!("error: {message}");
                        Failure::Config(d)
                    }
                    _ => Failure::Usage(message),
                }
            })?;
            if cli.json {
                print_json(&report)?;
            } else {
                print_init(&report, &start)?;
            }
            Ok(())
        }
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
        Command::Install { frozen, verify, systems, all_systems, reset_systems } => {
            let (ws, graph) = load(&start)?;
            let frozen = frozen || in_ci();
            for s in &systems {
                check_system_name(s)?;
            }
            let restore = if all_systems {
                install::RestoreSystems::All
            } else if systems.is_empty() {
                install::RestoreSystems::Host
            } else {
                install::RestoreSystems::Also(systems)
            };
            let summary = install::install(&ws, &graph, InstallOptions { frozen, verify, restore, reset_systems })?;
            if cli.json {
                print_json(&summary)?;
            } else {
                print_install_summary(&summary)?;
            }
            if !frozen && let Some(lock) = Lockfile::read(&ws.root).map_err(|e| Failure::Command(e.to_string()))? {
                let (lock, _) = install::host_projection(&lock)?;
                report_refresh(&ide::refresh(&ws, &lock));
            }
            Ok(())
        }
        Command::Ide { target: IdeTarget::Metals { output, sources, frozen } } => {
            let (ws, graph) = load(&start)?;
            let lock = install_quietly(&ws, &graph, frozen)?;
            let tc = compile::Toolchain::new(&ws, &lock).map_err(|e| Failure::Command(e.to_string()))?;
            let opts = ide::metals::Options { output: output.map(|o| start.join(o)), sources: sources.options() };
            let s = ide::metals::generate(&tc, &opts).map_err(|e| Failure::Command(e.to_string()))?;
            if cli.json {
                print_json(&s)?;
            } else {
                for w in &s.warnings {
                    eprintln!("warning: {w}");
                }
                println!(
                    "{} {} ({} namespaces, {} dependency modules; sources jars: {} cached, {} fetched, {} unavailable)",
                    if s.written { "Wrote" } else { "Up to date:" },
                    s.path,
                    s.namespaces,
                    s.dependency_modules,
                    s.sources.cached,
                    s.sources.fetched,
                    s.sources.missing
                );
            }
            Ok(())
        }
        Command::Ide { target: IdeTarget::Idea(args) } | Command::Idea(args) => {
            let (ws, graph) = load(&start)?;
            let lock = install_quietly(&ws, &graph, args.frozen)?;
            let tc = compile::Toolchain::new(&ws, &lock).map_err(|e| Failure::Command(e.to_string()))?;
            let mut s =
                ide::idea::generate(&tc, &ide::idea::Options { sources: args.sources.options(), force: args.force })
                    .map_err(|e| Failure::Command(e.to_string()))?;
            if let Some(w) = s.skipped_warning() {
                s.warnings.insert(0, w);
            }
            if cli.json {
                print_json(&s)?;
            } else {
                for w in &s.warnings {
                    eprintln!("warning: {w}");
                }
                for (module, variant) in &s.modules {
                    println!("{module}: {variant}");
                }
                println!(
                    "{} IntelliJ module{}, {} libraries; {} file{} written, {} removed, {} skipped (open the workspace directory in IntelliJ)",
                    s.modules.len(),
                    if s.modules.len() == 1 { "" } else { "s" },
                    s.libraries,
                    s.written.len(),
                    if s.written.len() == 1 { "" } else { "s" },
                    s.removed.len(),
                    s.skipped.len()
                );
            }
            Ok(())
        }
        Command::Compile { targets, tests, frozen } => {
            let (ws, graph) = load(&start)?;
            let mut selectors = cli.filter.clone();
            selectors.extend(targets);
            let selected = filter::select(&ws, &graph, &selectors, &invocation_dir(&start)?)?;
            let (lock, _) = ensure_installed(&ws, &graph, frozen)?;
            let _guard = compile::workspace_lock(&ws.root).map_err(|e| Failure::Command(e.to_string()))?;
            let started = std::time::Instant::now();
            let reporter = std::sync::Arc::new(Reporter::new(Mode::detect(cli.json), &ws.root, &run_id()));
            reporter.emit(Event::RunStart { run: run_id(), command: "compile".into() });
            let result = compile::Toolchain::new(&ws, &lock).and_then(|tc| {
                let handle = compile::WorkerHandle::new(&tc, reporter.clone());
                let result =
                    compile::compile(&tc, &graph, &selected, compile::Scope { tests, runtime: false }, &handle);
                handle.shutdown();
                result
            });
            let elapsed = started.elapsed().as_millis() as u64;
            match result {
                Ok(ok) => {
                    reporter.finish("compile", ok, elapsed, None);
                    if ok { Ok(()) } else { Err(Failure::Silent) }
                }
                // The reporter includes the error, so JSON output stays one document.
                Err(e) => {
                    reporter.finish("compile", false, elapsed, Some(&e.to_string()));
                    Err(Failure::Silent)
                }
            }
        }
        Command::Test { targets, suites, tests, timeout, frozen } => {
            let (ws, graph) = load(&start)?;
            let mut selectors = cli.filter.clone();
            selectors.extend(targets);
            let selected = filter::select(&ws, &graph, &selectors, &invocation_dir(&start)?)?;
            let (lock, _) = ensure_installed(&ws, &graph, frozen)?;
            let _guard = compile::workspace_lock(&ws.root).map_err(|e| Failure::Command(e.to_string()))?;
            let started = std::time::Instant::now();
            let reporter = std::sync::Arc::new(Reporter::new(Mode::detect(cli.json), &ws.root, &run_id()));
            reporter.emit(Event::RunStart { run: run_id(), command: "test".into() });
            let filtered = !suites.is_empty() || !tests.is_empty();
            let opts = testing::TestOptions {
                filters: suites,
                test_names: tests,
                timeout: (timeout > 0).then(|| std::time::Duration::from_secs(timeout)),
                report_name: None,
            };
            let result = compile::Toolchain::new(&ws, &lock).and_then(|tc| {
                // Clear every selected variant's report, including ones that no longer have tests.
                let all: Vec<_> = selected.iter().filter_map(|id| ws.variant(id)).collect();
                testing::clear_reports(&tc, &all);
                let variants = testing::with_tests(&tc, &selected)?;
                // Check the runner before compiling anything.
                let runner_cp = if variants.is_empty() { Vec::new() } else { testing::runner_classpath(&tc)? };
                let handle = compile::WorkerHandle::new(&tc, reporter.clone());
                let compiled =
                    compile::compile(&tc, &graph, &selected, compile::Scope { tests: true, runtime: true }, &handle);
                // The compile worker isn't needed while tests run.
                handle.shutdown();
                if !compiled? {
                    return Ok(Ok(false));
                }
                if variants.is_empty() {
                    if filtered {
                        return Ok(Err("no tests matched --suite / --test".to_string()));
                    }
                    eprintln!("no test sources in the selected modules");
                    return Ok(Ok(true));
                }
                let ok = testing::run_tests(&tc, &variants, &runner_cp, &opts, &reporter);
                if filtered && reporter.tests_run() == 0 {
                    return Ok(Err("no tests matched --suite / --test".to_string()));
                }
                Ok(Ok(ok))
            });
            let elapsed = started.elapsed().as_millis() as u64;
            let (ok, error) = match result {
                Ok(Ok(ok)) => (ok, None),
                Ok(Err(message)) => (false, Some(message)),
                Err(e) => (false, Some(e.to_string())),
            };
            reporter.finish("test", ok, elapsed, error.as_deref());
            if ok { Ok(()) } else { Err(Failure::Silent) }
        }
        Command::Run { target, main, frozen, args } => {
            let (ws, graph) = load(&start)?;
            // The target must also pass the global --filter (an intersection, as for `classpath`).
            let cwd = invocation_dir(&start)?;
            let mut selected = filter::select(&ws, &graph, std::slice::from_ref(&target), &cwd)?;
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
            let (lock, _) = ensure_installed(&ws, &graph, frozen)?;
            let tc = compile::Toolchain::new(&ws, &lock).map_err(|e| Failure::Command(e.to_string()))?;
            let v = ws.variant(id).expect("selected ids exist");
            {
                let _guard = compile::workspace_lock(&ws.root).map_err(|e| Failure::Command(e.to_string()))?;
                // Build output goes to stderr only; stdout belongs to the program.
                let reporter = std::sync::Arc::new(Reporter::new(Mode::Text, &ws.root, &run_id()));
                let handle = compile::WorkerHandle::new(&tc, reporter.clone());
                let target: std::collections::BTreeSet<String> = [v.id.clone()].into();
                let compiled =
                    compile::compile(&tc, &graph, &target, compile::Scope { tests: false, runtime: true }, &handle);
                // Stop the compile worker before the program starts.
                handle.shutdown();
                if !compiled.map_err(|e| Failure::Command(e.to_string()))? {
                    return Err(Failure::Silent);
                }
            }
            match testing::exec_main(&tc, v, main.as_deref(), &args) {
                Ok(0) => Ok(()),
                Ok(code) => Err(Failure::Exit(code.clamp(1, 255) as u8)),
                Err(e) => Err(Failure::Command(e.to_string())),
            }
        }
        Command::Daemon { action } => match action {
            DaemonAction::Ls => {
                let daemons = crate::daemon::list();
                if cli.json {
                    print_json(&daemons)?;
                } else if daemons.is_empty() {
                    println!("no daemons running");
                } else {
                    for d in daemons {
                        let key = d["key"].as_str().unwrap_or("");
                        if d["state"] != "running" {
                            println!("{key}  pid {}  {}", d["pid"], d["state"].as_str().unwrap_or("unknown"));
                            continue;
                        }
                        let rss = d["workerRssKb"].as_u64().map_or("?".to_string(), |kb| format!("{}M", kb / 1024));
                        println!(
                            "{key}  pid {}  worker {} ({rss} of {} heap)  up {}s  idle {}s  {} active  {} requests  {} client(s)  {} workspace(s)",
                            d["pid"],
                            d["workerPid"],
                            d["heap"].as_str().unwrap_or("?"),
                            d["uptimeSeconds"],
                            d["idleSeconds"],
                            d["activeRequests"],
                            d["requests"],
                            d["clients"],
                            d["workspaces"].as_array().map_or(0, Vec::len)
                        );
                    }
                }
                Ok(())
            }
            DaemonAction::Stop { force } => {
                let (n, failed) = crate::daemon::stop_all(force, |msg| eprintln!("{msg}"));
                if cli.json {
                    print_json(&serde_json::json!({"stopped": n, "failed": failed}))?;
                } else {
                    println!("stopped {n} daemon{}", if n == 1 { "" } else { "s" });
                }
                if failed.is_empty() { Ok(()) } else { Err(Failure::Silent) }
            }
            DaemonAction::Logs { tail } => {
                let Some(log) = crate::daemon::logs().into_iter().next() else {
                    return Err(Failure::Command("no daemon logs".into()));
                };
                let text =
                    std::fs::read_to_string(&log).map_err(|e| Failure::Command(format!("{}: {e}", log.display())))?;
                let lines: Vec<&str> = text.lines().collect();
                let from = tail.map_or(0, |n| lines.len().saturating_sub(n));
                if cli.json {
                    print_json(&serde_json::json!({"log": log, "lines": &lines[from..]}))?;
                    return Ok(());
                }
                eprintln!("==> {} <==", log.display());
                let mut out = std::io::stdout().lock();
                for line in &lines[from..] {
                    writeln!(out, "{line}")?;
                }
                out.flush()?;
                Ok(())
            }
            DaemonAction::Serve { key, classpath, jvm_options } => {
                crate::daemon::serve(crate::daemon::ServeArgs { key, classpath, jvm_options })
                    .map_err(|e| Failure::Command(e.to_string()))
            }
        },
        Command::Bsp { setup: true, force } => {
            let (root, _) = workspace::find_root(&start)?;
            let exe = ide::bsp::invoked_exe().map_err(|e| Failure::Command(e.to_string()))?;
            let s = ide::bsp::setup(&root, &exe, force).map_err(|e| Failure::Command(e.to_string()))?;
            if ide::idea::generated_before(&root) {
                eprintln!(
                    "warning: this workspace also has IntelliJ project files from `scpm idea`; importing it over BSP replaces that setup (open one or the other, and delete `.idea/` before a BSP import)"
                );
            }
            if cli.json {
                print_json(&s)?;
            } else {
                println!("{} {} (runs {} bsp)", if s.written { "Wrote" } else { "Up to date:" }, s.path, exe.display());
            }
            Ok(())
        }
        Command::Bsp { setup: false, .. } => match crate::bsp::serve(start) {
            Ok(0) => Ok(()),
            Ok(code) => Err(Failure::Exit(code.clamp(1, 255) as u8)),
            Err(e) => Err(Failure::Command(e.to_string())),
        },
        Command::Mcp => {
            crate::mcp::Server::new(start).serve().map_err(|e| Failure::Command(e.to_string()))?;
            Ok(())
        }
        Command::Why { artifact, system } => {
            let (ws, graph) = load(&start)?;
            // `why` runs `cs`, which resolves for the machine it runs on: it can explain only the
            // host system's classpath (design §5.2.1, "M1 scope").
            if let Some(s) = &system {
                check_system_name(s)?;
                let host = &crate::system::host().system;
                if s != host {
                    return Err(Failure::Command(format!(
                        "`scpm why --system {s}` needs the resolver sidecar's `graph` method, which comes with M2 of \
                         the sidecar (design §5.2.1); this scpm explains only the host system's classpath ({host})"
                    )));
                }
            }
            let (lock, install_notices) = ensure_installed(&ws, &graph, false)?;
            let selected: Vec<String> =
                filter::select(&ws, &graph, &cli.filter, &invocation_dir(&start)?)?.into_iter().collect();
            let mut report = crate::why::why(&ws, &graph, &lock, &artifact, &selected)?;
            if cli.json {
                // The install before `why` may have taken the notice (printed on stderr); keep it here too.
                report.notices.splice(0..0, install_notices);
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
        Command::Classpath { target, kind, frozen, system } => {
            if let Some(s) = &system {
                check_system_name(s)?;
            }
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
            let lock = match &system {
                Some(s) => {
                    let (lock, summary) = install::ensure_for(&ws, &graph, frozen, Some(s))?;
                    report_ensure(&summary);
                    lock
                }
                None => ensure_installed(&ws, &graph, frozen)?.0,
            };
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

/// Install-before-run (design §5.4): re-lock and fetch as needed (frozen in CI), then read the
/// lock. Outside frozen mode, IDE files scpm maintains are refreshed too: config edits that
/// don't re-lock (scalacOptions, sources, `ide.primary`, …) reach them as well. The refresh
/// uses cached sources jars only (no `cs`), and returns at once when there are no IDE files.
/// Warnings and notices are printed on stderr; the notices are returned too, for `--json` reports.
fn ensure_installed(ws: &Workspace, graph: &Graph, frozen: bool) -> Result<(Lockfile, Vec<String>), Failure> {
    let (lock, summary) = install::ensure(ws, graph, frozen)?;
    report_ensure(&summary);
    if !frozen && !in_ci() {
        report_refresh(&ide::refresh(ws, &lock));
    }
    Ok((lock, summary.notices))
}

/// Install-before-run without the IDE refresh (for the commands that generate IDE files).
fn install_quietly(ws: &Workspace, graph: &Graph, frozen: bool) -> Result<Lockfile, Failure> {
    let (lock, summary) = install::ensure(ws, graph, frozen)?;
    report_ensure(&summary);
    Ok(lock)
}

/// A `--system` value must name a system scpm knows.
fn check_system_name(system: &str) -> Result<(), Failure> {
    match crate::system::known(system) {
        Some(_) => Ok(()),
        None => {
            Err(Failure::Usage(format!("unknown system `{system}`; known systems: {}", crate::system::known_names())))
        }
    }
}

fn report_ensure(summary: &InstallSummary) {
    for w in &summary.warnings {
        eprintln!("warning: {w}");
    }
    for n in &summary.notices {
        eprintln!("note: {n}");
    }
    if !summary.relocked.is_empty() || summary.fetch.downloaded > 0 {
        eprintln!("installed: {} re-locked, {} downloaded", summary.relocked.len(), summary.fetch.downloaded);
    }
}

/// IDE files regenerated after an install go to stderr, so stdout keeps its own format.
fn report_refresh(r: &ide::Refreshed) {
    for w in &r.warnings {
        eprintln!("warning: {w}");
    }
    if !r.written.is_empty() {
        eprintln!("updated {}", r.written.join(", "));
    }
}

/// A sortable, unique id for a run: `<unix millis>-<pid>`.
fn run_id() -> String {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        format!("{millis}-{}", std::process::id())
    })
    .clone()
}

/// CI mode (implied `--frozen`) is `CI=true` (any case: AppVeyor sets `True`).
fn in_ci() -> bool {
    install::in_ci()
}

fn print_install_summary(s: &InstallSummary) -> io::Result<()> {
    for w in &s.warnings {
        eprintln!("warning: {w}");
    }
    for n in s.notes.iter().chain(&s.notices) {
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

/// What `scpm init` wrote, and what to run next (with `-C` when the root isn't the current
/// directory).
fn print_init(report: &crate::init::InitReport, start: &std::path::Path) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let what = match report.kind {
        "workspace" => format!("Created a workspace with module `{}`", report.module),
        _ => format!("Added module `{}` ({})", report.module, report.variants.join(", ")),
    };
    writeln!(out, "{what} in {}", report.root.display())?;
    for f in &report.created {
        writeln!(out, "  + {f}")?;
    }
    for f in &report.updated {
        writeln!(out, "  ~ {f}")?;
    }
    let here = start.canonicalize().is_ok_and(|s| s == report.root);
    let c = if here { String::new() } else { format!("-C {} ", report.root.display()) };
    if report.kind == "workspace" {
        writeln!(out, "\nNext:\n  scpm {c}run {}      compile and run it\n  scpm {c}test", report.module)?;
    } else {
        writeln!(out, "\nNext: depend on it with `{}: workspace:*`, then `scpm {c}compile`", report.module)?;
    }
    Ok(())
}

fn print_json<T: Serialize + ?Sized>(value: &T) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    out.flush()
}
