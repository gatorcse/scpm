//! `scpm mcp`: a Model Context Protocol server over stdio (design §9.3).
//!
//! Every tool is the same code path as a CLI command, returning compact JSON. The server keeps
//! one compile worker warm across calls. Messages are read on the main thread: `ping`,
//! `initialize` and listings are answered at once, tool calls run on their own threads, and
//! build calls (anything that may compile, test or write the lock) run one at a time and can
//! be cancelled with `notifications/cancelled`.

use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};

use crate::compile::{self, Toolchain, WorkerHandle, WorkerSlot};
use crate::filter;
use crate::graph::Graph;
use crate::install::{self, ClasspathKind};
use crate::report::{Event, Mode, Reporter, TaskStatus};
use crate::testing;
use crate::workspace::{self, Workspace};

/// Newest first; an unknown requested version gets the newest.
const SUPPORTED_PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_MAX_ITEMS: usize = 30;
/// Longest message text kept per diagnostic or test failure.
const MAX_MESSAGE: usize = 1500;

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const RESOURCE_NOT_FOUND: i64 = -32002;

pub struct Server {
    start: PathBuf,
    slot: Arc<WorkerSlot>,
    out: Arc<Mutex<io::Stdout>>,
    /// Serializes calls that build or write the lock.
    build_lock: Mutex<()>,
    /// The build call in progress: its request id and cancellation flag.
    current: Mutex<Option<(Value, Arc<AtomicBool>)>>,
    /// Requests cancelled while waiting for the build lock.
    cancelled_ids: Mutex<std::collections::HashSet<String>>,
    /// Set when stdin closes: build calls still waiting for the lock never start.
    closing: AtomicBool,
}

/// A request's progress token and its cancellation flag.
type Progress = Option<(Value, Arc<AtomicBool>)>;

/// Locks a mutex, recovering it if a panicking thread poisoned it.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A tool failure, reported as `isError` content (the model can correct it).
struct ToolError {
    message: String,
    /// Partial results (e.g. the summary of a build that hit an error).
    partial: Option<Value>,
}

impl<E: std::fmt::Display> From<E> for ToolError {
    fn from(e: E) -> Self {
        ToolError { message: e.to_string(), partial: None }
    }
}

fn tool_error(message: impl Into<String>) -> ToolError {
    ToolError { message: message.into(), partial: None }
}

impl Server {
    pub fn new(start: PathBuf) -> Server {
        Server {
            start,
            slot: Arc::new(WorkerSlot::default()),
            out: Arc::new(Mutex::new(io::stdout())),
            build_lock: Mutex::new(()),
            current: Mutex::new(None),
            cancelled_ids: Mutex::new(std::collections::HashSet::new()),
            closing: AtomicBool::new(false),
        }
    }

    fn send(&self, msg: &Value) {
        let mut out = self.out.lock().expect("stdout");
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }

    /// Serves messages, one per line, until stdin closes.
    pub fn serve(self) -> io::Result<()> {
        let server = Arc::new(self);
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut buf = Vec::new();
        std::thread::scope(|scope| -> io::Result<()> {
            loop {
                buf.clear();
                if reader.read_until(b'\n', &mut buf)? == 0 {
                    break;
                }
                let line = match std::str::from_utf8(&buf) {
                    Ok(l) if l.trim().is_empty() => continue,
                    Ok(l) => l,
                    Err(_) => {
                        server.send(&error_reply(Value::Null, PARSE_ERROR, "invalid UTF-8"));
                        continue;
                    }
                };
                let msg = match serde_json::from_str::<Value>(line) {
                    Ok(m) => m,
                    Err(e) => {
                        server.send(&error_reply(Value::Null, PARSE_ERROR, &format!("parse error: {e}")));
                        continue;
                    }
                };
                let server = server.clone();
                if is_tool_call(&msg) {
                    scope.spawn(move || {
                        if let Some(reply) = server.handle(&msg) {
                            server.send(&reply);
                        }
                    });
                } else if let Some(reply) = server.handle(&msg) {
                    server.send(&reply);
                }
            }
            // stdin closed: the client is gone; stop in-flight work instead of finishing it, and
            // skip the build calls queued behind it.
            server.closing.store(true, Ordering::SeqCst);
            server.cancel_current();
            Ok(())
        })?;
        server.slot.shutdown();
        Ok(())
    }

    /// Handles one message; returns the reply, if any.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let Some(obj) = msg.as_object() else {
            return Some(error_reply(Value::Null, INVALID_REQUEST, "expected a JSON-RPC object"));
        };
        // Responses to our (nonexistent) requests are never answered.
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            return Some(error_reply(
                obj.get("id").cloned().unwrap_or(Value::Null),
                INVALID_REQUEST,
                "missing `method`",
            ));
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = obj.get("id").cloned() else {
            if method == "notifications/cancelled" {
                self.cancel(&params["requestId"]);
            }
            return None;
        };
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => return self.call(id, &params),
            "resources/list" => Ok(json!({ "resources": [
                {"uri": "scpm://workspace", "name": "workspace", "description": "Modules and variants", "mimeType": "application/json"},
                {"uri": "scpm://run/latest", "name": "latest run", "description": "Event log of the last run (NDJSON)", "mimeType": "application/x-ndjson"},
            ]})),
            "resources/read" => self.read_resource(&params),
            _ => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, m)) => error_reply(id, code, &m),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params["protocolVersion"].as_str().unwrap_or_default();
        let version = SUPPORTED_PROTOCOLS.iter().find(|v| **v == requested).copied().unwrap_or(SUPPORTED_PROTOCOLS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}, "resources": {"listChanged": false}},
            "serverInfo": {"name": "scpm", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "scpm builds Scala workspaces. Use `compile` and `test` after edits; results are compact and \
                             list only problems. Targets accept module names, variant ids (core@jvm-3) and pnpm-style \
                             selectors (core..., ...core, [origin/main]). `diagnostics` returns the current diagnostics \
                             of every module without rebuilding.",
        })
    }

    /// Cancels the build call in progress, if any.
    fn cancel_current(&self) {
        // Bind first: a guard held in an `if let` scrutinee would live through `cancel`, which
        // locks `current` again (a self-deadlock).
        let running = lock(&self.current).clone();
        if let Some((id, _)) = running {
            self.cancel(&id);
        }
    }

    fn cancel(&self, request_id: &Value) {
        let current = lock(&self.current);
        match current.as_ref() {
            Some((id, flag)) if id == request_id => {
                flag.store(true, Ordering::SeqCst);
                self.slot.cancel("was stopped (request cancelled)");
                testing::cancel_all("was stopped (request cancelled)");
            }
            // Not running yet (waiting for the build lock): skip it when its turn comes.
            _ => {
                lock(&self.cancelled_ids).insert(request_id.to_string());
            }
        }
    }

    fn call(&self, id: Value, params: &Value) -> Option<Value> {
        let name = params["name"].as_str().unwrap_or_default();
        let Some(tool) = tools().as_array().and_then(|t| t.iter().find(|t| t["name"] == name).cloned()) else {
            return Some(error_reply(id, INVALID_PARAMS, &format!("unknown tool `{name}`")));
        };
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if let Err(message) = validate(&tool["inputSchema"], &args) {
            return Some(tool_result(id, Err(tool_error(message))));
        }
        let progress = params.pointer("/_meta/progressToken").cloned();
        let builds = !tool["annotations"]["readOnlyHint"].as_bool().unwrap_or(false);
        let cancelled = Arc::new(AtomicBool::new(false));
        let _guard = builds.then(|| {
            let guard = lock(&self.build_lock);
            // Reset before publishing `current`: a cancel landing in between must not be undone.
            self.slot.reset_cancel();
            testing::reset_cancel();
            *lock(&self.current) = Some((id.clone(), cancelled.clone()));
            guard
        });
        // `closing` is checked after publishing `current`: either this sees it, or the EOF
        // handler's `cancel_current` sees this call.
        let closing = builds && self.closing.load(Ordering::SeqCst);
        if lock(&self.cancelled_ids).remove(&id.to_string()) || closing {
            // Only a build call published itself; a read-only one must not clear a running build.
            if builds {
                *lock(&self.current) = None;
            }
            return None;
        }
        // A panic in a tool must not leave the client waiting (or poison the build lock).
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_tool(name, &args, progress, &cancelled)))
                .unwrap_or_else(|_| Err(tool_error(format!("internal error in `{name}`"))));
        if builds {
            *lock(&self.current) = None;
        }
        // A cancelled request gets no response.
        (!cancelled.load(Ordering::SeqCst)).then(|| tool_result(id, result))
    }

    fn run_tool(
        &self,
        name: &str,
        args: &Value,
        progress: Option<Value>,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<Value, ToolError> {
        let args = args.clone();
        let progress = progress.filter(|_| !cancelled.load(Ordering::SeqCst)).map(|t| (t, cancelled.clone()));
        match name {
            "ls" => self.ls(&args),
            "graph" => self.graph(&args),
            "config" => self.config(&args),
            "install" => self.install(&args),
            "compile" => self.compile(&args, progress),
            "test" => self.test(&args, progress),
            "diagnostics" => self.diagnostics(&args),
            "why" => self.why(&args),
            "classpath" => self.classpath(&args),
            _ => Err(tool_error(format!("unknown tool `{name}`"))),
        }
    }

    fn load(&self) -> Result<(Workspace, Graph), ToolError> {
        let ws = workspace::load(&self.start)?;
        let graph = Graph::build(&ws)?;
        Ok((ws, graph))
    }

    fn select(&self, ws: &Workspace, graph: &Graph, args: &Value) -> Result<BTreeSet<String>, ToolError> {
        let selectors: Vec<String> =
            args["targets"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
        let here = self.start.canonicalize()?;
        Ok(filter::select(ws, graph, &selectors, &here)?)
    }

    fn ls(&self, args: &Value) -> Result<Value, ToolError> {
        let (ws, graph) = self.load()?;
        let selected = self.select(&ws, &graph, args)?;
        let variants: Vec<Value> = ws
            .modules
            .iter()
            .flat_map(|m| {
                m.variants.iter().filter(|v| selected.contains(&v.id)).map(move |v| json!({"id": v.id, "dir": m.dir}))
            })
            .collect();
        Ok(json!({ "variants": variants }))
    }

    fn graph(&self, args: &Value) -> Result<Value, ToolError> {
        let (ws, graph) = self.load()?;
        let selected = self.select(&ws, &graph, args)?;
        let order: Vec<&String> = graph.order.iter().filter(|id| selected.contains(*id)).collect();
        let edges: Vec<_> =
            graph.edges.iter().filter(|e| selected.contains(&e.from) && selected.contains(&e.to)).collect();
        Ok(json!({"order": order, "edges": edges}))
    }

    fn config(&self, args: &Value) -> Result<Value, ToolError> {
        let (ws, graph) = self.load()?;
        let selected = self.select(&ws, &graph, args)?;
        Ok(json!({ "variants": ws.variants().filter(|v| selected.contains(&v.id)).collect::<Vec<_>>() }))
    }

    fn install(&self, args: &Value) -> Result<Value, ToolError> {
        let (ws, graph) = self.load()?;
        let frozen = args["frozen"].as_bool().unwrap_or(false);
        let summary = install::install(
            &ws,
            &graph,
            install::InstallOptions { frozen: frozen || install::in_ci(), verify: false },
        )?;
        Ok(serde_json::to_value(summary)?)
    }

    /// Runs `body` with a quiet reporter and the shared worker, returning the compact summary.
    fn build<F>(&self, args: &Value, command: &str, progress: Progress, body: F) -> Result<Value, ToolError>
    where
        F: FnOnce(&Toolchain, &Graph, &BTreeSet<String>, &WorkerHandle, &Arc<Reporter>) -> Result<bool, String>,
    {
        let (ws, graph) = self.load()?;
        let selected = self.select(&ws, &graph, args)?;
        let (lock, _) = install::ensure(&ws, &graph, false)?;
        let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let run = format!("{millis}-{}-mcp", std::process::id());
        let reporter = Arc::new(Reporter::new(Mode::Quiet, &ws.root, &run));
        if let Some((token, cancelled)) = progress {
            let out = self.out.clone();
            let done = AtomicU64::new(0);
            reporter.set_listener(move |event| {
                // No progress for a cancelled request: the client has already moved on.
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                if let Event::TaskEnd { task, status, .. } = event {
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let status = serde_json::to_value(status).unwrap_or_default();
                    let msg = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {
                        "progressToken": token, "progress": n, "message": format!("{task}: {}", status.as_str().unwrap_or("")),
                    }});
                    let mut out = out.lock().expect("stdout");
                    let _ = writeln!(out, "{msg}");
                    let _ = out.flush();
                }
            });
        }
        reporter.emit(Event::RunStart { run, command: command.into() });
        let started = Instant::now();
        // Like the CLI, hold the workspace lock while building, so another scpm command (or
        // server) doesn't write the same outputs; released before the next call.
        let result = compile::workspace_lock(&ws.root).map_err(|e| e.to_string()).and_then(|_guard| {
            let tc = Toolchain::new(&ws, &lock).map_err(|e| e.to_string())?;
            let handle = WorkerHandle::shared(&tc, reporter.clone(), self.slot.clone(), false);
            let result = body(&tc, &graph, &selected, &handle, &reporter);
            handle.shutdown();
            result
        });
        let (ok, error) = match &result {
            Ok(ok) => (*ok, None),
            Err(e) => (false, Some(e.as_str())),
        };
        let doc = compact(reporter.finish(command, ok, started.elapsed().as_millis() as u64, error), max_items(args));
        match result {
            Ok(_) => Ok(doc),
            Err(message) => Err(ToolError { message, partial: Some(doc) }),
        }
    }

    fn compile(&self, args: &Value, progress: Progress) -> Result<Value, ToolError> {
        let tests = args["tests"].as_bool().unwrap_or(false);
        self.build(args, "compile", progress, |tc, graph, selected, handle, _| {
            compile::compile(tc, graph, selected, compile::Scope { tests, runtime: false }, handle)
                .map_err(|e| e.to_string())
        })
    }

    fn test(&self, args: &Value, progress: Progress) -> Result<Value, ToolError> {
        let strings = |k: &str| -> Vec<String> {
            args[k].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect()
        };
        let filtered = !strings("suites").is_empty() || !strings("tests").is_empty();
        let opts = testing::TestOptions {
            filters: strings("suites"),
            test_names: strings("tests"),
            timeout: Some(std::time::Duration::from_secs(args["timeoutSeconds"].as_u64().unwrap_or(1800)))
                .filter(|d| !d.is_zero()),
        };
        self.build(args, "test", progress, |tc, graph, selected, handle, reporter| {
            // As in the CLI: clear every selected variant's report first, so a run that fails
            // before its tests start can't leave a stale passing one.
            let all: Vec<_> = selected.iter().filter_map(|id| tc.ws.variant(id)).collect();
            testing::clear_reports(tc, &all);
            let variants = testing::with_tests(tc, selected).map_err(|e| e.to_string())?;
            let runner_cp = if variants.is_empty() {
                Vec::new()
            } else {
                testing::runner_classpath(tc).map_err(|e| e.to_string())?
            };
            let scope = compile::Scope { tests: true, runtime: true };
            if !compile::compile(tc, graph, selected, scope, handle).map_err(|e| e.to_string())? {
                return Ok(false);
            }
            // As in the CLI: nothing to run is fine unless filters asked for tests.
            if variants.is_empty() {
                return if filtered { Err("no tests matched `suites` / `tests`".into()) } else { Ok(true) };
            }
            let ok = testing::run_tests(tc, &variants, &runner_cp, &opts, reporter);
            if filtered && reporter.tests_run() == 0 {
                return Err("no tests matched `suites` / `tests`".into());
            }
            Ok(ok)
        })
    }

    /// Current diagnostics of every compiled task (persisted per task), ranked and paged.
    fn diagnostics(&self, args: &Value) -> Result<Value, ToolError> {
        let ws = workspace::load(&self.start)?;
        let file = args["file"].as_str();
        let mut diags: Vec<Value> = Vec::new();
        for entry in std::fs::read_dir(ws.root.join(".scpm/out")).into_iter().flatten().flatten() {
            for name in ["fingerprint.diagnostics.json", "test-fingerprint.diagnostics.json"] {
                let Ok(bytes) = std::fs::read(entry.path().join(name)) else { continue };
                let Ok(list) = serde_json::from_slice::<Vec<Value>>(&bytes) else { continue };
                diags.extend(list.into_iter().filter(|d| {
                    file.is_none_or(|f| d["file"].as_str().is_some_and(|df| df == f || df.ends_with(&format!("/{f}"))))
                }));
            }
        }
        let total = diags.len();
        let mut diags = rank_diagnostics(diags);
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let max = max_items(args);
        let with_actions = args["withActions"].as_bool().unwrap_or(false);
        let page: Vec<Value> = diags
            .drain(..)
            .skip(offset)
            .take(max)
            .map(|mut d| {
                if let Some(o) = d.as_object_mut() {
                    o.remove("rendered");
                    if !with_actions {
                        o.remove("actions");
                    }
                }
                d
            })
            .collect();
        Ok(json!({"diagnostics": page, "total": total, "nextOffset": (offset + max < total).then_some(offset + max)}))
    }

    fn why(&self, args: &Value) -> Result<Value, ToolError> {
        let (ws, graph) = self.load()?;
        let artifact = args["artifact"].as_str().unwrap_or_default();
        let selected: Vec<String> = self.select(&ws, &graph, args)?.into_iter().collect();
        let (lock, _) = install::ensure(&ws, &graph, false)?;
        Ok(serde_json::to_value(crate::why::why(&ws, &graph, &lock, artifact, &selected)?)?)
    }

    fn classpath(&self, args: &Value) -> Result<Value, ToolError> {
        let (ws, graph) = self.load()?;
        let target = args["target"].as_str().unwrap_or_default();
        let selected = filter::select(&ws, &graph, &[target.to_string()], &self.start.canonicalize()?)?;
        let [id] = selected.iter().collect::<Vec<_>>()[..] else {
            return Err(tool_error(format!("`{target}` matches {} variants; name one", selected.len())));
        };
        let kind = match args["kind"].as_str().unwrap_or("compile") {
            "runtime" => ClasspathKind::Runtime,
            "test-compile" => ClasspathKind::TestCompile,
            "test-runtime" => ClasspathKind::TestRuntime,
            _ => ClasspathKind::Compile,
        };
        let (lock, _) = install::ensure(&ws, &graph, false)?;
        Ok(json!({ "paths": install::classpath_files(&ws, &lock, id, kind)? }))
    }

    fn read_resource(&self, params: &Value) -> Result<Value, (i64, String)> {
        let uri = params["uri"].as_str().unwrap_or_default();
        let internal = |e: ToolError| (INTERNAL_ERROR, e.message);
        let (text, mime) = match uri {
            "scpm://workspace" => (self.ls(&json!({})).map_err(internal)?.to_string(), "application/json"),
            "scpm://run/latest" => {
                let ws = workspace::load(&self.start).map_err(|e| (INTERNAL_ERROR, e.to_string()))?;
                let log: &Path = &ws.root.join(".scpm/runs/latest.ndjson");
                (std::fs::read_to_string(log).unwrap_or_default(), "application/x-ndjson")
            }
            other => return Err((RESOURCE_NOT_FOUND, format!("unknown resource `{other}`"))),
        };
        Ok(json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}))
    }
}

fn is_tool_call(msg: &Value) -> bool {
    msg.get("id").is_some() && msg["method"] == "tools/call"
}

fn max_items(args: &Value) -> usize {
    args["maxItems"].as_u64().map_or(DEFAULT_MAX_ITEMS, |n| n.max(1) as usize)
}

fn tool_result(id: Value, result: Result<Value, ToolError>) -> Value {
    let result = match result {
        Ok(value) => json!({
            "content": [{"type": "text", "text": value.to_string()}],
            "structuredContent": value,
            "isError": false,
        }),
        Err(ToolError { message, partial }) => {
            let mut structured = partial.unwrap_or_else(|| json!({}));
            structured["error"] = message.clone().into();
            json!({
                "content": [{"type": "text", "text": structured.to_string()}],
                "structuredContent": structured,
                "isError": true,
            })
        }
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Checks `args` against the subset of JSON Schema the tool schemas use: object with known
/// properties (no others), `required`, and property types string/boolean/integer/array of
/// strings.
fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(obj) = args.as_object() else { return Err("`arguments` must be an object".into()) };
    let props = schema["properties"].as_object().cloned().unwrap_or_default();
    for (key, value) in obj {
        let Some(prop) = props.get(key) else {
            let known: Vec<&String> = props.keys().collect();
            return Err(format!(
                "unknown argument `{key}`; expected one of: {}",
                known.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
            ));
        };
        let ok = match prop["type"].as_str() {
            Some("string") => {
                value.as_str().is_some_and(|s| prop["enum"].as_array().is_none_or(|e| e.iter().any(|v| v == s)))
            }
            Some("boolean") => value.is_boolean(),
            Some("integer") => value.as_u64().is_some(),
            Some("array") => value.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            _ => true,
        };
        if !ok {
            let expected = match prop["type"].as_str() {
                Some("array") => "an array of strings".to_string(),
                Some("string") if prop["enum"].is_array() => format!("one of {}", prop["enum"]),
                Some(t) => format!("a {t}"),
                None => "a value".into(),
            };
            return Err(format!("`{key}` must be {expected}"));
        }
    }
    for req in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !obj.contains_key(req) {
            return Err(format!("`{req}` is required"));
        }
    }
    Ok(())
}

/// Errors before warnings before info; messages shortened.
fn rank_diagnostics(mut diags: Vec<Value>) -> Vec<Value> {
    let rank = |d: &Value| match d["severity"].as_str() {
        Some("error") => 0,
        Some("warning") => 1,
        _ => 2,
    };
    diags.sort_by_key(rank);
    for d in &mut diags {
        if let Some(m) = d["message"].as_str().filter(|m| m.len() > MAX_MESSAGE) {
            let cut =
                m.char_indices().take_while(|(i, _)| *i < MAX_MESSAGE).last().map_or(0, |(i, c)| i + c.len_utf8());
            d["message"] = format!("{}…", &m[..cut]).into();
        }
    }
    diags
}

/// Keeps results token-frugal: tasks summarised to counts plus problems, ranked and capped
/// lists with counts of what was left out (design §9.3).
fn compact(mut doc: Value, max: usize) -> Value {
    if let Some(tasks) = doc.get_mut("tasks").and_then(Value::as_array_mut) {
        let count = |s: TaskStatus| {
            let s = serde_json::to_value(s).unwrap_or_default();
            tasks.iter().filter(|t| t["status"] == s).count()
        };
        let (total, up_to_date, ok, skipped) =
            (tasks.len(), count(TaskStatus::UpToDate), count(TaskStatus::Ok), count(TaskStatus::Skipped));
        let mut problems: Vec<Value> =
            tasks.drain(..).filter(|t| !matches!(t["status"].as_str(), Some("upToDate" | "ok" | "skipped"))).collect();
        let more = problems.len().saturating_sub(max);
        problems.truncate(max);
        doc["tasks"] =
            json!({"total": total, "ok": ok, "upToDate": up_to_date, "skipped": skipped, "problems": problems});
        if more > 0 {
            doc["tasks"]["moreProblems"] = more.into();
        }
    }
    if let Some(items) = doc.get_mut("diagnostics").and_then(Value::as_array_mut) {
        // Quick-fix actions are bulky; `diagnostics` with `withActions` returns them.
        for d in items.iter_mut() {
            if let Some(o) = d.as_object_mut() {
                o.remove("actions");
            }
        }
        let ranked = rank_diagnostics(std::mem::take(items));
        let more = ranked.len().saturating_sub(max);
        doc["diagnostics"] = Value::Array(ranked.into_iter().take(max).collect());
        if more > 0 {
            doc["moreDiagnostics"] = more.into();
        }
    }
    if let Some(failures) = doc.pointer_mut("/tests/failures").and_then(Value::as_array_mut) {
        for f in failures.iter_mut() {
            if let Some(m) = f.pointer("/failure/message").and_then(Value::as_str).filter(|m| m.len() > MAX_MESSAGE) {
                let cut =
                    m.char_indices().take_while(|(i, _)| *i < MAX_MESSAGE).last().map_or(0, |(i, c)| i + c.len_utf8());
                f["failure"]["message"] = format!("{}…", &m[..cut]).into();
            }
        }
        let extra = failures.len().saturating_sub(max);
        failures.truncate(max);
        if extra > 0 {
            doc["tests"]["moreFailures"] = extra.into();
        }
    }
    doc
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tools() -> Value {
    let targets = json!({
        "type": "array", "items": {"type": "string"},
        "description": "Module names, variant ids (core@jvm-3) or pnpm-style selectors; default: everything",
    });
    let max = json!({"type": "integer", "minimum": 1, "description": "Cap on listed items (default 30)"});
    let read_only = json!({"readOnlyHint": true, "idempotentHint": true, "openWorldHint": false});
    let build =
        json!({"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false});
    let schema = |props: Value, required: Value| json!({"type": "object", "properties": props, "required": required, "additionalProperties": false});
    json!([
        {"name": "compile", "description": "Compile modules (and their dependencies). Unchanged modules are skipped. Returns problems only.",
         "inputSchema": schema(json!({"targets": targets, "tests": {"type": "boolean", "description": "Also compile test sources"}, "maxItems": max}), json!([])),
         "annotations": build},
        {"name": "test", "description": "Compile and run tests. Returns counts and failures with file:line.",
         "inputSchema": schema(json!({"targets": targets, "suites": {"type": "array", "items": {"type": "string"}, "description": "Suite name globs"}, "tests": {"type": "array", "items": {"type": "string"}, "description": "Test case names"}, "timeoutSeconds": {"type": "integer", "description": "Per-module test timeout; 0 = none (default 1800)"}, "maxItems": max}), json!([])),
         "annotations": build},
        {"name": "diagnostics", "description": "Current compiler diagnostics of every compiled module, errors first, without rebuilding.",
         "inputSchema": schema(json!({"file": {"type": "string", "description": "Only this file (path or trailing path segments)"}, "offset": {"type": "integer", "description": "Skip this many (paging)"}, "withActions": {"type": "boolean", "description": "Include compiler quick-fix edits"}, "maxItems": max}), json!([])),
         "annotations": read_only},
        {"name": "ls", "description": "List modules and their variants.",
         "inputSchema": schema(json!({"targets": targets}), json!([])), "annotations": read_only},
        {"name": "graph", "description": "Module dependency graph in build order.",
         "inputSchema": schema(json!({"targets": targets}), json!([])), "annotations": read_only},
        {"name": "config", "description": "Fully resolved configuration of modules or variants (name targets to keep it short).",
         "inputSchema": schema(json!({"targets": targets}), json!([])), "annotations": read_only},
        {"name": "install", "description": "Lock dependencies and download them.",
         "inputSchema": schema(json!({"frozen": {"type": "boolean"}}), json!([])), "annotations": build},
        {"name": "why", "description": "Explain why an artifact (org::name, org:name_3 or name) is on the classpath.",
         "inputSchema": schema(json!({"artifact": {"type": "string"}, "targets": targets}), json!(["artifact"])), "annotations": build},
        {"name": "classpath", "description": "Classpath of one variant as absolute paths.",
         "inputSchema": schema(json!({"target": {"type": "string"}, "kind": {"type": "string", "enum": ["compile", "runtime", "test-compile", "test-runtime"]}}), json!(["target"])),
         "annotations": build},
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_ranks_counts_and_caps() {
        let doc = json!({
            "ok": false,
            "tasks": [{"task": "a", "status": "upToDate"}, {"task": "b", "status": "ok"}, {"task": "c", "status": "failed"}, {"task": "d", "status": "skipped"}],
            "diagnostics": [{"severity": "warning", "message": "w"}, {"severity": "error", "message": "e"}, {"severity": "warning", "message": "w2"}],
        });
        let c = compact(doc, 2);
        assert_eq!(
            c["tasks"],
            json!({"total": 4, "ok": 1, "upToDate": 1, "skipped": 1, "problems": [{"task": "c", "status": "failed"}]})
        );
        assert_eq!(c["diagnostics"][0]["severity"], "error", "errors first");
        assert_eq!(c["moreDiagnostics"], 1);
    }

    #[test]
    fn cancelling_on_stdin_eof_does_not_deadlock() {
        let server = Arc::new(Server::new(PathBuf::from(".")));
        let flag = Arc::new(AtomicBool::new(false));
        *lock(&server.current) = Some((json!(7), flag.clone()));
        let (tx, rx) = std::sync::mpsc::channel();
        let s = server.clone();
        std::thread::spawn(move || {
            s.cancel_current();
            let _ = tx.send(());
        });
        let finished = rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok();
        testing::reset_cancel();
        assert!(finished, "cancel_current deadlocked");
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn queued_build_calls_do_not_start_after_stdin_closes() {
        let server = Server::new(PathBuf::from("/nonexistent-scpm-workspace"));
        server.closing.store(true, Ordering::SeqCst);
        let call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "compile"}});
        assert!(server.handle(&call).is_none(), "a queued build ran after stdin closed");
        assert!(lock(&server.current).is_none());
    }

    #[test]
    fn a_cancelled_read_only_call_keeps_the_running_build_current() {
        let server = Server::new(PathBuf::from("/nonexistent-scpm-workspace"));
        *lock(&server.current) = Some((json!(7), Arc::new(AtomicBool::new(false))));
        lock(&server.cancelled_ids).insert(json!(9).to_string());
        let call = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "diagnostics"}});
        assert!(server.handle(&call).is_none(), "a cancelled call was answered");
        assert_eq!(lock(&server.current).as_ref().map(|(id, _)| id.clone()), Some(json!(7)));
    }

    #[test]
    fn validates_arguments() {
        let tools = tools();
        let compile = &tools.as_array().unwrap()[0]["inputSchema"];
        assert!(validate(compile, &json!({"targets": ["app"]})).is_ok());
        assert_eq!(validate(compile, &json!({"targets": "app"})).unwrap_err(), "`targets` must be an array of strings");
        assert!(validate(compile, &json!({"module": "app"})).unwrap_err().starts_with("unknown argument `module`"));
        assert!(validate(compile, &json!("app")).is_err());
        let classpath = &tools.as_array().unwrap()[8]["inputSchema"];
        assert_eq!(validate(classpath, &json!({})).unwrap_err(), "`target` is required");
        assert!(validate(classpath, &json!({"target": "a", "kind": "bogus"})).is_err());
    }

    #[test]
    fn protocol_basics() {
        let server = Server::new(PathBuf::from("."));
        let init = server
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2099-01-01"}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], SUPPORTED_PROTOCOLS[0], "unknown versions get the newest");
        assert!(server.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        assert!(
            server.handle(&json!({"jsonrpc": "2.0", "id": 9, "result": {}})).is_none(),
            "responses are not answered"
        );
        let list = server.handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        assert!(list["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "compile"));
        assert_eq!(
            server.handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).unwrap()["error"]["code"],
            METHOD_NOT_FOUND
        );
        assert_eq!(server.handle(&json!({"jsonrpc": "2.0", "id": 4})).unwrap()["error"]["code"], INVALID_REQUEST);
        assert_eq!(server.handle(&json!([1])).unwrap()["error"]["code"], INVALID_REQUEST);
        let unknown = server
            .handle(&json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "nope"}}))
            .unwrap();
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
        let bad = server
            .handle(&json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "ls", "arguments": {"targets": "x"}}}))
            .unwrap();
        assert_eq!(bad["result"]["isError"], true);
        assert!(bad["result"]["structuredContent"].is_object());
        let missing = server
            .handle(&json!({"jsonrpc": "2.0", "id": 7, "method": "resources/read", "params": {"uri": "scpm://nope"}}))
            .unwrap();
        assert_eq!(missing["error"]["code"], RESOURCE_NOT_FOUND);
    }
}
