#!/usr/bin/env bash
# End-to-end tests for the scpm JVM helpers: drives the compile worker and the test runner over
# the worker protocol, the way the CLI does. Needs java, cs (coursier), sbt and jq on PATH;
# from the repo root: `mise exec -- worker/it/run.sh`. Set SCPM_IT_SKIP_BUILD=1 to reuse jars.
set -euo pipefail

IT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKER_DIR="$(dirname "$IT_DIR")"
WORK="$WORKER_DIR/target/it"

SCALA3=3.9.0
SCALA3_LTS=3.3.9
SCALA213=2.13.18
MUNIT=1.3.6
CATS=2.13.0
ZINC="$(sed -n 's/^org.scala-sbt:zinc_3:\(.*\)$/\1/p' "$WORKER_DIR/deps/compile-worker.txt")"

for tool in java cs jq; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

PASSED=0
FAILED=0
TIMINGS=()

pass() { PASSED=$((PASSED + 1)); printf '  ok    %s\n' "$1"; }
fail() {
  FAILED=$((FAILED + 1))
  printf '  FAIL  %s\n' "$1"
  [ -z "${2:-}" ] || printf '        %s\n' "$2" | cut -c1-400
}
section() { printf '\n== %s\n' "$1"; }

# expect DESCRIPTION JSON JQ_FILTER: passes when the filter is true for the JSON input.
expect() {
  if jq -e "$3" >/dev/null 2>&1 <<<"$2"; then pass "$1"; else fail "$1" "$2"; fi
}

# --- build and resolve --------------------------------------------------------------------

section "build"
if [ -z "${SCPM_IT_SKIP_BUILD:-}" ]; then
  (cd "$WORKER_DIR" && sbt --client package >/dev/null)
fi
# jar_of PROJECT -> the packaged jar of an sbt 2 project (target/out/jvm/<scala or u>/<name>/)
jar_of() { ls "$WORKER_DIR"/target/out/jvm/*/"$1"/"$1"*.jar | grep -v -- '-tests\.jar$'; }
WORKER_JAR="$(jar_of scpm-compile-worker)"
RUNNER_JAR="$(jar_of scpm-test-runner)"
echo "  $WORKER_JAR"
echo "  $RUNNER_JAR"

fetch() { cs fetch --classpath "$@" 2>/dev/null; }
# A colon-separated classpath as a JSON array.
json_cp() { tr ':' '\n' <<<"$1" | grep -v '^$' | jq -R . | jq -sc .; }
# Splits a resolved classpath into the scala library jars and everything else.
SCALA_LIB='/org/scala-lang/scala3?-library(_3)?/'
lib_part() { tr ':' '\n' <<<"$1" | grep -E "$SCALA_LIB" | paste -sd: -; }
dep_part() { tr ':' '\n' <<<"$1" | { grep -Ev "$SCALA_LIB" || true; } | paste -sd: -; }

section "resolve"
WORKER_DEPS="$(fetch $(cat "$WORKER_DIR/deps/compile-worker.txt"))"
RUNNER_DEPS="$(fetch $(cat "$WORKER_DIR/deps/test-runner.txt"))"
# toolchain VERSION -> sets COMPILER_CP, LIBRARY_CP, BRIDGE
toolchain() {
  case "$1" in
    2.13.*)
      COMPILER_CP="$(fetch "org.scala-lang:scala-compiler:$1")"
      LIBRARY_CP="$(fetch "org.scala-lang:scala-library:$1")"
      BRIDGE="$(cs fetch --intransitive "org.scala-sbt:compiler-bridge_2.13:$ZINC" 2>/dev/null)"
      ;;
    3.*)
      COMPILER_CP="$(fetch "org.scala-lang:scala3-compiler_3:$1")"
      LIBRARY_CP="$(fetch "org.scala-lang:scala3-library_3:$1")"
      BRIDGE="$(cs fetch --intransitive "org.scala-lang:scala3-sbt-bridge:$1" 2>/dev/null)"
      ;;
  esac
}
CATS_CP="$(dep_part "$(fetch "org.typelevel:cats-core_3:$CATS")")"
MUNIT_RUNTIME_CP="$(fetch "org.scalameta:munit_3:$MUNIT" "org.scala-lang:scala3-library_3:$SCALA3_LTS")"
echo "  Scala $SCALA3, $SCALA3_LTS, $SCALA213; zinc $ZINC; munit $MUNIT; cats $CATS"

rm -rf "$WORK"
mkdir -p "$WORK"
cp -R "$IT_DIR/fixtures/." "$WORK/"

# Silences JDK 25 warnings about native access (swoval, in Zinc) and sun.misc.Unsafe (Scala 3 < 3.8).
JVM_FLAGS="--enable-native-access=ALL-UNNAMED --sun-misc-unsafe-memory-access=allow"

# --- worker process -----------------------------------------------------------------------

mkfifo "$WORK/worker.in"
java $JVM_FLAGS -cp "$WORKER_JAR:$WORKER_DEPS" scpm.worker.Main <"$WORK/worker.in" >"$WORK/worker.out" 2>"$WORK/worker.err" &
WORKER_PID=$!
exec 3>"$WORK/worker.in"
trap 'kill "$WORKER_PID" 2>/dev/null || true' EXIT

NEXT_ID=0
# send METHOD PARAMS_JSON -> sets ID
send() {
  NEXT_ID=$((NEXT_ID + 1))
  ID=$NEXT_ID
  jq -nc --argjson id "$ID" --arg method "$1" --argjson params "$2" \
    '{id: $id, method: $method, params: $params}' >&3
}
# await ID -> prints the response, once it arrives
await() {
  local deadline=$((SECONDS + 300)) response
  while :; do
    response="$(jq -c --argjson id "$1" 'select(.id == $id)' "$WORK/worker.out" 2>/dev/null || true)"
    if [ -n "$response" ]; then echo "$response"; return; fi
    kill -0 "$WORKER_PID" 2>/dev/null || { echo "worker exited; stderr:" >&2; cat "$WORK/worker.err" >&2; exit 1; }
    [ "$SECONDS" -lt "$deadline" ] || { echo "timed out waiting for response $1" >&2; exit 1; }
    sleep 0.05
  done
}
# events ID -> the event params of request ID, as a JSON array
events() {
  jq -sc --argjson id "$1" '[.[] | select(.method == "event" and .params.requestId == $id) | .params]' "$WORK/worker.out"
}
# compile_params TARGET SCALA_VERSION DIR [CLASSPATH] -> compile params JSON. Compiles DIR/src
# into DIR/out, with DIR as the WS root of the portable analysis.
compile_params() {
  toolchain "$2"
  jq -nc \
    --arg target "$1" --arg scalaVersion "$2" --arg dir "$3" --arg bridge "$BRIDGE" \
    --argjson compilerCp "$(json_cp "$COMPILER_CP")" \
    --argjson libraryCp "$(json_cp "$(lib_part "$LIBRARY_CP")")" \
    --argjson classpath "$(json_cp "${4:-}")" \
    --argjson sources "$(find "$3/src" -name '*.scala' -o -name '*.java' | sort | jq -R . | jq -sc .)" \
    '{target: $target, scalaVersion: $scalaVersion, scalaCompilerClasspath: $compilerCp,
      scalaLibraryClasspath: $libraryCp, compilerBridge: $bridge, sources: $sources,
      classpath: $classpath, classesDir: "\($dir)/out/classes", analysisFile: "\($dir)/out/analysis.zip",
      scalacOptions: [], javacOptions: [], rootPaths: {WS: $dir}}'
}
# compile TARGET SCALA_VERSION DIR [CLASSPATH] -> sets R to the response
compile() {
  send compile "$(compile_params "$@")"
  R="$(await "$ID")"
}
duration() { jq -r '.result.durationMs' <<<"$1"; }

section "initialize"
send initialize '{"protocolVersion": 1}'
R="$(await "$ID")"
expect "initialize returns versions and pid" "$R" \
  '.result.protocolVersion == 1 and .result.workerVersion == "0.1.0" and (.result.pid | type) == "number"'
send initialize '{"protocolVersion": 99}'
expect "unsupported protocol version is rejected" "$(await "$ID")" '.error.code == "unsupported_protocol"'
send frobnicate '{}'
expect "unknown method is rejected" "$(await "$ID")" '.error.code == "unknown_method"'

# --- Scala 3 ------------------------------------------------------------------------------

section "Scala $SCALA3"
S3="$WORK/scala3"
compile core@jvm-3 "$SCALA3" "$S3"
expect "cold compile succeeds" "$R" '.result.status == "ok" and .result.noop == false and .result.compiledSources == 2'
COLD3="$(duration "$R")"
expect "class files are written" "$(ls "$S3/out/classes/hello" | jq -R . | jq -sc .)" 'index("Greeting$.class") != null'
expect "compile.progress events are streamed" "$(events "$ID")" 'any(.[]; .t == "compile.progress" and .target == "core@jvm-3")'

compile core@jvm-3 "$SCALA3" "$S3"
expect "unchanged second compile is a no-op" "$R" '.result.status == "ok" and .result.noop == true and .result.compiledSources == 0'
NOOP3="$(duration "$R")"

cp "$S3/src/Main.scala" "$WORK/Main.scala.orig"
cp "$S3/edits/Main.type-error.scala" "$S3/src/Main.scala"
compile core@jvm-3 "$SCALA3" "$S3"
expect "type error fails the compile" "$R" '.result.status == "failed"'
DIAGNOSTICS="$(events "$ID" | jq -c '[.[] | select(.t == "diagnostic")] | sort_by(.range.start.line)')"
expect "type error is reported as a diagnostic" "$DIAGNOSTICS" \
  "length == 2 and (.[0] |
    .severity == \"error\" and .code == \"E007\" and .file == \"$S3/src/Main.scala\" and
    .range == {start: {line: 4, col: 25}, end: {line: 4, col: 27}} and
    .message == \"Found:    (42 : Int)\\nRequired: String\" and (.rendered | test(\"E007\")))"
expect "quick fixes are reported as actions" "$DIAGNOSTICS" \
  ".[1].actions[0] | (.title | test(\"\\\\(\\\\)\")) and
    .edits == [{file: \"$S3/src/Main.scala\", range: {start: {line: 5, col: 27}, end: {line: 5, col: 27}}, newText: \"()\"}]"

cp "$WORK/Main.scala.orig" "$S3/src/Main.scala"
compile core@jvm-3 "$SCALA3" "$S3"
expect "fixing the error compiles again" "$R" '.result.status == "ok" and .result.compiledSources >= 1'
expect "no diagnostics after the fix" "$(events "$ID")" 'all(.[]; .t != "diagnostic")'

cp "$S3/edits/Greeting.changed.scala" "$S3/src/Greeting.scala"
compile core@jvm-3 "$SCALA3" "$S3"
expect "warm compile after a body change recompiles one source" "$R" '.result.status == "ok" and .result.compiledSources == 1'
WARM3="$(duration "$R")"
TIMINGS+=("scala3 ($SCALA3, 2 files)|$COLD3|$WARM3|$NOOP3")

MOVED="$WORK/scala3-moved"
cp -Rp "$S3" "$MOVED"
compile core@jvm-3 "$SCALA3" "$MOVED"
expect "analysis is portable: same project in another directory is a no-op" "$R" \
  '.result.status == "ok" and .result.noop == true'

# --- Scala 2.13 with Java -----------------------------------------------------------------

section "Scala $SCALA213 + Java"
S2="$WORK/scala213"
compile shapes@jvm-2.13 "$SCALA213" "$S2"
expect "cold mixed Scala/Java compile succeeds" "$R" '.result.status == "ok" and .result.compiledSources == 2'
COLD2="$(duration "$R")"
expect "Scala and Java classes are written" "$(ls "$S2/out/classes/shapes" | jq -R . | jq -sc .)" \
  'index("Units.class") != null and index("Shape$.class") != null'
expect "non-exhaustive match is a warning diagnostic" "$(events "$ID")" \
  "any(.[]; .t == \"diagnostic\" and .severity == \"warning\" and .file == \"$S2/src/Shapes.scala\" and
    .range.start.line == 8 and (.message | test(\"not be exhaustive\")))"
compile shapes@jvm-2.13 "$SCALA213" "$S2"
expect "unchanged second compile is a no-op" "$R" '.result.noop == true'
NOOP2="$(duration "$R")"
sed -i.bak 's/%.2f units/%.3f units/' "$S2/src/Units.java" && rm "$S2/src/Units.java.bak"
compile shapes@jvm-2.13 "$SCALA213" "$S2"
expect "warm compile after a Java change" "$R" '.result.status == "ok" and .result.compiledSources >= 1'
WARM2="$(duration "$R")"
TIMINGS+=("scala213 ($SCALA213, 1 Scala + 1 Java)|$COLD2|$WARM2|$NOOP2")

# --- Library dependency -------------------------------------------------------------------

section "Scala $SCALA3 + cats-core $CATS"
SC="$WORK/cats"
compile sums@jvm-3 "$SCALA3" "$SC" "$CATS_CP"
expect "compile against cats-core succeeds" "$R" '.result.status == "ok" and .result.compiledSources == 1'
COLDC="$(duration "$R")"
compile sums@jvm-3 "$SCALA3" "$SC" "$CATS_CP"
expect "unchanged second compile is a no-op" "$R" '.result.noop == true'
NOOPC="$(duration "$R")"
sed -i.bak 's/"one"/"uno"/' "$SC/src/Sums.scala" && rm "$SC/src/Sums.scala.bak"
compile sums@jvm-3 "$SCALA3" "$SC" "$CATS_CP"
expect "warm compile after a change" "$R" '.result.status == "ok" and .result.compiledSources == 1'
TIMINGS+=("cats (Scala $SCALA3 + cats-core, 1 file)|$COLDC|$(duration "$R")|$NOOPC")

# --- Concurrency and cancellation ---------------------------------------------------------

section "concurrency and cancellation"
sed -i.bak 's/Hi,/Hey,/' "$S3/src/Greeting.scala" && rm "$S3/src/Greeting.scala.bak"
sed -i.bak 's/%.3f units/%.4f units/' "$S2/src/Units.java" && rm "$S2/src/Units.java.bak"
send compile "$(compile_params core@jvm-3 "$SCALA3" "$S3")"
A=$ID
send compile "$(compile_params shapes@jvm-2.13 "$SCALA213" "$S2")"
B=$ID
expect "concurrent compiles of two targets both succeed" \
  "$(jq -sc . <<<"$(await "$A") $(await "$B")")" 'all(.[]; .result.status == "ok" and .result.compiledSources == 1)'

BIG="$WORK/big"
mkdir -p "$BIG/src"
for i in $(seq 1 400); do
  printf 'package big\n\nobject Gen%d:\n  def value: List[Int] = (1 to %d).toList.map(_ * 2).filter(_ %% 3 == 0)\n' "$i" "$i" >"$BIG/src/Gen$i.scala"
done
send compile "$(compile_params big@jvm-3 "$SCALA3" "$BIG")"
BIG_ID=$ID
until jq -e --argjson id "$BIG_ID" 'select(.params.requestId == $id and .params.t == "compile.progress")' \
  "$WORK/worker.out" >/dev/null 2>&1; do sleep 0.05; done
send cancel "{\"requestId\": $BIG_ID}"
expect "cancel is acknowledged" "$(await "$ID")" '.result == {}'
expect "cancelled compile reports status cancelled" "$(await "$BIG_ID")" '.result.status == "cancelled"'

compile big@jvm-3 "$SCALA3" "$BIG"
expect "compile after a cancelled one starts over and succeeds" "$R" '.result.status == "ok" and .result.compiledSources == 400'
COLDB="$(duration "$R")"
sed -i.bak 's/_ \* 2/_ * 4/' "$BIG/src/Gen7.scala" && rm "$BIG/src/Gen7.scala.bak"
compile big@jvm-3 "$SCALA3" "$BIG"
expect "a change in one of 400 files recompiles only that file" "$R" '.result.status == "ok" and .result.compiledSources == 1'
TIMINGS+=("big (Scala $SCALA3, 400 files; 'cold' = warm JVM)|$COLDB|$(duration "$R")|")

# --- Test runner --------------------------------------------------------------------------

section "test runner (munit $MUNIT on Scala $SCALA3_LTS)"
SM="$WORK/munit"
compile tests@jvm-3 "$SCALA3_LTS" "$SM" "$(dep_part "$MUNIT_RUNTIME_CP")"
expect "munit fixture compiles" "$R" '.result.status == "ok"'
jq -nc --arg dir "$SM/out/classes" \
  '{id: 1, method: "run", params: {target: "tests@jvm-3", testClassesDirs: [$dir]}}' |
  java $JVM_FLAGS -cp "$RUNNER_JAR:$RUNNER_DEPS:$MUNIT_RUNTIME_CP:$SM/out/classes" scpm.testrunner.Main \
    >"$WORK/runner.out" 2>"$WORK/runner.err"
RUNNER_EVENTS="$(jq -sc '[.[] | select(.method == "event") | .params]' "$WORK/runner.out")"
expect "run result counts one pass and one failure" \
  "$(jq -c 'select(.id == 1)' "$WORK/runner.out")" \
  '.result.status == "failed" and .result.passed == 1 and .result.failed == 1 and .result.skipped == 0'
expect "suite start and end events" "$RUNNER_EVENTS" \
  '(map(select(.t == "test.suite.start" or .t == "test.suite.end") | [.t, .suite]) ==
    [["test.suite.start", "arith.ArithmeticSuite"], ["test.suite.end", "arith.ArithmeticSuite"]])'
expect "passing test case event" "$RUNNER_EVENTS" \
  'any(.[]; .t == "test.case" and .name == "addition works" and .status == "passed" and .failure == null)'
expect "failing test case event with location" "$RUNNER_EVENTS" \
  'any(.[]; .t == "test.case" and .name == "subtraction is broken" and .status == "failed" and
    .failure.file == "arith/ArithmeticSuite.scala" and .failure.line == 10 and (.failure.message | length) > 0)'
expect "test stdout becomes a log event" "$RUNNER_EVENTS" \
  'any(.[]; .t == "log" and .level == "info" and .message == "hello from a test")'
expect "runner stdout is protocol JSON only" "$(jq -sc 'length' "$WORK/runner.out" 2>&1)" '. > 0'

# --- Shutdown -----------------------------------------------------------------------------

section "shutdown"
send shutdown '{}'
expect "shutdown is acknowledged" "$(await "$ID")" '.result == {}'
exec 3>&-
if wait "$WORKER_PID"; then pass "worker exits 0"; else fail "worker exits 0"; fi
expect "worker stdout is protocol JSON only" "$(jq -sc 'all(.[]; has("id") or .method == "event")' "$WORK/worker.out" 2>&1)" '. == true'

section "timings (ms, worker durationMs)"
printf '  %-50s %8s %8s %8s\n' fixture cold warm no-op
for row in "${TIMINGS[@]}"; do
  IFS='|' read -r name cold warm noop <<<"$row"
  printf '  %-50s %8s %8s %8s\n' "$name" "$cold" "$warm" "$noop"
done

printf '\n%d passed, %d failed\n' "$PASSED" "$FAILED"
[ "$FAILED" -eq 0 ]
