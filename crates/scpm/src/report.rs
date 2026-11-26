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

/// One test case result (from the test runner, with the file made workspace-relative).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TestCase {
    pub task: String,
    pub suite: String,
    pub name: String,
    /// passed | failed | error | skipped | ignored | canceled | pending
    pub status: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<TestFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TestFailure {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack_trace: Option<String>,
}

impl TestCase {
    pub fn is_failure(&self) -> bool {
        matches!(self.status.as_str(), "failed" | "error")
    }

    pub fn is_skipped(&self) -> bool {
        matches!(self.status.as_str(), "skipped" | "ignored" | "canceled" | "pending")
    }
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
    #[serde(rename = "test.case")]
    TestCase(TestCase),
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

/// Most lines of a task's output kept while it runs (older ones are dropped from the terminal
/// view; the event log keeps everything).
const MAX_BUFFERED_LINES: usize = 5_000;

/// Collects events, writes the event log, and renders output.
pub struct Reporter {
    mode: Mode,
    log: Mutex<Option<File>>,
    log_path: Option<PathBuf>,
    state: Mutex<State>,
    color: bool,
    /// Prefix that makes workspace-relative paths repository-relative for annotations.
    repo_prefix: String,
    /// Print task output immediately rather than per task at its end.
    stream_logs: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct State {
    /// Buffered log lines per running task (capped; see `dropped`).
    logs: std::collections::HashMap<String, Vec<String>>,
    /// How many leading lines were dropped from each task's buffer.
    dropped: std::collections::HashMap<String, usize>,
    diagnostics: Vec<Diagnostic>,
    tests: Vec<TestCase>,
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
            stream_logs: std::sync::atomic::AtomicBool::new(false),
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
            // The framework's own log output already shows failures as they happen; text mode
            // recaps them compactly at the end.
            Event::TestCase(t) => state.tests.push(t.clone()),
            // Output of parallel tasks is buffered per task and printed together when the task
            // ends, so it doesn't interleave.
            Event::Log { task, message, .. } => {
                if self.prints_text() {
                    let line = if self.color { message.clone() } else { plain_text(message) };
                    if self.stream_logs.load(std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("  {line}");
                    } else {
                        let buffered = state.logs.entry(task.clone()).or_default();
                        if buffered.len() == MAX_BUFFERED_LINES {
                            buffered.remove(0);
                            *state.dropped.entry(task.clone()).or_default() += 1;
                        }
                        state.logs.entry(task.clone()).or_default().push(line);
                    }
                }
            }
            Event::TaskEnd { task, status, duration_ms, message } => {
                if let Some(dropped) = state.dropped.remove(task) {
                    eprintln!("  … {dropped} earlier lines of {task} output omitted (see .scpm/runs/latest.ndjson)");
                }
                if let Some(lines) = state.logs.remove(task) {
                    let github = self.mode == Mode::Github;
                    if github {
                        eprintln!("::group::{task} output");
                    }
                    for line in lines {
                        eprintln!("  {line}");
                    }
                    if github {
                        eprintln!("::endgroup::");
                    }
                }
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

    /// How many test cases were recorded.
    pub fn test_count(&self) -> usize {
        self.state.lock().expect("state").tests.len()
    }

    /// Test cases that actually ran (passed or failed), excluding skipped/ignored ones.
    pub fn tests_run(&self) -> usize {
        self.state.lock().expect("state").tests.iter().filter(|t| !t.is_skipped()).count()
    }

    /// Print task output as it arrives instead of buffering it per task (when only one task
    /// produces output, there is nothing to interleave).
    pub fn set_stream_logs(&self, stream: bool) {
        self.stream_logs.store(stream, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether every recorded test passed (or was skipped).
    pub fn tests_ok(&self) -> bool {
        !self.state.lock().expect("state").tests.iter().any(TestCase::is_failure)
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
        let tests = TestCounts::of(&state.tests);
        match self.mode {
            Mode::Text | Mode::Github => {
                if let Some(e) = error {
                    eprintln!("error: {e}");
                }
                let failed =
                    state.tasks.iter().filter(|t| matches!(t.1, TaskStatus::Failed | TaskStatus::Crashed)).count();
                let failures: Vec<&TestCase> = state.tests.iter().filter(|t| t.is_failure()).collect();
                if !failures.is_empty() {
                    eprintln!("\nFailed tests:");
                    for t in failures {
                        eprintln!("{}", render_failure(t));
                    }
                    eprintln!();
                }
                let test_part = if tests.total() > 0 {
                    format!(", tests: {} passed, {} failed, {} skipped", tests.passed, tests.failed, tests.skipped)
                } else {
                    String::new()
                };
                eprintln!(
                    "{command}: {} in {:.1}s ({} tasks, {failed} failed, {errors} errors, {warnings} warnings{test_part})",
                    if ok { "ok" } else { "FAILED" },
                    duration_ms as f64 / 1000.0,
                    state.tasks.len()
                );
            }
            Mode::Json => {}
        }
        match self.mode {
            Mode::Github => {
                let prefix = |f: Option<String>| {
                    f.map(|f| if Path::new(&f).is_absolute() { f } else { format!("{}{f}", self.repo_prefix) })
                };
                let diags: Vec<Diagnostic> = state
                    .diagnostics
                    .iter()
                    .cloned()
                    .map(|mut d| {
                        d.file = prefix(d.file);
                        d
                    })
                    .collect();
                let tests: Vec<TestCase> = state
                    .tests
                    .iter()
                    .cloned()
                    .map(|mut t| {
                        if let Some(f) = t.failure.as_mut() {
                            f.file = prefix(f.file.take());
                        }
                        t
                    })
                    .collect();
                github::emit(&diags, &tests, &state.tasks, command, ok, error)
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
                if tests.total() > 0 || command == "test" {
                    // Failures only (without stack traces); passing cases are in the event log.
                    let failures: Vec<TestCase> = state
                        .tests
                        .iter()
                        .filter(|t| t.is_failure())
                        .cloned()
                        .map(|mut t| {
                            if let Some(f) = t.failure.as_mut() {
                                f.stack_trace = None;
                            }
                            t
                        })
                        .collect();
                    doc["tests"] = serde_json::json!({
                        "passed": tests.passed, "failed": tests.failed, "skipped": tests.skipped, "failures": failures,
                    });
                }
                if let Some(e) = error {
                    doc["error"] = e.into();
                }
                println!("{}", serde_json::to_string_pretty(&doc).expect("json"));
            }
            Mode::Text => {}
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TestCounts {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl TestCounts {
    pub fn of(tests: &[TestCase]) -> TestCounts {
        let mut c = TestCounts::default();
        for t in tests {
            if t.is_failure() {
                c.failed += 1
            } else if t.is_skipped() {
                c.skipped += 1
            } else {
                c.passed += 1
            }
        }
        c
    }

    pub fn total(&self) -> usize {
        self.passed + self.failed + self.skipped
    }
}

fn render_failure(t: &TestCase) -> String {
    let location = t
        .failure
        .as_ref()
        .and_then(|f| {
            f.file.as_ref().map(|file| format!(" at {file}{}", f.line.map(|l| format!(":{l}")).unwrap_or_default()))
        })
        .unwrap_or_default();
    format!("  ✗ {} > {} ({}){location}", t.suite, t.name, t.task)
}

/// Removes ANSI escape sequences and control characters other than newlines and tabs (test
/// frameworks colour their messages; JSON, annotations and XML want plain text).
pub fn plain_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                // CSI: parameters and intermediates, then one final byte in @..~
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC (e.g. window titles): up to BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if !c.is_control() || c == '\n' || c == '\t' {
            out.push(c);
        }
    }
    out
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
        tests: &[TestCase],
        tasks: &[(String, TaskStatus, u64, Option<String>)],
        command: &str,
        ok: bool,
        error: Option<&str>,
    ) {
        let mut out = io::stdout().lock();
        let mut all = diags.to_vec();
        all.extend(tests.iter().filter(|t| t.is_failure()).map(test_failure_diagnostic));
        for line in annotations(&all) {
            let _ = writeln!(out, "{line}");
        }
        if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY")
            && let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path)
        {
            let mut text = summary(&all, tasks, command, ok);
            if let Some(e) = error {
                text.push_str(&format!("**Error:** {}\n\n", escape_md(e)));
            }
            let _ = f.write_all(text.as_bytes());
            let counts = TestCounts::of(tests);
            if counts.total() > 0 {
                let _ = f.write_all(
                    format!(
                        "Tests: {} passed, {} failed, {} skipped\n\n",
                        counts.passed, counts.failed, counts.skipped
                    )
                    .as_bytes(),
                );
            }
        }
    }

    /// A failed test as an error annotation at its source location.
    pub fn test_failure_diagnostic(t: &TestCase) -> Diagnostic {
        let failure = t.failure.as_ref();
        Diagnostic {
            task: format!("{} > {} > {}", t.task, t.suite, t.name),
            severity: Severity::Error,
            file: failure.and_then(|f| f.file.clone()),
            range: failure
                .and_then(|f| f.line)
                .map(|l| Range { start: Position { line: l, col: 1 }, end: Position { line: l, col: 1 } }),
            code: None,
            message: failure.map(|f| f.message.clone()).unwrap_or_else(|| format!("{} {}", t.name, t.status)),
            rendered: None,
            actions: Vec::new(),
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
    fn strips_ansi_and_controls() {
        assert_eq!(super::plain_text("\u{1b}[91m-3\u{1b}[0m\n\u{1b}[1m=> Obtained\u{7}"), "-3\n=> Obtained");
        assert_eq!(super::plain_text("a\u{1b}]0;title\u{7}b\u{1b}]8;;x\u{1b}\\c"), "abc");
    }

    #[test]
    fn events_round_trip() {
        let e =
            Event::TaskEnd { task: "a#compile".into(), status: TaskStatus::UpToDate, duration_ms: 3, message: None };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"t":"task.end","task":"a#compile","status":"upToDate","durationMs":3}"#);
    }
}
