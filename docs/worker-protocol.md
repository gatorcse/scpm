# scpm worker protocol (v1)

The Rust CLI (`scpm`) talks to JVM helpers over **JSON lines**: one JSON object per line,
UTF-8, `\n`-terminated. The helper reads requests on **stdin** and writes responses and
notifications on **stdout**. **stderr** is for free-form logs only; the CLI captures it
for `scpm daemon logs` and never parses it.

Two helpers speak this protocol:

| Helper | Main class | Lifetime |
|---|---|---|
| `scpmd` (compile worker) | `scpm.worker.Main` | long-lived: many requests per process |
| test runner | `scpm.testrunner.Main` | one process per test run (forked JVM, isolated classpath) |

Later the same messages travel over a unix socket for the shared daemon. Framing does not
change.

## Envelope

Requests (CLI → helper):

```json
{"id": 1, "method": "compile", "params": { ... }}
```

Responses (helper → CLI), exactly one per request id:

```json
{"id": 1, "result": { ... }}
{"id": 1, "error": {"code": "bad_request", "message": "..."}}
```

Notifications (helper → CLI), any number, interleaved:

```json
{"method": "event", "params": { ...event... }}
```

The helper may process requests concurrently. Events always carry the `requestId` they
belong to. Unknown fields must be ignored by both sides (forward compatibility).

## Methods: compile worker

### `initialize`

`params`: `{"protocolVersion": 1}`

`result`:

```json
{"protocolVersion": 1, "workerVersion": "0.1.0", "javaVersion": "25.0.2", "pid": 12345}
```

### `compile`

`params`:

| Field | Type | Notes |
|---|---|---|
| `target` | string | Variant id, e.g. `core@jvm-3`. Used only for labelling events. |
| `scalaVersion` | string | Full version, e.g. `3.7.4`, `2.13.18`. |
| `scalaCompilerClasspath` | string[] | Jars for the compiler itself: scala3-compiler + deps, or scala-compiler + scala-reflect + scala-library. |
| `scalaLibraryClasspath` | string[] | The library jars that belong on the user classpath (scala-library, scala3-library). |
| `compilerBridge` | string | Binary bridge jar: `org.scala-lang:scala3-sbt-bridge:<scalaVersion>` for Scala 3, `org.scala-sbt:compiler-bridge_2.13:<zinc version>` for 2.13. |
| `sources` | string[] | Absolute paths to `.scala` and `.java` files. |
| `classpath` | string[] | Dependency jars plus upstream modules' classes dirs, in order. Excludes the scala library (added from `scalaLibraryClasspath`). |
| `classesDir` | string | Output directory. Created if missing. |
| `analysisFile` | string | Zinc analysis store path. Missing → full compile. |
| `scalacOptions` | string[] | |
| `javacOptions` | string[] | |
| `javaHome` | string? | For javac; defaults to the worker's JVM. |
| `rootPaths` | object? | Portable-analysis markers, e.g. `{"WS": "/abs/ws", "CACHE": "/abs/coursier/cache"}`. When present, analysis is written with paths relative to these roots. |

`result`:

```json
{"status": "ok", "durationMs": 1234, "compiledSources": 7, "noop": false}
```

`status` is one of `ok`, `failed` (compile errors), `cancelled`, `crashed` (internal
error; details in `message`).

### `cancel`

`params`: `{"requestId": 1}` → `result`: `{}`. The cancelled request then completes
with `status: cancelled`.

### `shutdown`

`params`: `{}` → `result`: `{}`, then the process exits 0.

## Methods: test runner

The test runner is started as `java -cp <runner jar + deps + test classpath> scpm.testrunner.Main`
and receives exactly one `run` request, then exits.

### `run`

| Field | Type | Notes |
|---|---|---|
| `target` | string | e.g. `core@jvm-3` |
| `testClassesDirs` | string[] | Directories to scan for test classes. |
| `frameworks` | string[]? | Fully qualified `sbt.testing.Framework` class names. Default: auto-detect the common ones (munit, ScalaTest, weaver, ZIO Test, specs2, JUnit via junit-interface, utest). |
| `filters` | string[]? | Glob patterns on fully qualified test class names. |
| `testNames` | string[]? | Run only these test cases (framework support permitting). |

`result`: `{"status": "ok" | "failed" | "crashed", "passed": 10, "failed": 1, "skipped": 0, "durationMs": 845}`

## Events

Every event has `t` (type), `requestId`, and `target`.

```jsonc
// compile progress
{"t": "compile.progress", "requestId": 1, "target": "core@jvm-3", "phase": "typer", "unitsDone": 3, "unitsTotal": 7}

// a compiler diagnostic
{"t": "diagnostic", "requestId": 1, "target": "core@jvm-3",
 "severity": "error",                 // error | warning | info
 "file": "/abs/path/Foo.scala",       // absolute, or null for global diagnostics
 "range": {"start": {"line": 41, "col": 7}, "end": {"line": 41, "col": 19}},   // 1-based lines and columns; null if unknown
 "code": "E007",                       // Scala 3 error code if present, else null
 "message": "Found: Int\nRequired: String",
 "rendered": "...",                    // the compiler's own rendering, if available
 "actions": [{"title": "...", "edits": [{"file": "...", "range": {...}, "newText": "..."}]}]}

// test lifecycle
{"t": "test.suite.start", "requestId": 1, "target": "core@jvm-3", "suite": "a.b.FooSuite"}
{"t": "test.case", "requestId": 1, "target": "core@jvm-3", "suite": "a.b.FooSuite", "name": "parses",
 "status": "passed",                   // passed | failed | error | skipped | ignored | canceled | pending
 "durationMs": 12,
 "failure": {"message": "...", "stackTrace": "...", "file": "/abs/FooSuite.scala", "line": 88}}   // only if failed/error
{"t": "test.suite.end", "requestId": 1, "target": "core@jvm-3", "suite": "a.b.FooSuite", "durationMs": 40}

// stdout/stderr captured from tests or the compiler
{"t": "log", "requestId": 1, "target": "core@jvm-3", "level": "info", "message": "..."}
```

## Distribution

The worker and test runner are plain jars, not fat jars. Their third-party dependencies
are listed as Maven coordinates in `worker/deps/<helper>.txt`, one per line. The CLI
resolves those coordinates through coursier, pins them in the lockfile's `boot` section,
and launches `java -cp <helper jar>:<deps> <main class>`. During development the CLI finds
the helper jars via `SCPM_WORKER_JAR` and `SCPM_TEST_RUNNER_JAR`, or
`worker/*/target/**/*.jar` next to the binary's repo checkout.
