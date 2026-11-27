//! The run event stream and its reporters (design §9).
//!
//! Every run writes `.scpm/runs/<run-id>.ndjson` (and `latest.ndjson`) and feeds the same
//! events to the active reporter: human text, GitHub Actions annotations plus a step summary,
//! or a final JSON document.

use std::fs::{self, File};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub line: u64,
    pub col: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A compiler diagnostic, normalised to workspace-relative paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub task: String,
    pub severity: Severity,
    /// Workspace-relative, `/`-separated; `None` for global diagnostics.
    pub file: Option<String>,
    pub range: Option<Range>,
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Ok,
    UpToDate,
    Failed,
    Skipped,
    Cancelled,
    Crashed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t")]
pub enum Event {
    #[serde(rename = "run.start")]
    RunStart { run: String, command: String },
    #[serde(rename = "task.start")]
    TaskStart { task: String },
    #[serde(rename = "task.end")]
    TaskEnd {
        task: String,
        status: TaskStatus,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    #[serde(rename = "diagnostic")]
    Diagnostic(Diagnostic),
    #[serde(rename = "log")]
    Log { task: String, level: String, message: String },
    #[serde(rename = "run.end")]
    RunEnd {
        ok: bool,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
    },
}

/// Which renderer is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Github,
    Json,
}

impl Mode {
    /// `--json` wins; inside GitHub Actions the GitHub reporter is used; otherwise text.
    pub fn detect(json: bool) -> Mode {
        if json {
            Mode::Json
        } else if std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true") {
            Mode::Github
        } else {
            Mode::Text
        }
    }
}

/// Collects events, writes the event log, and renders output.
pub struct Reporter {
    mode: Mode,
    log: Mutex<Option<File>>,
    log_path: Option<PathBuf>,
    state: Mutex<State>,
    color: bool,
    /// Prefix that makes workspace-relative paths repository-relative for annotations.
    repo_prefix: String,
}

#[derive(Default)]
struct State {
    diagnostics: Vec<Diagnostic>,
    tasks: Vec<(String, TaskStatus, u64, Option<String>)>,
}

impl Reporter {
    pub fn new(mode: Mode, root: &Path, run: &str) -> Reporter {
        let dir = root.join(".scpm/runs");
        let log_path = dir.join(format!("{run}.ndjson"));
        let log = fs::create_dir_all(&dir).ok().and_then(|()| File::create(&log_path).ok());
        Reporter {
            mode,
            log: Mutex::new(log),
            log_path: Some(log_path),
            state: Mutex::new(State::default()),
            color: mode == Mode::Text && io::stderr().is_terminal(),
            repo_prefix: repo_prefix(root),
        }
    }

    /// Text and GitHub modes both print human-readable progress to stderr.
    fn prints_text(&self) -> bool {
        matches!(self.mode, Mode::Text | Mode::Github)
    }

    pub fn emit(&self, event: Event) {
        if let Some(log) = self.log.lock().expect("log").as_mut() {
            let _ = serde_json::to_writer(&mut *log, &event);
            let _ = log.write_all(b"\n");
        }
        let mut state = self.state.lock().expect("state");
        match &event {
            Event::Diagnostic(d) => {
                if self.prints_text() {
                    eprintln!("{}", self.render_text(d));
                }
                state.diagnostics.push(d.clone());
            }
            Event::TaskEnd { task, status, duration_ms, message } => {
                if self.prints_text() {
                    let line = match status {
                        TaskStatus::Ok => format!("✓ {task} ({:.1}s)", *duration_ms as f64 / 1000.0),
                        TaskStatus::UpToDate => format!("✓ {task} (up to date)"),
                        TaskStatus::Failed => format!("✗ {task} failed"),
                        TaskStatus::Skipped => format!("- {task} skipped (an upstream task failed)"),
                        TaskStatus::Cancelled => format!("✗ {task} cancelled"),
                        TaskStatus::Crashed => format!("✗ {task} crashed: {}", message.as_deref().unwrap_or("")),
                    };
                    eprintln!("{line}");
                }
                state.tasks.push((task.clone(), *status, *duration_ms, message.clone()));
            }
            _ => {}
        }
    }

    fn render_text(&self, d: &Diagnostic) -> String {
        if self.color
            && let Some(r) = &d.rendered
        {
            return r.clone();
        }
        let loc = match (&d.file, &d.range) {
            (Some(f), Some(r)) => format!("{f}:{}:{}: ", r.start.line, r.start.col),
            (Some(f), None) => format!("{f}: "),
            _ => String::new(),
        };
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        let code = d.code.as_deref().map(|c| format!("[{c}] ")).unwrap_or_default();
        format!("{loc}{sev}: {code}{}", d.message)
    }

    /// Finishes the run: writes `latest.ndjson`, then the mode-specific summary. An `error`
    /// (a command failure that isn't a task result) is part of the summary, so JSON output stays
    /// a single document.
    pub fn finish(&self, command: &str, ok: bool, duration_ms: u64, error: Option<&str>) {
        self.emit(Event::RunEnd { ok, duration_ms });
        drop(self.log.lock().expect("log").take());
        if let Some(path) = &self.log_path {
            let _ = fs::copy(path, path.with_file_name("latest.ndjson"));
        }
        let state = self.state.lock().expect("state");
        let errors = state.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
        let warnings = state.diagnostics.iter().filter(|d| d.severity == Severity::Warning).count();
        match self.mode {
            Mode::Text | Mode::Github => {
                if let Some(e) = error {
                    eprintln!("error: {e}");
                }
                let failed =
                    state.tasks.iter().filter(|t| matches!(t.1, TaskStatus::Failed | TaskStatus::Crashed)).count();
                eprintln!(
                    "{command}: {} in {:.1}s ({} tasks, {failed} failed, {errors} errors, {warnings} warnings)",
                    if ok { "ok" } else { "FAILED" },
                    duration_ms as f64 / 1000.0,
                    state.tasks.len()
                );
            }
            Mode::Json => {}
        }
        match self.mode {
            Mode::Github => {
                let diags: Vec<Diagnostic> = state
                    .diagnostics
                    .iter()
                    .cloned()
                    .map(|mut d| {
                        d.file = d
                            .file
                            .map(|f| if Path::new(&f).is_absolute() { f } else { format!("{}{f}", self.repo_prefix) });
                        d
                    })
                    .collect();
                github::emit(&diags, &state.tasks, command, ok, error)
            }
            Mode::Json => {
                let tasks: Vec<_> = state
                    .tasks
                    .iter()
                    .map(|(task, status, ms, message)| {
                        let mut t = serde_json::json!({"task": task, "status": status, "durationMs": ms});
                        if let Some(m) = message {
                            t["message"] = m.clone().into();
                        }
                        t
                    })
                    .collect();
                // The compiler's ANSI `rendered` text stays in the event log; JSON output is
                // for machines and agents, so keep it compact.
                let diagnostics: Vec<Diagnostic> =
                    state.diagnostics.iter().cloned().map(|d| Diagnostic { rendered: None, ..d }).collect();
                let mut doc = serde_json::json!({
                    "ok": ok, "command": command, "durationMs": duration_ms,
                    "tasks": tasks, "diagnostics": diagnostics,
                    "errors": errors, "warnings": warnings,
                });
                if let Some(e) = error {
                    doc["error"] = e.into();
                }
                println!("{}", serde_json::to_string_pretty(&doc).expect("json"));
            }
            Mode::Text => {}
        }
    }
}

/// `<path of the workspace inside the repository>/` when running under GitHub Actions with the
/// workspace in a subdirectory of `GITHUB_WORKSPACE`, so annotations attach to the right file.
fn repo_prefix(root: &Path) -> String {
    let Some(repo) = std::env::var_os("GITHUB_WORKSPACE") else { return String::new() };
    let repo = Path::new(&repo).canonicalize().unwrap_or_else(|_| PathBuf::from(&repo));
    match root.strip_prefix(&repo) {
        Ok(rel) if !rel.as_os_str().is_empty() => format!("{}/", rel.to_string_lossy().replace('\\', "/")),
        _ => String::new(),
    }
}

/// GitHub Actions output: workflow-command annotations (capped, ranked) and a step summary
/// with every diagnostic (design §9.2).
pub mod github {
    use super::*;

    /// GitHub shows at most 10 annotations per type per step and drops the rest silently.
    pub const PER_TYPE_CAP: usize = 10;

    pub fn emit(
        diags: &[Diagnostic],
        tasks: &[(String, TaskStatus, u64, Option<String>)],
        command: &str,
        ok: bool,
        error: Option<&str>,
    ) {
        let mut out = io::stdout().lock();
        for line in annotations(diags) {
            let _ = writeln!(out, "{line}");
        }
        if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY")
            && let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path)
        {
            let mut text = summary(diags, tasks, command, ok);
            if let Some(e) = error {
                text.push_str(&format!("**Error:** {}\n\n", escape_md(e)));
            }
            let _ = f.write_all(text.as_bytes());
        }
    }

    /// Errors first, then warnings, each capped; files in the same order as reported.
    pub fn annotations(diags: &[Diagnostic]) -> Vec<String> {
        let mut out = Vec::new();
        for (severity, kind) in [(Severity::Error, "error"), (Severity::Warning, "warning")] {
            let matching: Vec<&Diagnostic> = diags.iter().filter(|d| d.severity == severity).collect();
            for d in matching.iter().take(PER_TYPE_CAP) {
                let mut props = Vec::new();
                if let Some(f) = &d.file {
                    props.push(format!("file={}", escape_property(f)));
                }
                if let Some(r) = &d.range {
                    props.push(format!("line={}", r.start.line));
                    props.push(format!("col={}", r.start.col));
                    props.push(format!("endLine={}", r.end.line));
                    props.push(format!("endColumn={}", r.end.col));
                }
                let title = match &d.code {
                    Some(c) => format!("{} [{c}]", d.task),
                    None => d.task.clone(),
                };
                props.push(format!("title={}", escape_property(&title)));
                out.push(format!("::{kind} {}::{}", props.join(","), escape_data(&d.message)));
            }
            if matching.len() > PER_TYPE_CAP {
                out.push(format!(
                    "::notice title=scpm::{} more {kind}s were not annotated (GitHub's limit); see the job summary",
                    matching.len() - PER_TYPE_CAP
                ));
            }
        }
        out
    }

    pub fn summary(
        diags: &[Diagnostic],
        tasks: &[(String, TaskStatus, u64, Option<String>)],
        command: &str,
        ok: bool,
    ) -> String {
        let mut s = format!("### scpm {command}: {}\n\n", if ok { "✅ passed" } else { "❌ failed" });
        let failed: Vec<&str> = tasks
            .iter()
            .filter(|t| matches!(t.1, TaskStatus::Failed | TaskStatus::Crashed))
            .map(|t| t.0.as_str())
            .collect();
        s.push_str(&format!("{} tasks, {} failed", tasks.len(), failed.len()));
        if !failed.is_empty() {
            s.push_str(&format!(": `{}`", failed.join("`, `")));
        }
        s.push_str("\n\n");
        if !diags.is_empty() {
            s.push_str("| | Location | Task | Message |\n|---|---|---|---|\n");
            // GitHub accepts up to 1 MiB of summary per step; stay well below it.
            for d in diags.iter().take(SUMMARY_ROWS) {
                let icon = match d.severity {
                    Severity::Error => "❌",
                    Severity::Warning => "⚠️",
                    Severity::Info => "ℹ️",
                };
                let loc = match (&d.file, &d.range) {
                    (Some(f), Some(r)) => format!("`{f}:{}:{}`", r.start.line, r.start.col),
                    (Some(f), None) => format!("`{f}`"),
                    _ => String::new(),
                };
                let msg = escape_md(d.message.lines().next().unwrap_or(""));
                s.push_str(&format!("| {icon} | {loc} | `{}` | {msg} |\n", d.task));
            }
            if diags.len() > SUMMARY_ROWS {
                s.push_str(&format!("\n…and {} more (see the job log)\n", diags.len() - SUMMARY_ROWS));
            }
            s.push('\n');
        }
        s
    }

    /// Rows listed in the step summary.
    pub const SUMMARY_ROWS: usize = 500;

    /// Plain text inside a Markdown table cell (no HTML, no column breaks).
    pub fn escape_md(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('|', "\\|")
    }

    /// Escaping for the message part of a workflow command.
    pub fn escape_data(s: &str) -> String {
        s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
    }

    /// Escaping for property values (`file=…,title=…`).
    pub fn escape_property(s: &str) -> String {
        escape_data(s).replace(':', "%3A").replace(',', "%2C")
    }
}

#[cfg(test)]
mod tests {
    use super::github::*;
    use super::*;

    fn diag(sev: Severity, n: u64) -> Diagnostic {
        Diagnostic {
            task: "core@jvm-3#compile".into(),
            severity: sev,
            file: Some("modules/core/src/main/scala/A.scala".into()),
            range: Some(Range { start: Position { line: n, col: 3 }, end: Position { line: n, col: 9 } }),
            code: Some("E007".into()),
            message: "Found: Int\nRequired: String".into(),
            rendered: None,
            actions: vec![],
        }
    }

    #[test]
    fn annotations_are_escaped_and_capped() {
        let mut diags: Vec<Diagnostic> = (1..=12).map(|n| diag(Severity::Error, n)).collect();
        diags.push(diag(Severity::Warning, 99));
        let lines = annotations(&diags);
        assert_eq!(
            lines[0],
            "::error file=modules/core/src/main/scala/A.scala,line=1,col=3,endLine=1,endColumn=9,title=core@jvm-3#compile [E007]::Found: Int%0ARequired: String"
        );
        assert_eq!(lines.iter().filter(|l| l.starts_with("::error")).count(), PER_TYPE_CAP);
        assert!(lines[PER_TYPE_CAP].starts_with("::notice title=scpm::2 more errors"));
        assert!(lines.last().unwrap().starts_with("::warning"));
    }

    #[test]
    fn summary_lists_every_diagnostic() {
        let diags: Vec<Diagnostic> = (1..=12).map(|n| diag(Severity::Error, n)).collect();
        let s = summary(&diags, &[("core@jvm-3#compile".into(), TaskStatus::Failed, 10, None)], "compile", false);
        assert!(s.contains("❌ failed"));
        assert_eq!(s.matches("| ❌ |").count(), 12);
        assert_eq!(escape_md("List<String> | a & b"), "List&lt;String&gt; \\| a &amp; b");
    }

    #[test]
    fn events_round_trip() {
        let e =
            Event::TaskEnd { task: "a#compile".into(), status: TaskStatus::UpToDate, duration_ms: 3, message: None };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"t":"task.end","task":"a#compile","status":"upToDate","durationMs":3}"#);
    }
}
