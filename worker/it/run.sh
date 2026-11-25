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
UTEST=0.9.5
JUNIT_INTERFACE=0.13.3
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
UTEST_RUNTIME_CP="$(fetch "com.lihaoyi:utest_3:$UTEST" "org.scala-lang:scala3-library_3:$SCALA3")"
JUNIT_RUNTIME_CP="$(fetch "com.github.sbt:junit-interface:$JUNIT_INTERFACE")"
JAVA_HOME_NOW="$(java -XshowSettings:properties -version 2>&1 | sed -n 's/^ *java.home = //p')"
echo "  Scala $SCALA3, $SCALA3_LTS, $SCALA213; zinc $ZINC; munit $MUNIT; cats $CATS; utest $UTEST"

rm -rf "$WORK"
mkdir -p "$WORK"
cp -R "$IT_DIR/fixtures/." "$WORK/"

# The launch flags the CLI uses (worker/README.md): HotSpot's own messages go to stderr, never
# stdout, and an out-of-memory JVM exits. The last two silence JDK 25 warnings about native access
# (swoval, in Zinc) and sun.misc.Unsafe (Scala 3 < 3.8).
JVM_FLAGS="-XX:+DisplayVMOutputToStderr -Xlog:disable -Xlog:all=warning:stderr -XX:+ExitOnOutOfMemoryError
  --enable-native-access=ALL-UNNAMED --sun-misc-unsafe-memory-access=allow"

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
# compile_params TARGET SCALA_VERSION DIR [CLASSPATH] [OVERRIDES_JSON] -> compile params JSON.
# Compiles DIR/src into DIR/out, with DIR as the WS root of the portable analysis.
compile_params() {
  local overrides="${5:-}"
  toolchain "$2"
  jq -nc \
    --arg target "$1" --arg scalaVersion "$2" --arg dir "$3" --arg bridge "$BRIDGE" \
    --argjson overrides "${overrides:-"{}"}" \
    --argjson compilerCp "$(json_cp "$COMPILER_CP")" \
    --argjson libraryCp "$(json_cp "$(lib_part "$LIBRARY_CP")")" \
    --argjson classpath "$(json_cp "${4:-}")" \
    --argjson sources "$(find "$3/src" -name '*.scala' -o -name '*.java' | sort | jq -R . | jq -sc .)" \
    '{target: $target, scalaVersion: $scalaVersion, scalaCompilerClasspath: $compilerCp,
      scalaLibraryClasspath: $libraryCp, compilerBridge: $bridge, sources: $sources,
      classpath: $classpath, classesDir: "\($dir)/out/classes", analysisFile: "\($dir)/out/analysis.zip",
      scalacOptions: [], javacOptions: [], rootPaths: {WS: $dir}} + $overrides'
}
# compile TARGET SCALA_VERSION DIR [CLASSPATH] -> sets R to the response
compile() {
  send compile "$(compile_params "$@")"
  R="$(await "$ID")"
}
duration() { jq -r '.result.durationMs' <<<"$1"; }
# diagnostics ID -> the diagnostic events of request ID, by file and line
diagnostics() { events "$1" | jq -c '[.[] | select(.t == "diagnostic")] | sort_by(.file, .range.start.line)'; }
# class_files DIR -> a checksum listing of the class directory of fixture DIR
class_files() { (cd "$1/out/classes" && find . -type f | sort | xargs shasum); }
# responded_before A B -> whether request A was answered before request B
responded_before() {
  jq -se --argjson a "$1" --argjson b "$2" '[.[] | select(has("id")) | .id] | index($a) < index($b)' \
    "$WORK/worker.out" >/dev/null
}

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
# The failed compile restored the previous classes, so the fixed source is up to date again.
expect "fixing the error compiles again" "$R" '.result.status == "ok"'
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
    .range.start.line == 8 and .code == null and (.message | test(\"not be exhaustive\")))"
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

# --- Deleted and renamed sources ---------------------------------------------------------

section "deleted and renamed sources (Scala $SCALA3)"
SD="$WORK/scala3-del"
compile del@jvm-3 "$SCALA3" "$SD"
expect "A and B compile" "$R" '.result.status == "ok" and .result.compiledSources == 2'
BEFORE="$(class_files "$SD")"
rm "$SD/src/B.scala"
cp "$SD/edits/C.scala" "$SD/src/"
compile del@jvm-3 "$SCALA3" "$SD"
expect "code using a deleted source no longer compiles (no stale .tasty)" "$R" '.result.status == "failed"'
expect "the new user of the deleted source gets a diagnostic" "$(diagnostics "$ID")" \
  "any(.[]; .file == \"$SD/src/C.scala\" and .severity == \"error\" and (.message | test(\"Not found: B\")))"
if [ "$(class_files "$SD")" = "$BEFORE" ]; then pass "a failed compile leaves classesDir as before"
else fail "a failed compile leaves classesDir as before" "$(class_files "$SD")"; fi
rm "$SD/src/C.scala"
cp "$IT_DIR/fixtures/scala3-del/src/B.scala" "$SD/src/Bee.scala"
compile del@jvm-3 "$SCALA3" "$SD"
expect "a renamed source compiles" "$R" '.result.status == "ok" and .result.compiledSources >= 1'
rm "$SD/out/analysis.zip" "$SD/src/Bee.scala"
compile del@jvm-3 "$SCALA3" "$SD"
expect "without an analysis, stale classes are wiped: a deleted dependency fails the compile" "$R" \
  '.result.status == "failed"'
expect "and is reported where it is used" "$(diagnostics "$ID")" \
  "any(.[]; .file == \"$SD/src/A.scala\" and (.message | test(\"Not found: B\")))"

# --- Java diagnostics ---------------------------------------------------------------------

section "Java diagnostics (in-process and forked javac)"
JE="$WORK/java-errors"
cp -R "$JE" "$WORK/java-errors-forked"
JF="$WORK/java-errors-forked"
java_error_check() { # LABEL DIR
  expect "$1: Java error fails the compile" "$R" '.result.status == "failed"'
  expect "$1: Java error diagnostic with detail lines and a one-character range" "$(diagnostics "$ID")" \
    "length == 1 and (.[0] | .severity == \"error\" and .file == \"$2/src/Broken.java\" and .code == null and
      (.message | startswith(\"cannot find symbol\")) and (.message | test(\"symbol: +variable q\")) and
      .range == {start: {line: 5, col: 12}, end: {line: 5, col: 13}})"
}
compile jerr@jvm-2.13 "$SCALA213" "$JE"
java_error_check "in-process javac" "$JE"
compile jerr@jvm-2.13 "$SCALA213" "$JF" "" "{\"javaHome\": \"$JAVA_HOME_NOW\"}"
java_error_check "forked javac (javaHome)" "$JF"

# --- Request validation and broken toolchains ---------------------------------------------

section "request validation and broken toolchains"
send compile "$(compile_params core@jvm-3 "$SCALA3" "$S3" "" '{"compilerBridge": "/no/such/bridge.jar"}')"
expect "a missing bridge jar is a bad request naming the path" "$(await "$ID")" \
  '.error.code == "bad_request" and (.error.message | test("/no/such/bridge.jar"))'
toolchain "$SCALA3"
NOT_A_BRIDGE="$(lib_part "$LIBRARY_CP" | cut -d: -f1)"
BAD="$WORK/scala3-bad-bridge"
cp -R "$IT_DIR/fixtures/scala3" "$BAD"
send compile "$(compile_params bad@jvm-3 "$SCALA3" "$BAD" "" "{\"compilerBridge\": \"$NOT_A_BRIDGE\"}")"
expect "a jar that is not a compiler bridge crashes the compile" "$(await "$ID")" '.result.status == "crashed"'
compile core@jvm-3 "$SCALA3" "$S3"
expect "the worker keeps serving after a crash" "$R" '.result.status == "ok"'
send compile '{"target": "x"}'
expect "a compile request missing fields is answered with its id" "$(await "$ID")" \
  ".id == $ID and .error.code == \"bad_request\""

# --- Upstream modules ---------------------------------------------------------------------

section "upstream modules"
UP="$WORK/upstream"
UP_ROOTS="{\"rootPaths\": {\"WS\": \"$UP\"}}"
# with_upstream MODULE -> overrides passing MODULE's analysis as an upstream analysis
with_upstream() {
  jq -nc --arg dir "$UP/$1" --argjson base "$UP_ROOTS" \
    '$base + {upstreamAnalyses: [{classesDir: "\($dir)/out/classes", analysisFile: "\($dir)/out/analysis.zip"}]}'
}

compile lib@jvm-3 "$SCALA3" "$UP/lib" "" "$UP_ROOTS"
expect "upstream Scala module compiles" "$R" '.result.status == "ok"'
compile libuser@jvm-3 "$SCALA3" "$UP/libuser" "$UP/lib/out/classes" "$(with_upstream lib)"
expect "downstream module compiles against it" "$R" '.result.status == "ok"'
compile libuser@jvm-3 "$SCALA3" "$UP/libuser" "$UP/lib/out/classes" "$(with_upstream lib)"
expect "unchanged upstream: downstream is a no-op" "$R" '.result.noop == true'
cp "$UP/lib/edits/Names.renamed.scala" "$UP/lib/src/Names.scala"
compile lib@jvm-3 "$SCALA3" "$UP/lib" "" "$UP_ROOTS"
expect "upstream API change compiles" "$R" '.result.status == "ok"'
compile libuser@jvm-3 "$SCALA3" "$UP/libuser" "$UP/lib/out/classes" "$(with_upstream lib)"
expect "with upstream analysis: an upstream API change fails the dependent" "$R" '.result.status == "failed"'
expect "with a diagnostic at the use" "$(diagnostics "$ID")" \
  "any(.[]; .file == \"$UP/libuser/src/User.scala\" and (.message | test(\"names\")))"

compile javalib@jvm-3 "$SCALA3" "$UP/javalib" "" "$UP_ROOTS"
expect "upstream Java module compiles" "$R" '.result.status == "ok"'
compile app@jvm-3 "$SCALA3" "$UP/app" "$UP/javalib/out/classes" "$UP_ROOTS"
expect "downstream Scala module compiles against it" "$R" '.result.status == "ok"'
rm "$UP/javalib/src/javalib/Shout.java"
rm -rf "$UP/javalib/out/classes"
compile app@jvm-3 "$SCALA3" "$UP/app" "$UP/javalib/out/classes" "$UP_ROOTS"
expect "without upstream analysis: deleted upstream classes fail the dependent" "$R" '.result.status == "failed"'
expect "with a diagnostic at the use" "$(diagnostics "$ID")" \
  "any(.[]; .file == \"$UP/app/src/App.scala\" and (.message | test(\"Not found: (javalib|Shout)\")))"

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

rm "$BIG/out/analysis.zip"
send compile "$(compile_params big@jvm-3 "$SCALA3" "$BIG")"
FIRST=$ID
send compile "$(compile_params big@jvm-3 "$SCALA3" "$BIG")"
QUEUED=$ID
send cancel "{\"requestId\": $QUEUED}"
expect "cancelling a compile queued behind the same target answers cancelled" "$(await "$QUEUED")" \
  '.result.status == "cancelled"'
expect "the full compile it was queued behind still succeeds" "$(await "$FIRST")" \
  '.result.status == "ok" and .result.compiledSources == 400'
if responded_before "$QUEUED" "$FIRST"; then pass "the queued compile is answered without waiting for the running one"
else fail "the queued compile is answered without waiting for the running one"; fi

BEFORE_BIG="$(class_files "$BIG")"
perl -pi -e 's/List\[Int\]/Seq[Int]/' "$BIG"/src/*.scala
send compile "$(compile_params big@jvm-3 "$SCALA3" "$BIG")"
CHANGED=$ID
until jq -e --argjson id "$CHANGED" 'select(.params.requestId == $id and .params.t == "compile.progress")' \
  "$WORK/worker.out" >/dev/null 2>&1; do sleep 0.05; done
send cancel "{\"requestId\": $CHANGED}"
expect "cancelling a 400-file API change answers cancelled" "$(await "$CHANGED")" '.result.status == "cancelled"'
if [ "$(class_files "$BIG")" = "$BEFORE_BIG" ]; then pass "a cancelled compile leaves classesDir as before"
else fail "a cancelled compile leaves classesDir as before" "$(class_files "$BIG" | wc -l) files"; fi

DEEP="$WORK/deep"
mkdir -p "$DEEP/src"
{ printf 'object Deep:\n  val x = '; head -c 300000 /dev/zero | tr '\0' '('; printf 1; head -c 300000 /dev/zero | tr '\0' ')'; echo; } \
  >"$DEEP/src/Deep.scala"
compile deep@jvm-3 "$SCALA3" "$DEEP"
expect "a compiler stack overflow is answered crashed" "$R" \
  '.result.status == "crashed" and (.result.message | test("StackOverflowError"))'
compile core@jvm-3 "$SCALA3" "$S3"
expect "and the worker keeps serving" "$R" '.result.status == "ok"'

# --- Test runner --------------------------------------------------------------------------

# start_runner NAME CLASSPATH PARAMS_JSON -> starts the test runner with the request written and
# stdin kept open on fd 4, as the CLI does; sets RUNNER_PID
start_runner() {
  mkfifo "$WORK/$1.in"
  java $JVM_FLAGS -cp "$RUNNER_JAR:$RUNNER_DEPS:$2" scpm.testrunner.Main \
    <"$WORK/$1.in" >"$WORK/$1.out" 2>"$WORK/$1.err" &
  RUNNER_PID=$!
  exec 4>"$WORK/$1.in"
  jq -nc --argjson params "$3" '{id: 1, method: "run", params: $params}' >&4
}
# run_tests NAME CLASSPATH PARAMS_JSON -> runs to completion, closing stdin once the response
# arrives (as the CLI does); sets RUN_RESULT, RUN_EVENTS, RUN_LOGS (log messages) and RUN_EXIT
run_tests() {
  start_runner "$@"
  until jq -e 'select(has("id"))' "$WORK/$1.out" >/dev/null 2>&1 || ! kill -0 "$RUNNER_PID" 2>/dev/null; do
    sleep 0.05
  done
  exec 4>&-
  set +e
  wait "$RUNNER_PID"
  RUN_EXIT=$?
  set -e
  RUN_RESULT="$(jq -c 'select(has("id"))' "$WORK/$1.out")"
  RUN_EVENTS="$(jq -sc '[.[] | select(.method == "event") | .params]' "$WORK/$1.out")"
  RUN_LOGS="$(jq -c '[.[] | select(.t == "log") | .message]' <<<"$RUN_EVENTS")"
}

section "test runner: munit $MUNIT on Scala $SCALA3_LTS"
SM="$WORK/munit"
compile tests@jvm-3 "$SCALA3_LTS" "$SM" "$(dep_part "$MUNIT_RUNTIME_CP")"
expect "munit fixture compiles" "$R" '.result.status == "ok"'
MUNIT_CP="$MUNIT_RUNTIME_CP:$SM/out/classes"
run_params() { jq -nc --arg dir "$SM/out/classes" "{target: \"tests@jvm-3\", testClassesDirs: [\$dir]} + $1"; }

run_tests munit "$MUNIT_CP" "$(run_params '{filters: ["*.ArithmeticSuite"]}')"
expect "run result counts one pass and one failure" "$RUN_RESULT" \
  '.result == {status: "failed", passed: 1, failed: 1, skipped: 0, durationMs: .result.durationMs}'
expect "suite start and end events" "$RUN_EVENTS" \
  '(map(select(.t == "test.suite.start" or .t == "test.suite.end") | [.t, .suite]) ==
    [["test.suite.start", "arith.ArithmeticSuite"], ["test.suite.end", "arith.ArithmeticSuite"]])'
expect "passing test case event" "$RUN_EVENTS" \
  'any(.[]; .t == "test.case" and .name == "addition works" and .status == "passed" and .failure == null)'
expect "failing test case event with location" "$RUN_EVENTS" \
  'any(.[]; .t == "test.case" and .name == "subtraction is broken" and .status == "failed" and
    .failure.file == "arith/ArithmeticSuite.scala" and .failure.line == 10 and (.failure.message | length) > 0)'
expect "test stdout becomes a log event" "$RUN_EVENTS" \
  'any(.[]; .t == "log" and .level == "info" and .message == "hello from a test")'
expect "runner stdout is protocol JSON only" "$(jq -sc 'length' "$WORK/munit.out" 2>&1)" '. > 0'
expect "runner exits 0 after answering" "$RUN_EXIT" '. == 0'
expect "closing stdin after the response is not reported as the parent going away" "$RUN_LOGS" \
  'all(.[]; test("parent went away") | not)'

run_tests munit-names "$MUNIT_CP" "$(run_params '{filters: ["*.ArithmeticSuite"], testNames: ["addition works"]}')"
expect "testNames runs only the named test" "$RUN_RESULT" '.result.status == "ok" and .result.passed == 1 and .result.failed == 0'

run_tests munit-stdin "$MUNIT_CP" "$(run_params '{filters: ["*.StdinSuite"]}')"
expect "a test reading stdin gets end of input" "$RUN_RESULT" '.result.status == "ok" and .result.passed == 1'

run_tests munit-none "$MUNIT_CP" "$(run_params '{filters: ["*.NoSuchSuite"]}')"
expect "filters matching nothing: ok with a warning naming the filter" "$(jq -sc . <<<"$RUN_RESULT $RUN_EVENTS")" \
  '.[0].result.status == "ok" and .[0].result.passed == 0 and
    any(.[1][]; .t == "log" and .level == "warn" and (.message | contains("*.NoSuchSuite")))'

run_tests munit-exit "$MUNIT_CP" "$(run_params '{filters: ["*.ExitSuite"]}')"
expect "System.exit in a test: crashed result naming the status" "$RUN_RESULT" \
  '.id == 1 and .result.status == "crashed" and .result.message == "a test called System.exit(3)"'
expect "System.exit in a test: suite is closed and named in the log" "$RUN_EVENTS" \
  'any(.[]; .t == "test.suite.end" and .suite == "arith.ExitSuite") and
    any(.[]; .t == "log" and .level == "error" and (.message | test("arith.ExitSuite"))) and
    all(.[]; .t != "log" or (.message | test("parent went away") | not))'

start_runner munit-orphan "$MUNIT_CP" "$(run_params '{filters: ["*.SleepSuite"]}')"
until jq -e 'select(.params.message? // "" | startswith("child pid"))' "$WORK/munit-orphan.out" >/dev/null 2>&1; do
  sleep 0.05
done
exec 4>&-
deadline=$((SECONDS + 10))
while kill -0 "$RUNNER_PID" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.1; done
if kill -0 "$RUNNER_PID" 2>/dev/null; then
  kill "$RUNNER_PID"
  fail "the runner exits when its parent closes stdin"
else
  set +e; wait "$RUNNER_PID"; ORPHAN_EXIT=$?; set -e
  expect "the runner exits 4 when its parent closes stdin mid-run" "$ORPHAN_EXIT" '. == 4'
  expect "and answers crashed, saying why" "$(jq -c 'select(has("id"))' "$WORK/munit-orphan.out")" \
    '.result.status == "crashed" and .result.message == "parent went away (stdin closed)"'
  CHILD="$(jq -r 'select(.params.t == "log") | .params.message | capture("child pid (?<pid>[0-9]+)").pid' \
    "$WORK/munit-orphan.out")"
  deadline=$((SECONDS + 5))
  while kill -0 "$CHILD" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.1; done
  if [ -n "$CHILD" ] && ! kill -0 "$CHILD" 2>/dev/null; then pass "and kills the processes its tests started"
  else fail "and kills the processes its tests started" "child pid: $CHILD"; kill "$CHILD" 2>/dev/null || true; fi
fi

run_tests munit-missing "$MUNIT_CP" "$(run_params '{frameworks: ["no.such.Framework"]}')"
expect "a named framework that is not on the classpath is a bad request" "$RUN_RESULT" \
  '.error == {code: "bad_request", message: "framework no.such.Framework is not on the classpath"}'
expect "and the runner exits 2" "$RUN_EXIT" '. == 2'

section "test runner: JUnit 4 via junit-interface $JUNIT_INTERFACE"
SJ="$WORK/junit"
compile junit@jvm-2.13 "$SCALA213" "$SJ" "$JUNIT_RUNTIME_CP"
expect "JUnit fixture compiles" "$R" '.result.status == "ok"'
run_tests junit "$JUNIT_RUNTIME_CP:$SJ/out/classes" "$(jq -nc --arg dir "$SJ/out/classes" '{target: "junit", testClassesDirs: [$dir]}')"
expect "JUnit tests are discovered by annotation and run" "$RUN_RESULT" \
  '.result.status == "failed" and .result.passed == 1 and .result.failed == 1'
expect "JUnit case names are the method names" "$RUN_EVENTS" \
  '[.[] | select(.t == "test.case") | [.suite, .name, .status]] | sort ==
    [["calc.CalcTest", "adds", "passed"], ["calc.CalcTest", "divides", "failed"]]'

section "test runner: utest $UTEST (object suites)"
SU="$WORK/utest"
compile utest@jvm-3 "$SCALA3" "$SU" "$(dep_part "$UTEST_RUNTIME_CP")"
expect "utest fixture compiles" "$R" '.result.status == "ok"'
run_tests utest "$UTEST_RUNTIME_CP:$SU/out/classes" "$(jq -nc --arg dir "$SU/out/classes" '{target: "utest", testClassesDirs: [$dir]}')"
expect "utest object suites are discovered by module fingerprint and run" "$(jq -sc . <<<"$RUN_RESULT $RUN_EVENTS")" \
  '.[0].result.status == "ok" and .[0].result.passed == 1 and
    any(.[1][]; .t == "test.suite.start" and .suite == "sums.SumTests")'

# --- Shutdown -----------------------------------------------------------------------------

section "shutdown"
rm "$BIG/out/analysis.zip"
send compile "$(compile_params big@jvm-3 "$SCALA3" "$BIG")"
RUNNING=$ID
until jq -e --argjson id "$RUNNING" 'select(.params.requestId == $id and .params.t == "compile.progress")' \
  "$WORK/worker.out" >/dev/null 2>&1; do sleep 0.05; done
send shutdown '{}'
expect "shutdown is acknowledged" "$(await "$ID")" '.result == {}'
expect "a compile running at shutdown is answered cancelled" "$(await "$RUNNING")" '.result.status == "cancelled"'
if responded_before "$RUNNING" "$ID"; then pass "the running compile is answered before shutdown"
else fail "the running compile is answered before shutdown"; fi
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
