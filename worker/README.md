# scpm JVM helpers

The two JVM processes the Rust CLI drives over the [worker protocol](../docs/worker-protocol.md):

| Helper | Main class | Language | Runtime dependencies |
|---|---|---|---|
| `scpmd`, the compile worker | `scpm.worker.Main` | Scala 3.9 | [`deps/compile-worker.txt`](deps/compile-worker.txt): Zinc 2.1.0, jsoniter-scala-core, scala3-library |
| test runner | `scpm.testrunner.Main` | Java 17 | [`deps/test-runner.txt`](deps/test-runner.txt): `org.scala-sbt:test-interface:1.0` |

Both ship as plain jars. The `deps/*.txt` files list each helper's third-party dependencies as
`org:name:version`, one per line, with full artifact names (`zinc_3`, not `zinc` with `::`). The
CLI resolves them with coursier. `build.sbt` reads the same files, so the build and the CLI cannot
drift apart.

## Build

```sh
cd worker
mise exec -- sbt package     # both jars
mise exec -- sbt testFull    # unit tests (sbt 2's `test` only reruns what changed)
mise exec -- it/run.sh       # end-to-end tests (needs jq); SCPM_IT_SKIP_BUILD=1 reuses the jars
```

sbt 2 writes every project's output under the build root:

- `worker/target/out/jvm/scala-3.9.0/scpm-compile-worker/scpm-compile-worker_3-0.1.0.jar`
- `worker/target/out/jvm/u/scpm-test-runner/scpm-test-runner-0.1.0.jar`

## Launch by hand

```sh
cd worker
WORKER_JAR=target/out/jvm/scala-3.9.0/scpm-compile-worker/scpm-compile-worker_3-0.1.0.jar
java --enable-native-access=ALL-UNNAMED --sun-misc-unsafe-memory-access=allow \
  -XX:+ExitOnOutOfMemoryError \
  -cp "$WORKER_JAR:$(cs fetch -p $(cat deps/compile-worker.txt))" scpm.worker.Main
```

then type requests, one per line:

```json
{"id":1,"method":"initialize","params":{"protocolVersion":1}}
{"id":2,"method":"shutdown","params":{}}
```

The test runner shares one flat classpath with the tests and reads exactly one request:

```sh
RUNNER_JAR=target/out/jvm/u/scpm-test-runner/scpm-test-runner-0.1.0.jar
echo '{"id":1,"method":"run","params":{"target":"core@jvm-3","testClassesDirs":["/abs/test-classes"]}}' |
  java -cp "$RUNNER_JAR:$(cs fetch -p $(cat deps/test-runner.txt)):<test runtime classpath>:/abs/test-classes" \
    scpm.testrunner.Main
```

`it/run.sh` is a complete, working example of driving both helpers, including how to resolve
compiler, library and bridge jars for Scala 3 and 2.13.

**JVM flags.** On JDK 25, `--enable-native-access=ALL-UNNAMED` silences a warning from Zinc's
file watcher library (swoval), and `--sun-misc-unsafe-memory-access=allow` silences one from
Scala 3 versions before 3.8 (`LazyVals`). Both go to stderr only, so they are harmless either way.
`-XX:+ExitOnOutOfMemoryError` is recommended for the long-lived worker: a worker that has run out of
memory should die and be restarted rather than limp on.

## How the compile worker works

- **One Zinc, many Scala versions.** Each `(scalaVersion, compiler classpath, library classpath,
  bridge)` gets one `AnalyzingCompiler`, cached for the life of the process, so the compiler stays
  JIT-warm across requests. Class loaders are layered as platform + `xsbti` → scala library →
  scala compiler. `xsbti.*` always comes from the worker, so Zinc, the bridge and the compiler
  agree on the interfaces even when the compiler classpath carries its own `compiler-interface`
  jar (Scala 3 does). Nothing else of the worker, including its own Scala 3.9 library, is visible
  to the compiler.
- **Bridges.** The request supplies a binary bridge jar: `scala3-sbt-bridge` for Scala 3 (tested
  with 3.3.9 and 3.9.0) and `compiler-bridge_2.13:2.1.0` for 2.13 (tested with 2.13.18). The worker
  never compiles a bridge from sources, so Scala 2.12 is not supported.
- **Analysis.** Zinc's consistent binary format (`ConsistentFileAnalysisStore`, the sbt 2 default)
  at `analysisFile`. The last analysis of each target stays in memory and is reused while the file
  on disk is unchanged (same modification time), so a warm compile does not deserialize it again.
  Deleting or replacing the file is honoured.
- **Classpath.** The worker compiles against `classesDir` + `scalaLibraryClasspath` + `classpath`.
  The output directory has to come first so an incremental run sees the classes of the sources it
  does not recompile.
- **Portable analysis.** Paths go through Zinc's `MappedFileConverter` with the request's
  `rootPaths`, so files under a root are stored as `${WS}/...`, `${CACHE}/...` and so on. Paths
  under no root are stored as absolute paths. `it/run.sh` checks this by copying a compiled
  project to another directory and getting a no-op compile there. Diagnostics always carry
  absolute paths.
- **Diagnostics.** Streamed from a custom `xsbti.Reporter` as soon as the compiler reports them.
  Ranges are 1-based lines and columns, converted from Zinc's 0-based columns, and fall back to the
  caret position when the compiler gives no range. Scala 3 message ids become `E007`-style codes.
  `message` has ANSI colours stripped. `rendered` is exactly what the compiler printed, colours
  included. Quick fixes (`Problem.actions`) become `actions`.
- **Concurrency.** Each `compile` runs on its own thread with a 64 MiB stack (scalac recurses
  deeply). Requests for the same `analysisFile` are serialized and different targets run in
  parallel. The CLI decides how many to send at once.
- **Cancellation.** Polled through Zinc's `CompileProgress`. A bridge either throws
  `CompileCancelled` or just stops the run (the Scala 3 bridge does the latter). Either way the
  response is `cancelled` and the partial analysis is thrown away, so the next compile starts from
  the last good state.
  `shutdown` and end of stdin cancel everything that is still running, wait for those requests to
  answer `cancelled`, and then exit.
- **stdout hygiene.** `System.out` is pointed at stderr before anything else runs. Only the
  protocol writer holds the real stdout. Zinc's own log lines (info and above) become `log` events.
- **`compiledSources`** counts sources whose classes were (re)analysed in this run, taken from
  the API timestamps in the new analysis.

## How the test runner works

- **Plain Java, no Scala library.** The runner sits on one flat classpath with the code under
  test, which may be Scala 2.13, Scala 3.3 or Scala 3.9, each with a different `scala-library`. A
  runner written in Scala would bring its own and could clash. sbt's forked test runner is Java for
  the same reason. That rules out a JSON library too, so `Json.java` parses and writes the little
  JSON the runner needs.
- **Discovery.** Walks `testClassesDirs`, loads every class without initializing it, and matches
  it against each framework's `SubclassFingerprint`s and `AnnotatedFingerprint`s. Annotations count
  on the class or on any public method. A class goes to the first framework that matches it. When
  the request names no `frameworks`, the runner uses whichever of munit, ScalaTest, weaver, ZIO
  Test, utest, specs2 and JUnit (junit-interface) are on the classpath. `filters` are globs on class
  names, and `testNames` become `TestSelector`s.
- **Events.** Each suite task gets `test.suite.start`/`test.suite.end`, and each framework event
  becomes a `test.case`. JUnit-style frameworks (munit, junit-interface) report
  `Suite.testName`, which is split back into suite and test name. A failure's `file`/`line` come
  from the innermost stack frame in the suite's own class.
- **Output.** `System.out` and `System.err` become `log` events (levels `info` and `error`), line
  by line. Framework loggers map to `debug`/`info`/`warn`/`error`. The process exits 0 once the
  response is written, even if a framework left non-daemon threads behind, and exits 2 on a
  malformed request.

## Measurements

Worker-reported `durationMs` from `it/run.sh`, three runs on an Apple M2 Pro (10 cores), JDK
25.0.5, default heap. "Cold" is the first compile of that Scala version in a fresh worker JVM, so
it includes class loading and JIT warm-up of the compiler. "Warm" is a later compile after a source
change, in the same worker.

| Fixture | Cold | Warm (after change) | No-op |
|---|---|---|---|
| Scala 3.9.0, 2 files (body change in 1) | 1435–1570 ms | 97–103 ms | 7 ms |
| Scala 2.13.18, 1 Scala + 1 Java (Java change) | 1176–1242 ms | 36–49 ms | 2 ms |
| Scala 3.9.0 + cats-core, 1 file | 424–435 ms ¹ | 292–313 ms | 2 ms |
| Scala 3.9.0, 400 generated files (1 file changed) | 1715–1820 ms ¹ | 173–195 ms | — |

¹ These run after the 2-file Scala 3.9 fixture in the same worker, so the compiler is already
loaded and partly warm. The first-ever compile is not cold here, and the gap from the 2-file
fixture's 1.4 s shows what the warm worker saves. The 400-file "cold" figure is a full compile in
an already warm worker.

These match the shape of the design doc's measurements: the first compile in a JVM costs about a
second of fixed warm-up, an incremental compile in a warm worker costs tens to hundreds of
milliseconds, and a no-op is a few milliseconds. The CLI is meant to skip even that with its own fingerprint check.

## Limitations

- **Upstream analyses are not used.** The protocol does not pass upstream modules' analysis
  files, so upstream class directories are treated like library jars and changes are detected
  from their class files. This is correct but coarser than sbt, which invalidates by API name
  hashes across modules. A later protocol version could add `upstreamAnalyses: {classesDir: analysisFile}`.
- No pipelining or early output (`earlyAnalysisStore`, `earlyJarPath`) yet.
- No Scala 2.12 (needs a bridge compiled from source) and no Scala.js or Scala Native linking.
- Compile progress `unitsDone`/`unitsTotal` are passed through from the compiler as they are.
  Scala 3 counts units × phases.
- `javaHome` uses Zinc's forked `javac` and is not covered by the end-to-end tests. Without it,
  Java sources are compiled in-process by the worker's JDK.
- The compiler cache is unbounded. A worker that has seen many Scala versions keeps all of them
  loaded until it restarts.
- Test runner: no forking or parallelism of its own (frameworks may run tests in parallel
  internally). No framework arguments (for example munit's `-n` to turn off colours, so munit's log
  lines contain ANSI codes). Test cases are not attributed to nested suites beyond what the
  framework reports. A test that calls `System.exit` ends the run without a response.
- A failure's `file` is the source path relative to its source root (`a/b/FooSuite.scala`),
  because class files record only a file name. The CLI resolves it against the target's sources.
