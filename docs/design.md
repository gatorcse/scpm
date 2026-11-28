# scpm — design exploration

> Status: **exploration / RFC**. Written 2026-11-24. Name: `scpm` ("Scala package manager"), chosen after the collision check in
> [§13](#13-things-you-didnt-ask-about).
>
> Research inputs: pnpm 11/12 and aube docs, coursier 2.1.27 source and docs, bleep
> 1.0.0-M16, Metals v2.0.0-RC1, sbt 2.1, Mill 1.x, Zinc 2.1, JDK 25 Leyden JEPs, plus local
> measurements (marked *measured*). Claims we could not verify are marked **[unverified]**.

---

## 0. TL;DR — recommended decisions

| Question | Recommendation |
|---|---|
| Positioning | A **package-manager-first** workspace tool (pnpm/aube shaped) that also compiles, rather than a build tool that also resolves. bleep already ships much of the "fast build tool" half — see [§2](#2-landscape-and-what-bleep-already-did). |
| Config layout | **Split**: root `scpm-workspace.yaml` (members, catalogs, policy, tasks, pipelines, versioning) + one `scpm.yaml` per module (its deps, its tasks). Sharing via named **presets** + root-level **task defaults**. `scpm graph` / `scpm config --resolved` give back the single explicit view. |
| Config format | YAML with a published JSON Schema; unknown keys are errors with did-you-mean (pnpm 12 behaviour). |
| CLI language | **Rust.** It owns everything that does not need a JVM: config, graph, hashing, no-op detection, lockfile fetch, cache restore, reporting. |
| Compiler side | A **Scala JVM worker** (“`scpmd`”) that embeds **Zinc 2.1** directly — *not* Bloop. Narrow protocol: compile/test/run requests in, structured events out. All scheduling stays in Rust. |
| Daemon? | **Yes locally** (one shared per-user daemon per `(scpm version, worker JDK)`, idle-timeout, serves every workspace and worktree). **In CI, the same worker runs in the foreground for the life of the job** with a JDK 25 AOT cache. Zinc cannot be made fast enough cold: JIT warmup is 3.4–5× (*measured*). |
| Dependencies | **Coursier owns resolution and the cache.** Phase 1: spawn native `cs`. Phase 3: a resolver sidecar on the coursier API (JSON lines over stdio), shipped as a JVM jar; native-image later (§5.2.1). With it the lock becomes universal: locked for every standard system (OS and arch), each machine using its own (§5.4). Never reimplement Maven resolution in Rust. |
| Lockfile | **Yes**, our own (`scpm-lock.yaml`), because nothing in the Scala ecosystem has one. Restoring from the lock needs no resolver, only intransitive downloads with sha256 verification, so the Rust CLI does it itself from Phase 1, writing a coursier-compatible cache layout. |
| Semver ranges | Allowed **only in manifests, only for direct deps, resolved at lock time**. Published POMs never contain ranges. `^`/`~` are translated with Scala's early-semver rules and the dependency's `info.versionScheme`. |
| Agents | **`scpm mcp`** is a first-class front-end from Phase 1. It is a thin layer over `--json` and the event stream, owns a foreground worker in Phase 1 and uses the shared daemon from Phase 2, and is token-frugal by default (§9.3). |
| Reporting | Everything is an **event stream** (NDJSON). Reporters (tty, GitHub, JUnit, JSON, SARIF, MCP) are consumers. The plugin API is mostly “subscribe to events / contribute facts”. |
| IDE | Write **`.metals/mbt.json`** for Metals v2's fast path (Scala 2.12, 2.13 and 3; full speed needs Scala 3.3.8+ or 3.8.4+). Serve a **narrow, well-implemented BSP**. For IntelliJ, add `scpm idea` (generated `.idea` XML) early and a native ExternalSystem plugin in Phase 5, so we're not stuck with IntelliJ's weaker BSP experience (§10). |
| Worktrees | Shared daemon + shared coursier cache + **copy-on-write seeding** of build outputs and portable Zinc analysis from a sibling worktree or the remote cache: `scpm worktree add` gives a warm incremental build. |

---

## 1. Goals and non-goals

**Priorities (from the brief):** speed, reportability, DX, in that order of tie-breaking.

Concretely:

1. **Cold CI start is effectively free.** A no-op or cache-hit `scpm pipeline` in a fresh GitHub Actions runner never boots a JVM. Target: < 300 ms of tool overhead on a warm runner cache for a 50-module workspace.
2. **Every result is structured.** Diagnostics, test results, dependency facts and timings are data first and text second. GitHub annotations, step summaries, JSON for agents and SARIF are all just renderings of that data.
3. **Config is data, not code.** No compiling the build to learn what the build is. Any tool (Renovate, an agent, `jq`, an IDE) can read and edit it.
4. **Familiar to anyone who has used pnpm**: workspaces, `workspace:` protocol, catalogs, filters, `tasks` / `pipeline`, `change` / `version` / lanes.
5. **Ecosystem-compatible**: same coursier cache, same Maven repositories, publishes normal POMs, consumable by sbt/Mill/Gradle users.

**Non-goals:**

- sbt's arbitrary task/setting graph with scopes (axes) and in-process plugins.
- Being a general polyglot build system (Bazel/Pants). JVM languages first: Scala 2.13/3 and Java, Kotlin maybe later.
- Our own dependency resolution algorithm.
- A plugin ecosystem that runs inside the build process.

---

## 2. Landscape, and what bleep already did

The research surprised us: **bleep has moved a long way since Bloop.** As of 1.0.0-M16 (Nov 2026, still milestone, essentially one maintainer):

- GraalVM-native CLI. It **replaced Bloop** with its own Zinc daemon, `bleep-bsp`, one per (bleep version × JVM) shared by every workspace. The maintainer's PR [#496](https://github.com/oyvindberg/bleep/pull/496) claims ~12 ms to connect and ~200 ms for a no-op compile of 34 projects.
- Portable Zinc analysis (path markers in the style of sbt 2's `MappedVirtualFile`). On a 78-project build, clean → pull → compile went from 22 minutes to 16 seconds.
- `--invalidated=origin/main` change detection plus an S3 remote cache.
- GitHub `::error`/`::warning` annotations, `$GITHUB_STEP_SUMMARY`, JUnit XML, `-o json`, and an **MCP server**.
- A single `bleep.yaml` with templates, `extends`, and a `cross:` matrix; sbt/Maven import.
- Their documented pain: users accumulate stale multi-GB daemons, which led to `bleep server ls/top`, an idle timeout and a heap governor.

Other relevant prior art:

- **sbt 2.x**: `sbtn` native client by default, hermetic task cache, Bazel-compatible remote cache.
- **Mill 1.x**: native launcher, daemon by default, and declarative `build.mill.yaml` since 1.1.
- **scala-cli**: Bloop by default.
- **Metals v2** (RC1, Oct 2026): builds its own index and needs build tools mostly for *metadata* (`mbt.json`).

**What scpm does that bleep doesn't** (the reason to exist rather than contribute):

| | bleep | scpm |
|---|---|---|
| Mental model | Build tool | Package manager + workspace orchestrator (pnpm) that compiles |
| Config | One root file + templates | Workspace file + per-module manifests (pnpm) |
| Lockfile | None found **[unverified]**; user-level resolve cache | Committed `scpm-lock.yaml` with sha256, toolchain pinned too |
| Version ranges, catalogs, `workspace:` protocol | Not like pnpm | First-class |
| CI fast path | Native CLI → JVM daemon | **No JVM at all** on no-op/cache-hit paths (lockfile restore, remote cache, annotations all in Rust) |
| Remote cache | Explicit `pull`/`push` | Transparent inside `scpm pipeline` |
| Release management | — | changes / version / lanes / ledger, MiMa-checked bumps |
| Task graph | Fixed built-ins + scripts | pnpm-style `tasks:` with `^dep` edges, inputs/outputs caching |
| CLI language | GraalVM (lost Intel-mac binaries with GraalVM 25) | Rust (cross-compiles everywhere) |

**Open strategic question:** bleep is open source (licence **[unverified — check before reuse]**). Its `bleep-bsp` Zinc-daemon work, portable analysis, and sbt importer are directly reusable ideas, and possibly code. It is worth one conversation with its maintainer before writing a compile server from scratch.

---

## 3. Architecture

```mermaid
flowchart TB
    subgraph clients["Clients"]
        direction LR
        user["Developer / CI"]
        agent["AI agent"]
        ide["IDE: IntelliJ, Metals"]
    end

    subgraph cli["scpm: Rust, one static binary, no JVM"]
        direction TB
        front["Front-ends: CLI, MCP server, BSP"]
        core["Config + module graph<br/>Task scheduler<br/>Hashing and no-op detection"]
        deps["Lockfile<br/>Locked-artifact fetcher"]
        out["Event log (NDJSON)<br/>Reporters<br/>Remote cache client"]
        front --> core
        core --> deps
        core --> out
    end

    subgraph jvm["Out-of-process helpers"]
        direction LR
        resolver["Resolver<br/>Phase 1: cs CLI<br/>Phase 3: JVM sidecar on the coursier API<br/>Runs only when the lock is stale"]
        scpmd["scpmd: Scala on the worker JDK (25+)<br/>Zinc 2.1, scalafmt, scalafix, JS/Native linkers<br/>Shared daemon locally, foreground in CI"]
        testjvm["Forked test/run JVM<br/>project JDK, sbt test-interface"]
    end

    subgraph disk["Shared on-disk state"]
        direction LR
        cscache[("Coursier cache<br/>shared with sbt, Mill, scala-cli")]
        outdir[("Per-worktree .scpm/out<br/>classes + portable Zinc analysis")]
        taskcache[("Task cache<br/>local + remote")]
    end

    clients --> front
    deps -- "spawn, JSON over stdio" --> resolver
    core -- "JSON lines: stdio (Phase 1), unix socket (daemon)" --> scpmd
    core -- "spawn" --> testjvm
    scpmd -- "events stream back" --> out
    testjvm -- "events stream back" --> out
    resolver --> cscache
    deps --> cscache
    scpmd --> outdir
    out <--> taskcache
```

Only the Rust box runs on every command. Everything in "Out-of-process helpers" starts only when the work needs it (§3.2).

### 3.1 Division of labour

**Rust CLI (`scpm`) does everything that does not need a JVM:**

- parse and validate config, expand presets and catalogs, build the module graph
- read the lockfile; fetch locked artifacts directly (HTTP + sha256) into the coursier cache layout
- fingerprint inputs (git blob OIDs where possible, as bleep and Metals v2 do) and decide no-ops
- schedule tasks; restore and push the remote cache; run shell tasks
- render every report; serve BSP and MCP front-ends

**JVM worker (`scpmd`) is deliberately dumb.** The request is "compile this module with this classpath, these sources and these options, analysis file here, mapping roots here". The worker streams diagnostics and progress events back, then returns the analysis and products. It has no notion of a workspace graph. The wire format is [docs/worker-protocol.md](worker-protocol.md) (JSON lines); the CLI normalises worker events into the §9.1 event schema (workspace-relative paths, `edit` shape, categories). Rust can therefore:

- swap the worker implementation (embedded Zinc today; a Leyden or CRaC snapshot tomorrow);
- run several workers;
- and keep the worker reusable by other tools.

This is the Pants/Bazel shape: a native orchestrator with persistent JVM workers, which is proven at scale.

**Resolver.** Coursier, out of process (see [§5](#5-dependencies-and-coursier)). It is only needed when the lockfile is stale, so in CI with `--frozen` it is never invoked.

### 3.2 Hot paths, and which ones touch the JVM

| Command | JVM? | Notes |
|---|---|---|
| `scpm install` / `scpm ci` with up-to-date lock | No | Rust fetches missing jars by URL + sha256. No POMs, no resolution rounds (§5.2). Phase 1 gets the JDK with `cs java-home` (native, cached). |
| `scpm install` with stale lock | `cs` (native); the resolver sidecar from Phase 3 | Coursier resolves and downloads. 40–130 ms warm *measured* with `cs` (cats-effect+circe → spark-sql); the sidecar's cold and warm costs are in §5.2.1. |
| `scpm compile` no-op | No | Fingerprint check in Rust; bleep's ctime-manifest trick (5 s → 5 ms per project). |
| `scpm pipeline` all cache hits | No | Restore outputs + replay logs/annotations from the remote cache. |
| `scpm compile` with changes | Yes | Daemon locally; foreground worker in CI. |
| `scpm test` | Yes | Forked JVM per test run (needed for isolation anyway). |
| `scpm graph`, `scpm why`, `scpm outdated`, `scpm deps --json` | No / `cs` (the sidecar from Phase 3) | |

---

## 4. Configuration model

### 4.1 Single root config vs per-module manifests

| | Single root (sbt, bleep, Mill YAML) | Split (pnpm, Cargo, aube) |
|---|---|---|
| “Show me the whole build” | One file | Needs a command (`scpm graph`, `scpm config --resolved`) |
| Locality / ownership | Everything in one hot file: merge conflicts, CODEOWNERS can't be scoped | Deps sit next to code; CODEOWNERS per module; less churn on a shared file |
| Agents | Must edit a large shared file | Edit the module they are working in |
| Moving or copying a module | Edit two places | Move a directory |
| Startup cost | 1 parse | N parses — irrelevant in Rust (~hundreds of small YAML files in low ms), and cacheable by mtime |
| Shared settings | Templates in the same file | Needs an explicit sharing mechanism (below) |
| Familiarity for our target users | sbt/bleep users | pnpm/Cargo users |

**Recommendation: split, pnpm style.**

- **Root `scpm-workspace.yaml`** owns workspace-wide concerns:
  - members (globs)
  - catalogs, overrides
  - repositories and credentials references
  - toolchain defaults
  - presets
  - task definitions and pipelines
  - versioning and supply-chain policy
- **Each module's `scpm.yaml`** owns only its identity, its sources, its dependencies (including other workspace modules), its compiler and run settings, and its own task overrides.

The explicitness concern ("A depends on B, here are all my projects") is kept two ways:

1. **Inter-module edges are still declared explicitly** in the dependent's manifest, as pnpm does with `workspace:`.
2. Members are an explicit glob list in the root (or a literal list if you prefer). `scpm graph` prints the resolved whole.

A lone `scpm.yaml` with no workspace file is a valid single-module project, just as a lone `package.json` is.

`scpm init [DIR]` writes the starting point. Outside a workspace it writes `scpm-workspace.yaml` (Scala and munit in the default catalog, `modules: [modules/*]`), one app module `modules/app` with a `Main` and a munit suite, and `.scpm/` in `.gitignore`. Inside a workspace it adds a library module at DIR, which `modules:` must already match (it does not edit the globs). It never overwrites a file, and it loads the result back as a workspace before returning, removing what it wrote if that fails. It resolves nothing; the first `install`, `compile` or `run` does.

### 4.2 Sketch: workspace file

```yaml
# scpm-workspace.yaml
$schema: https://scpm.dev/schema/workspace-1.json
scpm: 0.4.2                       # Phase 3: pinned tool version; older/newer binaries re-exec this one

modules:
  - modules/*
  - apps/*
  - '!**/fixtures/**'

organization: com.acme           # default groupId for publishing

toolchain:
  jdk: temurin:25                # project JDK: release target, runs tests and `run` (§6.3); workspace-only
  scala: catalog                 # default Scala version for modules (key `scala` in the default catalog)

repositories:
  - central
  - id: acme
    url: https://acme.jfrog.io/artifactory/maven
    credentials: env:ARTIFACTORY_TOKEN     # bearer token; or { user: env:U, password: env:P } (§5.7)
    # allowInsecureCredentials: true       # only for an http:// repository that must get credentials (§5.7)
    groups: [com.acme]                     # com.acme* comes only from here, and nothing else does (§5.4)

lock:
  # systems: [linux-x86_64, macos-aarch64]   # unset: scpm's default set (the standard five from M2); writing a set out
  #                                          # opts out of default tracking and the M2 upgrade (§5.4 "Systems")
  jdk: 25                        # the default: the project JDK's major version

ide: { metals: true }            # regenerate .metals/mbt.json on every `scpm install` (also `idea: true`); §10

catalog:                         # default catalog → referenced as `catalog`
  scala: 3.9.0                   # the key is the field's natural key: `scala` here, the coordinate for deps
  org.typelevel::cats-core: 2.13.0
  org.typelevel::cats-effect: ^3.6.0
  org.scalameta::munit: 1.2.1
  com.fasterxml.jackson:jackson-bom: 2.20.1       # BOMs are catalog entries too (see `boms`)

catalogs:
  scala213:
    scala: 2.13.18
    org.typelevel::cats-core: 2.12.0
  spark:
    org.apache.spark::spark-sql: 3.5.1
  aws:
    software.amazon.awssdk:bom: ^2.31.0

boms:                            # applied to every module (Maven BOM import semantics via coursier)
  com.fasterxml.jackson:jackson-bom: catalog      # coord → spec, like dependencies: exact, range,
  software.amazon.awssdk:bom: catalog:aws         #   `catalog` or `catalog:<name>` (looked up by the
  io.netty:netty-bom: 4.1.115.Final               #   BOM's coordinate); the lock pins the version

overrides:                       # forced versions, workspace-wide (coursier --force-version)
  com.google.guava:guava: 33.4.0-jre

presets:                         # named, mergeable fragments; modules opt in with `extends:`
  library:
    scalacOptions: [-deprecation, -feature, -Werror]
    publish: true
    versionScheme: early-semver
  cross-213-3:
    scala: [catalog, catalog:scala213]    # a list = cross-build over these Scala versions

tasks:                           # pnpm-style task defaults, inherited by every module
  compile: { dependsOn: ['^compile'] }   # built-in tasks need no `run:`
  test:    { dependsOn: [compile], outputs: [] }
  lint:    { outputs: [] }
  fmt:check: { outputs: [] }
  lint:strict:                   # custom task reusing a built-in with arguments
    run: builtin:scalafix --check --config .scalafix-strict.conf
    outputs: []

pipelines:
  default: [fmt:check, compile, lint, test]
  release: [compile, test, mima]

pipelineBase: origin/main

policy:
  minimumReleaseAge: 1d          # see §5.6
  versionConflicts: error        # sbt-style eviction errors using info.versionScheme

versioning:
  changes: .changes
  fixed: [[core, core-testkit]]
  lanes: { api-next: rc }
```

### 4.3 Sketch: module manifest

```yaml
# modules/core/scpm.yaml
name: core                       # artifactId = core_3 on publish (core_sjs1_3 for Scala.js)
extends: [library, cross-213-3]
platforms: [jvm, js]             # variants = platforms × scala versions → core@jvm-3, core@js-2.13, …

dependencies:
  org.typelevel::cats-core: catalog
  io.circe::circe-core: ^0.14.10             # range: resolved at lock time, see §5.5
  util: workspace:*                          # another module in this workspace
  com.acme:auth-client: 3.2.1                # plain Java artifact (single colon)

providedDependencies:
  org.apache.spark::spark-sql: catalog:spark

testDependencies:
  org.scalameta::munit: catalog

boms:                            # added to the workspace's; same coordinate → the module's wins
  com.acme:platform-bom: 2026.11.0 # (workspace BOMs aren't in the merge chain: `!replace` can't drop them)

javacOptions: [-parameters]
ide: { primary: jvm-3 }          # the variant `scpm idea` makes the IntelliJ module (default: the first); §10.2
generatedSources: [src_managed]  # Phase 3: dirs under ${outDir} that tasks write; compiled with the sources

variants:                        # per-variant overrides, keyed by variant selector, only when needed
  '2.13':                        # matches every variant on Scala 2.13 (also: 'js', 'jvm-3', …)
    scalacOptions: [-Xsource:3]
    compilerPlugins:             # coord → spec; added as -Xplugin
      org.typelevel:::kind-projector: 0.13.3

tasks:
  generate:                      # module-local task; codegen before compile
    run: scpm run codegen -- --out ${outDir}/src_managed
    outputs: ['${outDir}/src_managed/**']
  compile: { dependsOn: ['^compile', generate] }
```

```yaml
# modules/codegen/scpm.yaml
name: codegen
scala: none                      # Java-only: no Scala suffix, no scala-library; one variant, codegen@jvm
release: 17                      # Java release target; default: the project JDK's major version
mainClass: com.acme.codegen.Main
jvmOptions: [-Xmx1g]             # forked `run` / `test` JVMs
env: { CODEGEN_STRICT: 'true' }

dependencies:
  com.squareup:javapoet: 1.13.0
```

Notes:

- **Dependency syntax** keeps the Scala convention (`::` cross-version, `:::` full version, `:` plain Java). Keys are coordinates and values are version specs, so Renovate and agents can edit a single line. Bare names (`util:`) refer to workspace modules.
- **Scopes**: `dependencies`, `providedDependencies` (compile-only), `runtimeDependencies`, `testDependencies`. Four is deliberately small; no Ivy configurations.
- **Catalog references** look up the field's natural key: for a dependency, its coordinate (`catalog` → default catalog, `catalog:spark` → the `spark` catalog). The default catalog is written as a bare `catalog` (also accepted: `catalog:default`, or a quoted `'catalog:'`) because a trailing `:` is not valid in a YAML value (pnpm can use `catalog:` only because package.json is JSON). For `scala`, the natural key is `scala`. A catalog entry may itself be a range (`^3.6.0`); it is resolved once per (platform, Scala binary version) for the whole workspace (§5.4). A variant that needs a different version uses a `variants` override:
  ```yaml
  variants:
    '2.13':
      dependencies:
        org.typelevel::cats-core: catalog:scala213   # looks up org.typelevel::cats-core in catalogs.scala213
  ```
- **Source layout** is sbt/Maven's by default: `src/main/scala`, `src/main/java`, `src/main/resources`, `src/test/scala`, `src/test/java`, `src/test/resources`. Version- and platform-specific directories (`src/main/scala-2`, `scala-3`, `scala-2.13`, `scala-jvm`, `scala-js`, `scala-native`, and their `src/test/…` twins) are added automatically for the variants they match. `sources`, `resources`, `testSources` and `testResources` (lists of directories relative to the module) replace the defaults. They may point anywhere inside the workspace (a shared `../shared` directory is a common sbt layout); absolute paths and paths that leave the workspace root are errors.
- **`${outDir}`** is `.scpm/out/<variant-id>/`. `generatedSources` lists directories relative to it; tasks write them and they join the compile sources. Because tasks arrive in Phase 3, so does `generatedSources`; until then both are "not supported yet". In the sketches above, `tasks`, `pipelines`, `pipelineBase`, `generatedSources`, `policy` and `versioning` are later-phase keys.
- **`scala: none`** marks a Java-only module: no suffix, no scala-library, variant id `<name>@jvm`.
- **`compilerPlugins`** maps a coordinate to a spec, like dependencies. `:::` coordinates are resolved full-cross (`_<full Scala version>`). Every plugin jar is passed as `-Xplugin:<jar>`, Scala 3 plugins included.
- **Java and run settings:** `javacOptions`; `release`, the Java release target (default: the project JDK's major, §6.3); `mainClass`; `jvmOptions` and `env` for forked `run` and `test` JVMs.
- **Exclusions and classifiers** use an expanded object form when needed:
  ```yaml
  dependencies:
    org.foo:bar:
      version: 1.2.3
      exclude: [org.slf4j:*]
      classifier: linux-x86_64
  ```

### 4.4 Sharing from parent to child

pnpm has almost no inheritance; it relies on catalogs and root-level settings. We need slightly more because of scalac options and cross builds. Four mechanisms, in priority order:

1. **Catalogs.** Versions, defined once and referenced everywhere. Named catalogs replace sbt's `val catsVersion = …` idiom.
2. **Root `tasks:`.** Defaults inherited by every module and overridable per module (pnpm 12 semantics: an unconfigured task defaults to `dependsOn: ['^self']`, where `^self` means the same task in each upstream module).
3. **Presets + `extends:`.** Named fragments merged into the module in order, by the rules below. A `!replace` YAML tag on a value (`scalacOptions: !replace [-deprecation]`) replaces instead of merging. This is bleep's templates idea, but the presets live in the root and are referenced from leaves.
4. **Config dependencies** (later). Presets and catalogs published as a Maven artifact (`com.acme:scpm-presets:1.4.0`) and pinned in the lockfile's boot section, so an org can share them across repos. This mirrors pnpm's `configDependencies`.

Directory-based implicit inheritance (child configs silently picking up parent-directory config) is **not** supported: everything a module gets is visible from its own file plus `extends:`. `scpm config core --resolved [--json]` prints the fully expanded result, and `scpm config core --explain scalacOptions` shows where each value came from (as `aube config explain` does).

#### Effective variant config (normative)

**Order.** A variant's effective config is built by merging, later over earlier:

1. toolchain defaults (`toolchain:` in the workspace);
2. presets, in `extends` order. A preset may itself have `extends`; expansion is depth-first, each preset's own `extends` applied before the preset, and a preset already applied is skipped. A cycle is an error;
3. the module file;
4. each `variants` entry whose selector matches the variant, in file order (later wins). Entries contributed by presets come first; an entry repeated under the same selector merges into the first.

**Merge rules.**

| Value | Rule |
|---|---|
| Map | Merge by key, recursively. |
| Scalar | Later replaces earlier. |
| List | Append, **without** deduplication. |
| Set-like list: `extends` | Append, deduplicated (first occurrence kept). |
| `scala`, `platforms` | Always **replace** (`platforms` is also deduplicated). They define the matrix, so a module's `scala:` overrides a preset's list rather than growing it. |
| Any value tagged `!replace` | Replaces. |

**Where keys may appear.**

- A preset may contain any module key except `name`.
- A `variants` entry may contain any module key except `name`, `extends`, `platforms`, `scala`, `variants` and `ide`.
- `toolchain.jdk` is workspace-only in v1. A module sets its Java target with `release`.

**Variant ids** are `<module>@<platform>-<scala binary>` (`core@jvm-3`, `core@js-2.13`), or `<module>@jvm` for a Java-only module. A module's `scala` list may therefore hold at most one version per binary version; `[3.8.4, 3.9.0]` is an error.

**Selectors** (keys of `variants`, and the part after `@` in filters) have the form `<platform>`, `<scala>` or `<platform>-<scala>`, where `<platform>` is `jvm`, `js` or `native`, and `<scala>` is a Scala binary version (`3`, `2.13`) or a full version (`3.9.0`, `3.8.0-RC1`, which may itself contain `-`). A variant matches if every given part matches; a selector that matches no variant is an error. In filters, `-F core` matches every variant of `core`, and globs work (`-F 'core@jvm-*'`).

### 4.5 Cross-building

This is Scala-specific and has no pnpm analogue. A module's variants are `platforms × scala versions`, optionally pruned with an `exclude` list. A variant id looks like `core@jvm-3` or `core@js-2.13`; a Java-only module (`scala: none`) has one variant, `codegen@jvm` (id and selector grammar in §4.4).

- `workspace:` edges match variants by platform and Scala binary version, failing with a clear error when no match exists. An edge to a Java-only module matches any JVM variant.
- The lockfile has an entry per variant, resolved jointly with its upstream workspace modules (§5.4).
- Filters understand variants: `scpm test -F core@jvm-*`; `-F core` selects every variant of `core`.

### 4.6 Format choice

**YAML** with a published JSON Schema. Reasons:

- pnpm, aube and bleep use it, so our audience already knows it;
- editors give completion from the `$schema`;
- Renovate understands YAML.

TOML is the main alternative (Cargo, Mill YAML aside) and is arguably safer: there's no Norway problem, so `no` and `3.10` stay strings. We would mitigate YAML's pitfalls by treating every version field as a string in the schema and rejecting unquoted floats.

Unknown keys are **errors** with did-you-mean suggestions, matching pnpm 12's `ERR_PNPM_UNRECOGNIZED_WORKSPACE_SETTINGS`.

**The v1 schema is defined up front** and accepted from Phase 1. A key for a feature that isn't implemented yet is an error — "`<key>` is not supported yet (planned for Phase N)" — whenever it would change behaviour (`platforms: [jvm]` is fine in Phase 1; `platforms: [jvm, js]` is not). It is never silently ignored.

---

## 5. Dependencies and coursier

### 5.1 What coursier gives us (and what it doesn't)

**Coursier gives us** (verified against 2.1.27 source and docs):

- Maven and Ivy resolution with ~10 years of edge cases handled: parent POMs, properties, `dependencyManagement`, profile activation, relocations.
- **BOMs**: `--bom`, `Dependency.addBom`.
- Forced versions with patterns; exclusions; classifiers and types.
- Scala `::` and `:::` coordinates with platform suffixes for JS and Native.
- Reconciliation strategies and strict rules.
- Maven ranges and Ivy dynamic versions.
- Gradle Module Metadata (experimental).
- Offline mode; mirrors including `settings.xml`; credentials; proxies.
- Parallel downloads with retry/backoff and 429 handling.
- The shared cache used by sbt, Mill, scala-cli and bleep.
- JVM management (`cs java`, JVM index v2 with checksums).

**Coursier does not give us:**

| Missing | Notes |
|---|---|
| A lockfile | [coursier#1487](https://github.com/coursier/coursier/issues/1487) is still open; neither sbt, Mill, scala-cli nor bleep has one. |
| SHA-256 in its report | The report has no checksums or repository IDs. |
| A non-JVM API | No C ABI, no RPC. |
| npm-style ranges and semver conflict policy | Coursier reads `info.versionScheme` but doesn't enforce it. |
| “outdated / update” UX | |
| Publishing | `coursier/publish` is archived; it now lives on as `scala/scala-publish`. |
| Version-range unions | Only the last interval is kept. |

### 5.2 Using coursier from Rust

| Option | Verdict |
|---|---|
| Spawn native `cs` and parse `fetch -j` JSON | **Phase 1.** Zero build cost. `cs fetch` gives the graph as JSON; `cs resolve` is text only; versions come via `cs complete-dep org:name:` (text). There are hidden but real flags: `--bom`, `-V`, `--intransitive`, `--reconciliation`, `--strict`, `--rule`, `--checksum`, `-m offline`. The limitation is that errors are only stderr text. |
| Own resolver sidecar: a small Scala 3 app on the coursier API (`worker/resolver`), JSON lines over stdio | **Phase 3, as a JVM jar** (its jars from GitHub Releases for now, through a list built into scpm); GraalVM native-image is a later swap behind the same protocol. Structured errors, the repository and file of every artifact, `groups` enforced inside resolution, the whole resolution context (OS, arch, JDK) for every system of a universal lock, version listing and the dependency graph; it can stay alive for a session. Design and measurements: §5.2.1. |
| Host the resolver inside `scpmd` | A fine alternative once the daemon exists; but resolution should not *require* a warm JVM in CI, so keep the sidecar. |
| JNI or GraalVM `--shared` C library | Possible but adds little over a sidecar process. Skip. |
| Reimplement in Rust | **No.** The long tail costs months: profiles, BOM import cycles, relocations, Gradle variant matching, coursier's deliberate latest-wins semantics. The result would also diverge from sbt/Mill/scala-cli on the same inputs. Existing Rust crates (`jbuild`, `deps-maven`, `mvn`) are immature. |

The **one thing Rust does natively is restoring from the lockfile**, which is not resolution: it is "download these N files and verify sha256" into the coursier cache layout. It ships in **Phase 1**, behind a compatibility test suite that runs `cs` and the Rust fetcher concurrently on one cache.

- URL: the configured URL of the artifact's `repo` id (§5.4), with mirrors applied, plus the Maven path;
- path: `<cache>/https/<host>/<path>`, with coursier's escaping rules;
- write `.part` then rename;
- take POSIX `fcntl` locks to interoperate with the JVM's `FileChannel.lock` (**[verify on macOS]**);
- write no checksum sidecars: the sha256 from the lock is the check, and coursier fetches `.sha1` files on demand when it next verifies a file; files already in the cache are trusted when their size matches the lock, and `scpm install --verify` re-hashes them (re-downloading any that don't match);
- retry 429/5xx, transport errors and truncated bodies (a byte count other than the locked `size`), honouring `Retry-After` up to 30s;
- authenticate with the repository's `credentials` and, for repositories without them, coursier's own credential sources (§5.7), putting the matched user in the cache path (`<protocol>/<user>@<host>/…`) as coursier's `FileCache` does.

Release artifacts are immutable, so this is safe. Changing (`-SNAPSHOT`) artifacts are always delegated to coursier.

#### Who downloads what, and why

There are two cases, and **no single command chains a coursier resolve into a Rust download**. So there is no cross-tool waterfall.

| Case | When | Who does it | Requests |
|---|---|---|---|
| **Lock is stale** | `scpm add`, `scpm update`, a manifest edit | Coursier, resolving *and* downloading in one `cs fetch`. Rust only hashes the jars it got (local disk, no network) to write sha256 into the lock. | All POMs (with parent and BOM chains), then all jars |
| **Lock is fresh** | CI, a fresh clone, a new worktree, switching branches | Rust alone, from the coordinates, repository ids and sha256 values in the lock. No resolution at all. | The host system's jars only (§5.4, "Systems"), in one parallel burst |

**What coursier actually does** (verified in the 2.1.27 source: `Fetch.scala`, `ResolutionProcess.scala`):

- **No overlap between resolution and jar downloads.** `Fetch` resolves completely, then starts downloading jars.
- **Resolution runs in rounds with a barrier.** Each round fetches every missing POM in parallel; the next round starts only when the slowest one finishes.
  - Parent POMs cost one round per level.
  - Imported BOMs are only discovered once the parents are in, so they cost more rounds.
  - The critical path is roughly the deepest dependency path × (1 + parent depth + BOM depth) round-trips.
- **Concurrency defaults to 6 downloads.**
- **There is no "just download these" mode.** `--intransitive` still fetches each POM and its parents, and the `url=` dependency parameter still sends a HEAD request per artifact.

That serial depth is inherent to Maven resolution, and it's the real cost in a cold CI run. The lockfile removes it: every URL is known up front, so Rust can:

- **Skip POMs entirely.** That's roughly half the requests, and all of the serial rounds.
- **Use high concurrency over HTTP/2.** Many streams per host instead of 6 connections.
- **Verify sha256 from the lock.** Coursier only checks checksums that the repository serves, usually SHA-1.
- **Overlap downloads with other startup work**: starting `scpmd`, restoring the remote cache.

The POMs Rust skipped are not missing from anything that needs them. Coursier fetches them on demand the next time it resolves, and the coursier cache stays valid either way.

**Optional optimisation for the stale-lock case: speculative prefetch.** When the lock is stale, most entries in the *old* lock are still right. Rust can start downloading those jars while coursier resolves, and whatever coursier then asks for is already cached. Wrong guesses cost bandwidth but never correctness.

**Why not have coursier do the fresh-lock case too?** It can't do it without POMs from the CLI. It could through the library (`cache.file(artifact)` for each artifact, gathered), which the resolver sidecar (§5.2.1) could expose. But Rust already has the lock in memory, needs the sha256 checks anyway, and starting the fetch costs it no process spawn. The Phase 0 spike should measure a cold `cs fetch` against a Rust lock restore on a GitHub runner to quantify the gap.

#### 5.2.1 The resolver sidecar

> Status: **M1 implemented** (opt-in with `SCPM_RESOLVER=sidecar`; "As implemented (M1)" near the end of this section), with a measurement spike (2026-11-26, below). Decided: a **JVM jar first**; native-image later. The user's decisions of 2026-11-27 (bootstrap, distribution, bearer tokens, TLS trust, cold cost, M1 scope) are recorded where they apply and listed at the end.

The sidecar replaces `cs` for everything that resolves: re-locks, boot entries, `why`, and version listing. The Rust fetcher keeps restoring from the lock (§5.2), and no frozen CI run starts the sidecar.

**Why not keep `cs`.** Phase 1 gets correct results from `cs` 2.1.27, but only with workarounds that exist because a CLI reads its own configuration and takes only flags:

| `cs` limitation | Phase 1 workaround | With the sidecar |
|---|---|---|
| Reads ambient config: env, `settings.xml`, mirror files, Scala CLI config | Sealed environment: an allowlist of `COURSIER_*`/`CS_*` variables, `CS_MAVEN_HOME` pointing at an empty directory, `SCALA_CLI_CONFIG` and `COURSIER_MIRRORS` naming missing files (§5.9) | A minimal environment, and every setting passed explicitly to the API (below) |
| Two launcher kinds take JVM options differently | A launcher probe: `-J-D…` arguments for the native launcher, `JAVA_TOOL_OPTIONS` for the jpackage one | scpm starts the JVM itself |
| Credentials only from files | A private 0600 credentials file (§5.7) | Credentials in the request, never on disk; matched by origin as today (below) |
| No per-repository group filter | Grouped repositories first; violations are refused after the fact (§5.4) | `groups` enforced during resolution |
| Profile activation by OS/arch follows the machine | One system per lock, and only the JDK part of its context applies (§5.4) | OS, arch and JDK come from the request, for every system in the lock: the universal lock (§5.4, "Systems") depends on this |
| Text output (`resolve`, `complete-dep`), stderr errors | Parsing trees and ANSI codes; errors are stderr text | Structured results and error codes |

**Packaging.**

| | Decision | Why |
|---|---|---|
| Project | `worker/resolver`, a third sbt project in `worker/build.sbt`; Scala 3.9 like the compile worker; main class `scpm.resolver.Main`; runtime dependencies in `worker/deps/resolver.txt` | One build and one dependency-file convention for all JVM helpers |
| Coursier | `io.get-coursier:coursier_3:2.1.27` | The same version as the pinned `cs`, so the compatibility test (Migration, below) compares coursier with itself. 2.1.25 was the first release with `_3` artifacts, so no Scala 2.13 module is needed (only `scala-library` 2.13.18, which Scala 3 uses anyway). Its closure already has `jsoniter-scala-core_3` 2.43.0, the version the compile worker uses, so the JSON code comes free. Coursier is upgraded by changing `deps/resolver.txt` and the compatibility pin together; once `cs` is gone, by the golden-lock tests alone |
| Distribution | **GitHub Releases assets on `gatorcse/scpm`**, for now: every jar of the resolver's closure is uploaded to the scpm release, and the built-in list (below) holds each jar's URL, sha256 and size. The Rust fetcher downloads them with the machine's proxy and verifies the sha256 | Public, no authentication, one host. GitHub Packages' Maven registry (`maven.pkg.github.com`) needs a token even to download public packages, so every user would need a PAT; Releases assets are used instead. **Later:** publish to Maven Central, which changes only the built-in list's URLs (repositories, mirrors and `groups` then apply as for any artifact) |
| JDK | The JDK the compile worker detects (`detect_java`: `SCPM_JAVA`, then `JAVA_HOME`, then `PATH`, reading the JDK's `release` file). **Minimum JDK 17** (Scala 3.9's minimum); 25 or newer for the AOT cache. An older JDK fails with a message naming the version found and how to point `SCPM_JAVA` at another | No JDK download and no `cs java-home` |
| Development | `SCPM_RESOLVER_CLASSPATH` overrides the closure, for running a local build | Like the worker's local jars |

**Bootstrap: who resolves the resolver?** Today `cs` resolves every boot closure, and the lock pins it afterwards; a pinned closure needs no resolver. The resolver's own closure can't be resolved by the resolver, so **each scpm release carries that closure as a built-in list** (decided): URL, sha256 and size per jar, generated when the release is built. Only the list is compiled in; the jars stay on the release.

- The list is restored directly and is **not stored in the lock**. It belongs to the scpm binary, not the workspace, so an scpm upgrade causes no lock churn and `bootInputs` doesn't cover it. Frozen CI never fetches it, because it never starts the sidecar.
- The jars go into scpm's cache (`~/.cache/scpm/resolver/<sha256>.jar`), not the coursier cache: they aren't Maven artifacts at a repository URL. The fetcher re-hashes nothing it has already verified. `SCPM_RESOLVER_CLASSPATH` replaces the whole list for development, or for a network that can't reach GitHub.
- **The download never sends credentials**, whatever the user config's entries match: the assets are public.
- **Hosts:** `github.com` and the host it redirects asset downloads to (`release-assets.githubusercontent.com`, formerly `objects.githubusercontent.com`). Users behind a proxy, `noProxy` rules or a host allowlist need both. Risk 4 and `scpm config explain` name them.
- **`resolverMirror:`** in the user config (§5.9) is an optional base URL that replaces the release URL; the jar file names are appended to it (default; user may revisit). It is safe because every jar stays pinned by the list's sha256. It must allow anonymous access, because no credentials are sent to it.
- **`SCPM_RESOLVER_CLASSPATH`** drops the pins, so scpm prints a notice on every command that uses it.
- **Release order.** The release workflow publishes the GitHub release, with its assets, before any binary that names those assets is distributed: a draft release's assets answer 404.
- The chain is then: built-in list → Rust fetcher → resolver JVM → every other closure (compiler, bridges, worker, test runner, tools, variants).
- Rejected: keeping `cs` only to resolve the resolver (that keeps all of `cs`'s sealing), and bundling the jars in the binary (against distributing JVM helpers as jars).

**JVM start-up.** The one-shot sidecar starts with the worker spike's recipe (JDK 25 AOT cache, C1 only):

- **AOT cache** (JEP 483/514/515). The first run with a new key is the training run (`-XX:AOTCacheOutput`), which writes to a temporary path that is renamed into place, so concurrent scpm processes never read a half-written cache. Training costs about 1 s once (1.25 s through `mise exec` against 0.37 s without training, M2 Pro), and the cache is about 28 MB. It lives in `~/.cache/scpm/aot/resolver-<key>.aot`. The key covers the scpm build, the **exact classpath string**, the JDK build and the JVM flags, as for the worker. The `setup-scpm` action caches it. A detected JDK older than 25 runs without a cache. M1 runs without an AOT cache (an M1 cut, below); it arrives with M2.
- **`-XX:TieredStopAtLevel=1`** (C1 only) for one-shot runs. A kept-alive sidecar runs tiered by default. In the spike C1 was faster even on the 10th warm request, so the default should be revisited after measuring a large graph (spark-sql).
- Always `-XX:+DisplayVMOutputToStderr -Xlog:disable -Xlog:all=warning:stderr -XX:+ExitOnOutOfMemoryError` (stdout carries protocol JSON only, as for the worker), a modest `-Xmx` (512m to start with), and probably `-XX:-UsePerfData` (no `hsperfdata` file) and `-XX:+DisableAttachMechanism` (no attach socket while it holds credentials). The stable private `user.home` (below) is also part of what the AOT cache sees.

**Native-image later.** The protocol is the only contract, so a native-image build of the same sources (coursier ships its own native-image configuration) can replace the jar without any Rust change except how the process starts. It would be published per OS and arch, a classifier per `os-arch` with type `exe`. The spike says when that is worth it: native `cs` answers in 31 ms on the Mac, where a cold JVM sidecar with the AOT cache and C1 needs 164–194 ms (table below), so the gap is about 160 ms on every command that re-locks. Frozen CI never pays it. **Decided:** that cold cost is acceptable for now, so native-image waits.

**Protocol.** It uses the [worker protocol](worker-protocol.md)'s envelope: JSON lines on stdin/stdout, `{"id", "method", "params"}` → exactly one `{"id", "result"}` or `{"id", "error"}`, `event` notifications, unknown fields ignored, and free-form logs on stderr only. Requests may run concurrently.

| Method | Params | Result |
|---|---|---|
| `initialize` | `protocolVersion` (1), `proxy`, `trust` (`{pemBundle}`: on Linux the system PEM bundle's path, below; `null` on macOS and Windows, which use their OS stores) | `resolverVersion`, `coursierVersion`, `javaVersion`, `pid`. A version mismatch is `unsupported_protocol`; Rust and the resolver ship together, so that only happens with a development override |
| `resolve` | `repositories`, `cache`, `ttl`, `refresh`, `offline`, and `requests`: one per classpath to resolve, each with a `label` (`core@jvm-3 testRuntime`, `boot scala-compiler@3.9.0`), `systems` (a list), `jdk`, `natives`, `roots` (`{module, version, classifier?, type?, exclusions[], transitive}`), `boms`, `forced` (`org:name:version`) and `artifactTypes` | One entry per request, in order: either `classpath`, merged across the request's systems (below), or `error` (below). One bad variant doesn't hide the others |
| `listVersions` | `repositories`, `cache`, `ttl`, `refresh`, `module` (`org:name`) | `versions` (ascending) and, per allowed repository, `{id, versions}` |
| `graph` | as one `resolve` request, with a single `system`; resolves metadata only, downloads no artifacts | The resolved graph: `nodes` (`{module, version}`) and `edges` (`{from, to, requested, excluded?}`), with evictions (`requested` ≠ selected). `why` computes chains in Rust, which replaces parsing `--what-depends-on` trees, and can also say "requested 2.9.0, got 2.13.0" |
| `shutdown` | — | `{}`, then exit. End of stdin exits too, so the sidecar never outlives scpm |

- **A classpath entry** is structured: `{group, artifact, version, classifier?, extension, url, file, size, repository}` (`size` is not in M1). Rust keeps its single key function (`artifact_key`, which uses the file's extension rather than the Maven type) to build the lock's `group:artifact:version[:classifier][@type]`, so `cs` and the sidecar can't disagree about keys. `repository` is the id of the repository that served the module (below).
- **`repositories`** is the whole configured list, in declared order: `{id, url, groups?, auth?}`. `url` is the URL actually contacted, with mirrors already applied by scpm (§5.9). `auth` is `{user, password, realm?}` after scpm's plain-HTTP and origin policy (§5.7, §5.9). A bearer token still goes out as basic auth for the user `token`, as in Phase 1 (decided), so the cache path coursier derives from the user (`https/token@host/…`, §5.2) stays the same for `cs`, the fetcher and the sidecar; `realm` carries a realm-scoped entry imported from coursier's credential sources, which is otherwise sent only after a challenge naming that realm, as today. The sidecar applies `groups` itself (below), so Rust doesn't send a pre-filtered list. Repositories that share a URL become one coursier repository; `repository` in a result is then the first of them allowed for the module, and scpm's existing hint rule applies (§5.4).
- **Several systems in one call** (part of M1, not a cut). `systems` and `jdk` are per request. The sidecar resolves the request once per system, concurrently, with that system's OS and arch (below), and returns one merged `classpath`.
  - Entries on every system carry no `systems`, and the others list theirs, sorted: §5.4's lock form, so Rust writes it as it comes.
  - The sidecar does §5.4's canonical merge itself, identifying entries by `(group, artifact, version, classifier, extension)` (the fields `artifact_key` uses). It returns `bySystem` lists for the whole classpath only when the merge finds a cycle.
  - `natives` (`[{module, versions?, natives: {<system>: <classifier>}}]`) are the native mappings that apply, built-in and configured, sent by Rust. The sidecar applies them to any node, direct or transitive, through `Fetch`'s extra-artifacts hook, from the repository that served the module.
  - A request with one system is the single-system case `cs` can also do.
- **`proxy`** (`{http?, https?, noProxy[]}`, already resolved and normalised by scpm, §5.9) is set once in `initialize`, because the JVM's `ProxySelector` is global. A kept-alive sidecar is restarted when the proxy settings change.
- **Cache policy.** `ttl` and `refresh` are explicit. The defaults are what coursier's `CacheEnv` yields with no environment: checksums `[SHA-1, none]`, a 24 h TTL for changing metadata, and the policies `LocalUpdateChanging` then `FetchMissing`. `refresh: true` re-checks changing metadata (`maven-metadata.xml`) regardless of the TTL; `scpm update` and `scpm outdated` use it.
- **sha256 is computed by Rust**, from `file`, as today: Rust already hashes what coursier downloaded (§5.2), and the spike shows that hashing in a C1-only JVM is slow (19 MB of jars: about +130 ms with C1 against +15 ms with C2). `size` lets Rust check it read the file coursier wrote.
- **`listVersions`** is implemented with coursier's `Versions` over the routed repositories, never with `Complete`, so `completeOpt` can't bypass routing. A 404 or a missing listing counts as no versions from that repository. **Any 401, 403 or transport failure on an allowed repository fails the request** (`unauthorized`/`transport`), so a missing token can't silently select a lower version. Release ages (§5.6) are per version, and `maven-metadata.xml`'s `<lastUpdated>` is per artifact, so `minimumReleaseAge` is checked by the Rust fetcher with HEAD requests on the candidate versions, not by the sidecar. The compatibility test compares version **sets** with `cs complete-dep`.
- **Errors** carry `code`, `message` and `data`. The worker's codes (`bad_request`, `unknown_method`, `unsupported_protocol`, `internal`) plus:

  | Code | Meaning | `data` |
  |---|---|---|
  | `not_found` | No allowed repository has the module or version | `module`, `version`, `tried: [{repository, url, status}]` |
  | `no_artifact` | A root with a classifier or non-jar type resolved to no file (§5.4) | `root` |
  | `unauthorized` | 401/403 | `repository`, `url`, `status` |
  | `transport` | Network failure after coursier's retries | `repository`, `url`, `cause` |
  | `checksum` | A served checksum didn't match | `url`, `expected`, `actual` |
  | `offline_miss` | `offline` and not cached | `url` |
  | `bad_environment` | Start-up found a setting it must not inherit (below) | `what` |

  `tried` comes from a recording wrapper around the cache's `fetch`, which logs each URL and status per request. `bad_environment` is detected at start-up, before any request has an id: the sidecar answers the `initialize` request with it and exits, and if stdin closes first it exits with status 2 and the reason on stderr.

**No ambient configuration.** The sidecar reads nothing but its request. Coursier's library defaults read the environment and system properties (`CacheDefaults`, `CacheEnv`, `CoursierEnv` in 2.1.27): `COURSIER_CACHE`, `COURSIER_CREDENTIALS` with `credentials.properties`, `SCALA_CLI_CONFIG`, `COURSIER_CONFIG_DIR`, `COURSIER_TTL`, `COURSIER_MODE`, `COURSIER_CHECKSUMS`, `COURSIER_CACHE_SERVER*`, `COURSIER_REPOSITORIES`, `COURSIER_MIRRORS`, `COURSIER_EXTRA_MIRRORS`, `COURSIER_MAVEN_SETTINGS`, `CS_MAVEN_HOME`, `MAVEN_HOME`, `coursier.*` properties, `CI` (user agent), and `os.*`/`java.version` (profile activation). The `@data` case classes evaluate their default arguments **at construction**, before any `with…` call: `Resolve(cache)` evaluates `Resolve.defaultRepositories` and `defaultMirrors`, `Versions(cache)` evaluates `defaultMirrorConfFiles`, `Fetch(cache)` does both, and a malformed file then throws even though its result would be replaced. So the sidecar never relies on a default and never calls the convenience constructors:

- **Every object is built with each defaulted field given**: `FileCache(location, …)`, `Resolve(cache, repositories = routed, mirrors = Nil, mirrorConfFiles = Nil, resolutionParams = …, …)`, `Artifacts(cache, …)`, `new Fetch(resolve, artifacts, None)`, and `Versions(cache, Some(module), routed, mirrorConfFiles = Nil, mirrors = Nil)`. A unit test fails when a coursier upgrade adds a defaulted field the sidecar doesn't set.
- **`FileCache`**, one per request, gets: `credentials` (the request's, below), `cachePolicies`, `checksums` and `ttl` (above), retry and timeout settings, `userAgent` (§5.7), a pool sized by scpm (below), and `allowCacheSubstitution = false`, which turns off the `COURSIER_CACHE_SERVER` path. `CacheDefaults.credentials`, which reads the credential files and the Scala CLI config, is then never evaluated.
- **Credentials** become `DirectCredentials` on that per-request `FileCache` (host, user, password, `realm`, `httpsOnly` per the plain-HTTP policy), not `Authentication` on each repository: coursier matches them by origin on every request, including after redirects, which is what `cs` and the reqwest fetcher do today. Per-repository `Authentication` would need `passOnRedirect` and would behave differently from the fetcher on a redirect.
- **TLS trust: the OS trust store** (decided), as for the Rust fetcher (reqwest with rustls, which verifies against the OS's store **[verify for reqwest 0.13]**). The sidecar builds its own `SSLContext` and passes it to each `FileCache` (`sslSocketFactoryOpt`) instead of setting `javax.net.ssl.*` properties:
  - on macOS, the Keychain (`KeychainStore-ROOT` and `KeychainStore`);
  - on Windows, `Windows-ROOT`;
  - on Linux, the system PEM bundle, whose path Rust finds the same way its own TLS stack does and sends in `initialize`.

  The JDK's own `cacerts` is not used, so a corporate root CA installed in the OS works on both paths.
- **`ResolutionParams`** gets the request's `osInfoOpt` and `jdkVersionOpt0`, plus `useSystemOsInfo = false` and `useSystemJdkVersion = false`.
- **Proxy.** `coursier.proxy.SetupProxy` (what the `cs` CLI calls to read `settings.xml` proxies) is never called. Coursier opens connections with `URL.openConnection()`, which asks the JVM-wide `ProxySelector`, so the sidecar installs its own from `initialize`'s `proxy`, with scpm's `noProxy` rules (§5.9). That matches the fetcher better than translating to `http.nonProxyHosts`. `java.net.useSystemProxies` stays off.
- **The process is sealed too.** scpm starts the JVM with a cleared environment plus a short allowlist: the parent's `LANG`/`LC_*` (so non-ASCII cache paths decode as they do for scpm), and on Windows `SystemRoot`, `SystemDrive`, `windir`, `TEMP` and `TMP`. That removes the JVM option variables (`JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS`, `JDK_JAVA_OPTIONS`) and every `COURSIER_*`/`CS_*` variable in one step, without a list to maintain. The command line also sets `-Duser.home=<stable private directory>` (stable so the AOT cache stays valid), `-Dcoursier.config-dir=<private empty directory>` and `-Dscala-cli.config=<missing file>`, so a stray `~/.m2`, coursier config directory or Scala CLI config lookup finds nothing. On Windows, coursier finds its config directory through the Known Folders API (dirs-dev), which ignores `user.home`; there the `coursier.config-dir` property is the guard, and the Windows run of the hostile suite must cover it.
- **Start-up check.** The sidecar answers `bad_environment` when any `coursier.*` property other than `coursier.config-dir`, any `scala-cli.*` other than `scala-cli.config`, any `*.proxyHost`, or `java.net.useSystemProxies` is set.

**`groups` inside resolution.** Each configured repository is wrapped in a delegating `Repository`, `Routed(underlying, id, allows: Module => Boolean)`:

- For a module it may not serve, it answers not-found (`Left("org:name is not routed to acme")`) **without making a request**. It overrides `find0`, the deprecated `find`, and both `versions` overloads, all routed.
- It does **not** override `findFromVersionConstraint`: the inherited implementation calls the routed `versions` and `find0`.
- It forwards `versionsCheckHasModule`. `completeOpt` returns `None`, because nothing in the sidecar uses `Complete` and it must not bypass routing.
- `find0` returns the **underlying** repository as the `ArtifactSource`. `Resolution.artifacts` matches on `ArtifactSource.ModuleBased`, so returning the wrapper would make Gradle-module artifacts vanish. The spike already did this, and got the same 14 artifacts in the same order as `cs`.
- Because results name the underlying repository, the sidecar keeps a map from each underlying instance to its configured id, which is how a classpath entry gets its `repository`.
- Coursier 2.1.27 type-tests `MavenRepositoryLike` in three places only. When it applies mirrors, scpm applies them instead. When it parses repository strings, the sidecar doesn't use it. When it toggles Gradle module support, the sidecar sets that on the underlying repository before wrapping it. The compatibility test confirms nothing else depends on the concrete type.

`allows` is the §5.4 rule computed for that repository, which covers both directions: a grouped repository serves only the groups for which it has the longest covering prefix; a repository without `groups` serves only groups that no rule covers. Repositories go to coursier in declared order, not grouped ones first. The matcher (dot boundaries, longest prefix wins) then exists in both Rust (`allowed_repos`, used by the fetcher) and Scala, so both test suites run one shared fixture file of `(repositories, group) → allowed ids`.

The wrapper takes a predicate, not a list, so it can support both forms in §5.4's "Later" note. With an **exclusive** form (today's `groups`), a covered group is served only by its repositories. With a **non-exclusive** form (e.g. `alsoServes:`, like Gradle's plain `includeGroup`), the repository is added to a group's candidates without taking the group away from the others: `allows(m)` is true for a repository without `groups`, or one whose `alsoServes` covers the group, as long as no exclusive rule covers the group. Order stays the declared order.

What this changes in §5.4's resolution limit: a re-lock no longer asks any repository about a module it may not serve, in either direction. That covers version listings for ranges inside POMs (coursier takes the first allowed repository whose listing succeeds) and the `graph` behind `why`. An internal module missing from its own repository is a `not_found` naming that repository, not a copy found elsewhere and then refused. The refusal after the fact stays as a cheap second check, and as the only check for restores behind a shared mirror. What `groups` can't see stays as it is: a machine's catch-all mirror forwarding a request upstream.

**Resolution context.** Each system is turned into the system properties a JVM on it would report, and coursier's own `Activation.Os.fromProperties` derives the activation values from them. That covers the family (`unix`, `mac`, `windows`), the name and the arch:

| System | `os.name` | `os.arch` | `path.separator` |
|---|---|---|---|
| `linux-x86_64` | `Linux` | `amd64` | `:` |
| `linux-aarch64` | `Linux` | `aarch64` | `:` |
| `macos-aarch64` | `Mac OS X` | `aarch64` | `:` |
| `macos-x86_64` | `Mac OS X` | `x86_64` | `:` |
| `windows-x86_64` | `Windows 11` | `amd64` | `;` |
| `windows-aarch64` | `Windows 11` | `aarch64` | `;` |

`os.version` is left unset, so profiles that test a version never activate. The JDK goes in as `jdkVersionOpt0`. The lock then no longer depends on the resolving machine, which removes §5.4's Phase 1 limit; `resolvedOn` and its warning go away. Every variant the sidecar writes carries `fullContext: true`. A variant without it (written by `cs`, or converted from Phase 1) may hold the resolving machine's OS-profile choices, so the next non-frozen sidecar run re-locks just those variants (§5.4, "Systems").

This is what makes the **universal lock** (§5.4, "Systems") possible and cheap. Each classpath becomes one request with several systems, and each extra system costs about one warm resolve (≈10–25 ms for the spike's graph). Comparing the per-system results shows exactly which artifacts are system-specific; those are the only entries that carry `systems` in the lock.

**Lifecycle.**

| Front-end | Sidecar lifetime |
|---|---|
| `scpm install`, `add`, `update`, `why`, `outdated` | Started at the first resolve, used for the rest of the command, `shutdown` at the end. All of a command's requests go to one process concurrently (today: up to 8 `cs` processes) |
| `scpm bsp`, `scpm mcp` (from M2; M1 is one-shot only) | One sidecar per front-end and workspace, kept alive and stopped after 10 minutes idle (its own timer, plus end of stdin). A manifest edit then re-locks in about 20 ms instead of about 200 |
| The shared daemon (`scpmd`, §6.3) | None for now: it serves every workspace, so a sidecar it owned would hold several workspaces' credentials |
| `--frozen` / CI restore | Never started |

- **Concurrency.** Requests run in parallel. Each has its own `FileCache` for credentials, and all share one download pool and the on-disk cache. Coursier's in-process locking and its cache file locks coordinate with other scpm processes, sbt and Mill, as `cs` does today. Today a re-lock runs about 48 downloads at once (8 `cs` processes × coursier's 6), so the pool is sized for **total** concurrency, with a per-host limit, rather than coursier's per-process 6. Coursier also runs resolution work on `cache.ec`, so the pool can't simply be the download limit. A cold-cache, many-variant re-lock is among the measurements still to do. Timeouts are scpm's job (it kills the process). A `cancel` method can follow when BSP needs it.
- **Not a shared per-user daemon**, unlike `scpmd`. Resolution is rare, and warm it is cheap. A process shared across workspaces would also hold several workspaces' credentials; a per-workspace child holds only one workspace's, and only for the request.
- **Not inside `scpmd`** for now, for the same credentials reason, plus crash and heap isolation and §5.2's rule that resolving must not need a warm daemon.

**Tests** (beyond the golden locks):

- **Routing**, against a recording fixture server: zero disallowed requests, in both directions, per resolution path. Cases: an internal artifact with a public parent POM, a public BOM imported by an internal POM, a relocation across groups, a range in a third-party POM, `latest.release`, `listVersions` and `graph`.
- **Concurrency**: two requests with different system sets running in parallel in one sidecar must give the same results as when run alone.
- **Universal lock**:
  - each system's projection equals a single-system resolve for that system;
  - the canonical merge gives the same bytes whatever order the per-system results arrive in;
  - a fixture with a cycle produces `bySystem` for the whole classpath;
  - built-in and overridden native mappings, on direct and transitive nodes;
  - a mapping-table change: M > N re-locks only the variants holding a module inside a touched range of a change or tombstone (including a range an entry was narrowed away from), and raises the header whenever the lock is written; M < N stops any re-lock; a frozen run only notes it;
  - a `cs`-written variant (no `fullContext`) next to sidecar-written ones: the sidecar re-locks only the unmarked one;
  - a jar-only fixture has no `systems` entry;
  - a host outside the set, with and without system-specific entries;
  - a frozen restore downloads only the host's files.
- **Hostile environment** (§5.9), on Linux, macOS and **Windows** CI.
- **Compatibility fixtures** served from a local fixture repository, so results don't drift with Central.

**Migration.**

| Step | What | `cs` |
|---|---|---|
| M0 (done) | The spike below | Default |
| M1 | The minimum: `worker/resolver`, the Rust client and the bootstrap list; `resolve` (with multi-system resolution) and `listVersions`; lockfile version 2 from both resolvers; `SCPM_RESOLVER=sidecar` opts in, and a project may then set several `lock.systems` | Default, single system only; also `why` |
| M2 | Sidecar is the default; the **standard system set becomes the default** for new locks and for locks whose header came from the old default, at their first non-frozen install (§5.4, "Systems") | Fallback for one release, for single-system workspaces only |
| M3 | The `cs` resolve path is deleted | Gone, unless something else still calls it |

- **M1 scope (decided): the minimum, except multi-system resolution.** M1 leaves out:
  - the kept-alive sidecar (BSP, MCP);
  - `alsoServes`;
  - release-age data from the sidecar;
  - `size` in results;
  - the AOT cache;
  - `graph`: `why` stays on `cs` in M1, so it explains only the host system's classpath. `why --system <s>` for any other system is an error in M1 that names the sidecar's `graph` method (M2); it never prints the host's graph as if it were another system's.

  All of these follow in M2. Multi-system resolution is in M1, so projects can opt into universal locks before M2 makes them the default.
- **M1 prerequisites.** The implementation lands after #14 (machine settings) and #5 (the compile worker launcher and `detect_java`) have merged, so its branch has both. The Rust CLI on `machine-config` doesn't start any JVM yet. In M1 the sidecar runs on the JDK the compile worker detects: no JDK download, and no `cs java-home`.
- **Compatibility test.** Every golden-lock fixture is resolved both ways on one warm cache and must give a **byte-identical `scpm-lock.yaml`**, identical `why` chains, and equal version sets. These fixtures are single-system, with the system equal to the host, and use no module with a native mapping, so `cs` can agree. `fullContext` and `nativeMappings`, which only the sidecar writes, are stripped before comparing. Multi-system fixtures are tested against the sidecar alone (Tests, above). The known differences each have a test of their own where only the sidecar is right: a non-host context, and a `groups` violation (`not_found` instead of a refusal).
- **Deleted from `coursier.rs`** at M3, once nothing else calls `cs`:
  - the sealing: `seal`, `unsealed_env`, `KEPT_COURSIER_ENV`, `JVM_OPTION_ENV`;
  - the launcher probe: `launcher_jvm_options`, `JvmOptions`;
  - the credentials file: `credential_properties`, `credentials_env_value`, `escape_property`;
  - `add_common_args*` and `CommonArgs`, `clean_stderr`, the `cs fetch` report parsing (`Report`, `report_parts`, `artifact_types`), and the tree parsing for `why` (`reverse_tree_chains`, `strip_ansi`);
  - the proxy-to-JVM-properties translation and its `http.nonProxyHosts` test.

  `foreign.rs` keeps its notices but no longer needs `unsealed_env`: the sidecar ignores every inherited variable. If anything still calls `cs`, its sealing stays until that caller is gone.
- **Stays:**
  - the Rust fetcher (`fetch.rs`) and the cache layout (`cache_path`, `escape`, `maven_path`);
  - `artifact_key`, the single key function;
  - `repositories()`, the plain-HTTP and credential policy, and the origin rule with its clash error (§5.9);
  - `machine.rs`, whose resolved settings now go into the request instead of `cs`'s environment;
  - `allowed_repos` for restores.

**Spike** (2026-11-26, *measured*). A throwaway coursier-API resolver (Scala 3.9, `coursier_3` 2.1.27, a `Routed`-style wrapper over Central, a Linux context) resolved `org.typelevel:cats-effect_3:3.5.7` + `org.scalameta:munit_3:1.3.6` (14 jars) with a warm cache. Each figure is the median of 7 runs after one discarded warm-up, wall clock from spawn to exit; kept-alive figures are timed inside the sidecar. JDK Temurin 25.0.5.1 on both. The Linux container is arm64 Docker on the same Mac with `--cpus=2`, so it reproduces a 2-vCPU runner's core count, not its x86 cores.

| Configuration (ms) | M2 Pro, 10 CPUs | Linux, 2 CPUs |
|---|---|---|
| `cs fetch`, native launcher | **31** | not measured |
| `cs fetch`, JVM (`coursier-cli_3` on JDK 25) | 717 | 712 |
| Sidecar cold, plain | 443 | 439 |
| Sidecar cold, C1 only | 368 | 336 |
| Sidecar cold, AOT cache | 222 | 264 |
| Sidecar cold, AOT + C1 | 194 | **168** |
| Sidecar cold, AOT + C1 + SerialGC | 191 | 164 |
| Kept alive, 2nd request (tiered / C1) | 26 / 20 | 23 / 18 |
| Kept alive, 10th request (tiered / C1) | 12 / 8 | 12 / 8 |

Read: AOT + C1 cuts a cold sidecar by 56% (Mac) to 62% (Linux), matching the worker spike. That leaves it 163 ms behind native `cs` on the Mac, and about 4× faster than `cs`'s own JVM launcher. Of the AOT + C1 total, the first request takes about 100 ms and JVM start-up the rest. A kept-alive sidecar beats native `cs` from the second request. The sidecar resolved the same artifacts as `cs`, in the same order.

**Risks.**

1. **Coursier API drift.** The design relies on `@data` fields (`allowCacheSubstitution`, `useSystemOsInfo`, `jdkVersionOpt0`) and constructor defaults. The defaulted-field test, the cleared environment, the private directories and the hostile-environment suite catch a new default being read.
2. **Duplicate matchers** (`groups` routing, `noProxy`) in Rust and Scala, kept in step by shared fixtures.
3. **A Java is needed for `scpm add`** in a project that has never compiled: JDK 17 or newer, found by `detect_java` as for compiling. Its absence, or an older JDK, fails with a clear message.
4. **The bootstrap list needs GitHub reachable**: `github.com` and the host it redirects asset downloads to (`release-assets.githubusercontent.com`, formerly `objects.githubusercontent.com`), through the machine's proxy. A network that blocks them can set `resolverMirror` (still sha256-pinned), or wait for the Central move. `SCPM_RESOLVER_CLASSPATH` also works, but without the pins.
5. **Universal-lock costs.**
   - A native-heavy project's lock grows by one entry per system for each native module.
   - A re-lock does one resolve per system, and downloads every system's files to hash them: JavaFX or LWJGL natives five times over.
   - The built-in mapping table needs maintaining as libraries rename classifiers.
6. **Unmeasured:** large graphs (heap, C1 against C2, AOT training on a spark-sql-sized resolve), a cold-cache many-variant re-lock against today's 48 parallel downloads, native `cs` on the 2-CPU container, and AOT cache portability across machines with the same JDK build (also open in §6.3).

**Decisions (user, 2026-11-27).**

1. **Bootstrap:** the resolver's own jars are restored from a list built into scpm, not from the lock.
2. **Distribution:** GitHub Releases assets on `gatorcse/scpm` for now, not Maven Central. GitHub Packages Maven needs auth even for public packages, so Releases assets are used instead. Moving to Central later changes only the built-in list's URLs.
3. **Bearer tokens:** keep sending them as basic auth for the user `token`.
4. **TLS trust:** the OS trust store.
5. **Cold cost:** about 160 ms more than native `cs` per re-lock is acceptable; native-image waits.
6. **M1 scope:** the minimum, except that multi-system resolution is not cut (Migration, above).
7. **Naming:** an OS and arch pair is a *system* (`lock.systems`, `--system`, `--all-systems`, `natives:`, `lock.nativeClassifiers`); *platform* stays `jvm`/`js`/`native` (§5.4, "Systems").

**As implemented (M1).** `worker/resolver`, `crates/scpm/src/resolver.rs` (the client and the bootstrap) and `natives.rs` (the mapping table); opt in with `SCPM_RESOLVER=sidecar`. The protocol is documented in [worker-protocol.md](worker-protocol.md). Details settled, or changed, in the implementation:

- **`initialize` also takes `userAgent`**, so the sidecar sends the same `User-Agent` as `cs` and the fetcher. **`ttl` is in seconds.** The client sends no `ttl`, `refresh` or `offline` yet (the defaults apply): `refresh: true` (which re-checks changing metadata with a zero TTL) waits for `scpm update` and `scpm outdated`, which don't exist yet. Realm-scoped entries from coursier's credential sources are all sent (`challenges`), and the realm a challenge names picks one.
- **TLS trust** comes from the OS store, with the JDK's `cacerts` added where that store can't be read whole, and a note on stderr when that is a fallback:
  - macOS: `KeychainStore-ROOT` (the system roots) and `KeychainStore` (the user's keychain). `KeychainStore-ROOT` exists from JDK 23; on JDK 17–22 the user keychain holds no public roots, so `cacerts` stands in for the system roots (with a note).
  - Windows: `Windows-ROOT` plus `cacerts`. Windows adds a root to its store only when its own chain building first needs it, and the JDK's `Windows-ROOT` reads the store without triggering that, so a public root may not be there yet; corporate roots installed in Windows still count.
  - Linux: the PEM bundle scpm sends; without one, `cacerts` (with a note).
- **`repository` in a result** is derived in the sidecar from the artifact's URL (the longest matching configured URL, then the first allowed for the group), the same rule Rust uses for the lock's `repo` hint, rather than from a map of underlying repository instances. Rust keeps deriving `repo` itself, so the field is informational. `listVersions` names repositories sharing a URL the same way.
- **The download pool** is a fixed 48 threads in the sidecar, shared by every request's `FileCache`; there is no per-host limit yet (coursier's `HostThrottle` handles 429s, one for the whole sidecar). Requests, and the per-system resolutions of a request, run on two fixed pools sized to the CPUs (at least 4), so many requests wait their turn instead of each holding a thread.
- **`bad_environment`** answers the first request, whatever its method, then the sidecar exits with status 2. **`shutdown`** first lets requests still running finish and answer.
- **Errors:** `tried` lists only what repositories answered (a refresh of changing metadata after its TTL included, never the cache's own misses); a miss in an `offline` request is `offline_miss`, naming the POM it needed from the first allowed repository; an artifact's `not_found` names its module and version; a served checksum that doesn't match is `checksum` (in `listVersions` too; `expected` and `actual` are empty when coursier's error doesn't give them); a resolution failure that isn't a missing module is `internal`; among several missing modules, a refused one (401/403) is reported first. **`listVersions`** fails on a refused or broken `maven-metadata.xml` only; coursier's fallback after a 404 there, a directory listing, often isn't allowed (a Nexus without browse rights, S3 without ListBucket), so its failures count as no listing; a `maven-metadata.xml` that was served but gives no versions (unreadable) is `transport`, not an empty list.
- **Timeouts** (scpm's job): 60 s for the sidecar to start and answer `initialize`, 30 min for one `resolve` or `listVersions` (a whole cold re-lock), 10 s to exit after `shutdown`; past them scpm kills the process and the command fails saying which.
- **The JVM** is the detected JDK's own `bin/java` (the JDK whose `release` file scpm read), never a shim or wrapper on PATH, which may not work, or pick another JDK, in the sealed environment. Without a JDK home the sidecar doesn't start, and the error names `SCPM_JAVA_HOME`.
- **The bootstrap list** is `worker/resolver/bootstrap.txt`, written by `sbt resolver/bootstrapList` (`SCPM_RELEASE_TAG` names the release; without it the list says `release none`) and compiled in with `include_str!`. A line is `<sha256> <size> <file name>`, the resolver's own jar first. **Assets are named by content**, `<sha256>.jar`: URLs are `https://github.com/gatorcse/scpm/releases/download/<release>/<sha256>.jar`, or `resolverMirror` + `<sha256>.jar`, so one mirror directory serves every release. Where the jars come from, in order: `SCPM_RESOLVER_CLASSPATH` (with its notice, printed when the sidecar starts); a source checkout's locally built jars (`sbt resolver/stage` copies the closure to `worker/target/out/jvm/resolver-stage`, found above the running binary as the compile worker's jar is), for a `release none` list (and, in a debug build only, over a released one, with the same notice; a release binary never runs unpinned jars without `SCPM_RESOLVER_CLASSPATH`); the built-in list. A `release none` list without `resolverMirror` is an error that says how to build or point at the jars. Tests can replace the list (`SCPM_TEST_RESOLVER_LIST`) in debug builds only. **The release workflow (not written yet) must:** build the jars, run `bootstrapList` with `SCPM_RELEASE_TAG`, upload every listed jar as the asset `<sha256>.jar` of that release, publish the release, and only then build and distribute the binaries that embed the list.
- **The built-in mapping table** is version 1. A module may have several rows, each a version range and the systems it maps there, following what Maven Central publishes: Netty epoll `linux-x86_64` from 4.1.0.Final, `linux-aarch64` (no underscore) for [4.1.50.Final, 4.1.55.Final), `linux-aarch_64` from 4.1.55.Final; kqueue `osx-x86_64` from 4.1.11.Final and `osx-aarch_64` from 4.1.68.Final; io_uring (`linux-x86_64`, `linux-aarch_64`) from 4.2.0.Final; LWJGL's `lwjgl`, `-glfw`, `-jemalloc`, `-openal`, `-opengl` and `-stb` from 3.3.0 (the first with every arm64 classifier). Each module has a change-log line; the M > N rule and tombstones are implemented and unit-tested with a synthetic table.
- **Missing native jars.** The sidecar requests native jars as optional, and coursier 2.1.27 skips an optional artifact answered 404 or 403 (measured in `ServiceSuite`; S3 answers 403 for a missing object when the caller lacks ListBucket); a 401, a transport failure or a bad checksum still fails the resolution. What `scpm install` then says, when it re-locks:
  - a system no row maps at the resolved version (a library that doesn't support it, or a version older than its natives): a **note**;
  - a system a built-in row maps, whose jar the repository answered 404 or 403 for: a **warning**, since the table follows Maven Central and this means a proxy's negative cache or a mirror without the classifier jars. The lock keeps no record of the omission beyond the jar's absence, so every later install (frozen ones too) reads it back from the lock and warns again when the host system is affected, telling how to re-lock once the jar is served;
  - the same under a mapping the user wrote (`natives:`, `lock.nativeClassifiers`), most likely a typo in a classifier: an **error**, and the lock isn't written;
  - a module whose mapping is turned off (`natives: {}`): nothing.
- **Not done in M1:** `scpm config explain` doesn't name the resolver's hosts yet; the hostile-environment test runs on the CI's Linux only (no Windows job exists yet).

**Still open.** Host the resolver inside `scpmd` later, as a separate class loader, once per-workspace credentials can be kept apart there?

### 5.3 Cache sharing

Use the user's existing coursier cache (`COURSIER_CACHE` or the platform default), so downloads are shared with sbt, Mill, scala-cli and bleep, and worktrees cost nothing extra.

In CI, the `setup-scpm` action caches it, keyed by the lockfile hash and the runner's host system (a frozen restore fetches only that system's files, §5.4).

### 5.4 Lockfile: `scpm-lock.yaml`

Design goals, taking lessons from pnpm 12's experimental v13 lockfile format: few merge conflicts, one entry per artifact, sorted, stable.

```yaml
lockfileVersion: 2               # Phase 1 wrote version 1, with a single `resolutionContext` instead of `systems` + `jdk`; scpm converts it
systems: [linux-aarch64, linux-x86_64, macos-aarch64, macos-x86_64, windows-x86_64]   # see "Systems" (also defaultSystems and nativeMappings)
jdk: 25
bootInputs: sha256:…             # repositories + systems + jdk; a change re-resolves every boot entry

boot:                            # everything needed before a JVM starts; lists reference `artifacts`
  jdk:                           # planned format (not written by Phase 1): project JDK per system, a JVM index id
    linux-x86_64:  { id: temurin:25.0.1 }
    macos-aarch64: { id: temurin:25.0.1 }
  scala-compiler@3.9.0: [org.scala-lang:scala3-compiler_3:3.9.0, org.scala-lang:scala3-library_3:3.9.0, …]
  compiler-bridge@3.9.0: [org.scala-lang:scala3-sbt-bridge:3.9.0]
  compiler-bridge@2.13.18+zinc-2.1.0: [org.scala-sbt:compiler-bridge_2.13:2.1.0]   # Scala 2 bridges come from Zinc
  worker: [dev.scpm:scpm-worker_3:0.4.2, …]
  test-runner: [dev.scpm:scpm-test-runner_3:0.4.2, org.scala-sbt:test-interface:1.0, …]
  tools:                         # planned format (not written by Phase 1)
    scalafmt@3.10.1: [org.scalameta:scalafmt-cli_2.13:3.10.1, …]
    scalafix@0.14.4: [ch.epfl.scala:scalafix-cli_2.13.18:0.14.4, …]

catalogs:                        # selections for catalog *ranges*, one per (platform, Scala binary version; `java` for Java coordinates)
  default/org.typelevel::cats-effect@jvm-3: { range: ^3.6.0, version: 3.6.3 }

variants:                        # keyed by variant id, even for single-variant modules
  core@jvm-3:
    specifiers:                  # each direct dependency as written, and the version scpm selected
      bom:com.fasterxml.jackson:jackson-bom: { spec: catalog, version: 2.20.1 }   # BOMs too
      org.typelevel::cats-core: { spec: catalog, version: 2.13.0 }
      io.circe::circe-core:     { spec: ^0.14.10, version: 0.14.14 }
      org.scalameta::munit:     { spec: catalog, version: 1.2.1 }
      util:                     { spec: 'workspace:*' }
    inputsHash: sha256:…
    compileClasspath:            # ordered
      - { module: util@jvm-3 }
      - org.typelevel:cats-core_3:2.13.0
      - io.circe:circe-core_3:0.14.14
      - …
    runtimeClasspath: [ … ]
    testCompileClasspath: [ … ]
    testRuntimeClasspath: [ … ]

artifacts:                       # group:artifact:version[:classifier][@type]; one entry per file
  org.typelevel:cats-core_3:2.13.0:
    repo: central                # repository id from config, never a mirror: a hint for restores
    sha256: …
    size: 6012345
  io.netty:netty-transport-native-epoll:4.1.115.Final:linux-x86_64: { … }
```

#### Variants

Each variant locks four ordered classpaths:

| List | Contents |
|---|---|
| `compileClasspath` | `dependencies` + `providedDependencies` |
| `runtimeClasspath` | `dependencies` + `runtimeDependencies` |
| `testCompileClasspath` | `compileClasspath` + `testDependencies` |
| `testRuntimeClasspath` | `runtimeClasspath` + `providedDependencies` + `testDependencies` |

- **Workspace modules** (`workspace:*`) appear as references (`{ module: util@jvm-3 }`), not artifacts: every upstream variant in the transitive closure, in dependency order, before the external artifacts, which follow coursier's order. A direct `workspace:*` reference lands on the lists its declaring scope feeds (table above), so one under `testDependencies` is on the test lists only. From there the closure follows each upstream module's `dependencies` on every list and its `runtimeDependencies` on the runtime lists, never its provided or test dependencies.
- **Resolution is joint over the variant's transitive workspace closure.** A variant's external dependencies are resolved in one coursier call together with those of every upstream variant, so if A depends on B, A's classpaths hold one version of each library for both A's and B's code.
- **Catalog ranges are resolved once per (platform, Scala binary version) for the whole workspace**, so every variant that uses that catalog entry on that platform and Scala binary version gets the same version. The selection lives in the lock's top-level `catalogs` section, keyed `<catalog>/<key>@<platform>-<scala binary>`. Re-locking a stale variant reuses an existing selection whose range is unchanged; only `scpm update` re-selects, and it then re-locks every variant that uses the entry.
- **`specifiers`** records each direct dependency's spec as written and the version scpm selected for it (§5.5), before coursier's reconciliation, like pnpm's importers. `workspace:*` entries have no version. BOMs (the workspace's and the module's) are recorded too, keyed `bom:<coordinate>` so they never collide with a dependency on the same coordinate. A joint resolution applies one version per BOM coordinate: the variant's own, else the nearest upstream variant's (§5.4). A Java-only module skips workspace BOMs written `org::name`; its own are errors.
- **`inputsHash`** is a sha256 over every effective input that affects the variant's resolution, after presets and `variants` (§4.4): platform and full Scala version, dependencies of every scope (catalog references replaced by their entries), `compilerPlugins`, BOMs, overrides, repositories (ids, URLs and `groups`), the system set, `lock.jdk`, the configured native overrides (`natives:`, `lock.nativeClassifiers`; built-in mappings are versioned in the header instead, "Systems", below) and `policy.minimumReleaseAge` / `minimumReleaseAgeExclude`, plus what joint resolution consumes from each upstream variant in its workspace closure: its compile and runtime dependencies, BOMs and Scala version. An upstream variant's test or provided dependencies and compiler plugins don't affect downstream variants, so editing them re-locks only that variant.
- A variant with `compilerPlugins` also has an ordered `compilerPlugins` list.

**Artifacts** are keyed `group:artifact:version[:classifier][@type]`, with `@type` omitted for `jar`. Each file has one entry, shared by every variant that uses it, holding `repo`, `sha256` and `size`. The lock records no dependency graph between artifacts: it would depend on the order variants happened to be locked in, so `scpm why` asks coursier instead (below). Declared classifiers and types (`classifier: sources`, `type: exe`, `aar`, `test-jar`, `bundle`, …) are locked like any other file, and a direct dependency that resolves to no file is an error naming it, except a plain POM-packaged aggregator (no classifier or non-jar type, such as `groovy-all`), which contributes only its transitive artifacts. **IDE sources jars are not in the lock.** They are fetched on demand and verified against the repository's checksum.

**Boot** holds the closures of the Scala compiler, compiler bridge, scpm worker, test runner, and tools (scalafmt, scalafix), each a list of references into `artifacts`. The resolver sidecar's own jars are not in the lock: scpm restores them from a list built into each release (§5.2.1). A CI run can then fetch everything it needs in parallel from Rust before any JVM starts. `boot.jdk` is the project JDK (§6.3), keyed by `os-arch` (`linux-x86_64`, `macos-aarch64`, …). In Phase 1 it records only the JVM index id, which `cs java-home --jvm <id>` turns into a cached JDK; the Rust JDK fetcher (Phase 3) adds a URL and sha256 per entry. Boot entries re-resolve when their inputs change: a Scala 2 compiler bridge's key carries the Zinc version (`compiler-bridge@2.13.18+zinc-2.1.0`), and the top-level `bootInputs` hash covers the repositories, the system set and the JDK, so changing any of them re-resolves every boot entry.

#### Systems: a universal lock

> The default from M2 of the resolver sidecar (§5.2.1). Until then a new lock covers one system (below).

**Terms.** A **system** is an OS and CPU architecture pair (`linux-x86_64`, `macos-aarch64`), following Nix's use of the word. **Platform** keeps its §4.5 meaning: `jvm`, `js` or `native`. The names `lock.systems`, `--system`, `--all-systems`, `natives:` and `lock.nativeClassifiers` were confirmed by the user.

**The lock covers every system the project supports, and each machine uses its own.**

- **The system set** is `lock.systems`. For a new lock the default is the standard set: `linux-aarch64`, `linux-x86_64`, `macos-aarch64`, `macos-x86_64` and `windows-x86_64`. A project can narrow it (`lock.systems: [linux-x86_64, macos-aarch64]`), or extend it with any other system scpm knows the JVM properties for (§5.2.1's table, e.g. `windows-aarch64`).
- **The JDK** is `lock.jdk`, by default the project JDK's major version.
- **Sorting.** The lock's header, every entry's system list and the default set are all sorted, so the same set always serializes the same way.
- **Every variant is resolved per system**, Scala.js and Scala Native ones too. Their artifacts depend on the platform rather than the OS, but an OS-activated POM profile anywhere in the graph still applies, and one resolution would hand some systems the wrong dependencies. In practice their per-system results are identical, so the merge writes plain strings and the lock looks as if they were resolved once; the cost is a few warm resolves on metadata already in memory. Native mappings apply only to JVM variants.

This is the *universal lock* other ecosystems settled on:
- npm, pnpm and Yarn platform packages with `os`/`cpu`: installs skip the other systems, and `supportedArchitectures` fetches them;
- Cargo's `[target.'cfg(...)'.dependencies]`;
- uv and Poetry locks, which record every system's wheel.

scpm needs no change from library authors: the system-specific jars and profiles already exist on Maven Central.

| What differs by system | How scpm handles it | Config |
|---|---|---|
| A dependency a POM switches on with an OS/arch profile (e.g. JavaFX: OS-activated profiles set `javafx.platform`, which picks the classifier jar) | Every classpath is resolved once per system, with that system's OS and arch (§5.2.1) | None |
| A native published as a classifier (Netty's `netty-transport-native-epoll`/`-kqueue`, LWJGL's `natives-*`) | A system → classifier mapping per module adds that system's jar | scpm's built-in mappings; a `natives:` entry overrides one |
| One jar with every native inside (sqlite-jdbc, JNA) | Nothing to do | None |

**Native mappings.** An override looks like this:

```yaml
dependencies:
  - dep: io.netty:netty-transport-native-epoll:4.1.115.Final
    natives:                         # system -> classifier; replaces scpm's built-in mapping
      linux-x86_64: linux-x86_64
      linux-aarch64: linux-aarch_64
      # other systems in the set: no native jar for this module
```

- **Built-in mappings.** scpm ships a mapping table keyed by module, with a version range where a library renamed its classifiers: for example Netty's epoll, kqueue and io_uring transports, and LWJGL's modules. A mapping applies wherever its module appears in a JVM variant's graph, direct or transitive. The sidecar then adds the mapped classifier jar at the resolved version, from the repository that served the module. Most projects never write a mapping.
- **Versioned table.** The table has a version number. Each entry keeps a short change log: for every table version that changed it, the **touched range**, the union of the entry's version range before and after that change. A deleted entry leaves a tombstone whose touched range is its last range. So the binary can list, for any (N, M], every version range whose mapping changed, including the parts a newer range dropped (an entry narrowed from 1–3 to 2–3 has touched range 1–3), without shipping old tables.
- **Mapped modules contribute a jar.** A mapping applies only to a module that has an entry on the variant's locked classpaths; the native jar is added next to that entry. A module that is only a POM in the graph (a parent, a BOM, a `type: pom` dependency) gets no native jar. Every library in the built-in table publishes its natives next to a regular jar, so this costs nothing, and it is what makes "affected" decidable from the lock alone.
- **Overrides.**
  - A `natives:` map on a direct dependency replaces the built-in mapping for that module.
  - `lock.nativeClassifiers` in the workspace does the same for a module that only arrives transitively, as `[{module: org:name, natives: {…}}]`.
  - `natives: {}` turns a mapping off.
- **Systems a mapping doesn't list** get no native jar for that module. `scpm install` notes this in its summary. Built-in mappings list only the systems a library supports, so for those it is a note, not a warning.
- **Boot closures** are resolved per system too, because profiles apply there as well. Mappings don't apply to them: none of scpm's boot tools uses classifier natives.

**What decides the system set and the mappings.** A lock must not go stale just because a newer scpm binary has a different default set or a newer mapping table. So both are recorded in the lock's header, and **built-in mappings are not part of `inputsHash`**:

- **The system set.**
  - When `lock.systems` is unset and the header carries `defaultSystems: <set version>`, **the header governs** (default; user may revisit).
  - The binary's default set applies only to a new lock, to `scpm install --reset-systems`, and to a header without `defaultSystems`: one written from an explicit `lock.systems` that has since been removed falls back to the current default.
- **`nativeMappings: <table version>`** in the header (N) is compared with the binary's table (M). The non-frozen rows apply to sidecar runs only: `cs` skips the table comparison, re-locks no variant because of mappings, and keeps the header.

  | Run | N vs M | What happens |
  |---|---|---|
  | Frozen | any difference | Never stale because of mappings. The lock is used as it is, with a note: "lock uses mapping table N, this scpm has M" |
  | Non-frozen | M > N | Re-lock, in this same run, every **affected** variant: one whose locked classpaths have a module at a version inside a **touched range** of a change in (N, M] (entries and tombstones alike). A change to the Netty 4.2 entry doesn't re-lock 4.1 users; narrowing an entry from 1–3 to 2–3 does re-lock 1.x users, so they lose the old mapping. **Whenever the install writes the lock at all**, the header becomes M: every affected variant was re-locked in the same run, so all variants match M. An install that writes nothing leaves the header alone, so jar-only projects see no churn |
  | Non-frozen | M < N | Never downgrade. An install that would re-lock anything (a manifest edit, a variant without `fullContext`, …) stops with "lock uses mapping table N; upgrade scpm". One with nothing to re-lock only restores |

  So every variant marked `fullContext` matches the header's table; unmarked variants have no mappings, and the next sidecar run re-locks them. A mapping for a module that first arrives in a re-lock is applied by that re-lock. A lock without `nativeMappings` holds no sidecar-written variants (below), so the first sidecar re-lock sets it.
- **`inputsHash`** covers the system set and `lock.jdk`, and the configured overrides (`natives:`, `lock.nativeClassifiers`), which live in the manifest like any other dependency setting. Absent overrides hash as they did before the keys existed. The **whole** workspace-level `lock.nativeClassifiers` list is in every JVM variant's hash, not just the entries that apply to it, so any edit to it re-locks every JVM variant. Nothing in it depends on the lock or on the binary's table, so it can be computed before resolving.
- **`bootInputs`** covers the repositories, the system set and the JDK. No mappings apply to boot closures.

**Lock format.** An entry that is on every system stays a plain string. Only an entry that isn't carries `systems`:

```yaml
lockfileVersion: 2
systems: [linux-aarch64, linux-x86_64, macos-aarch64, macos-x86_64, windows-x86_64]
defaultSystems: 2                    # the header came from scpm's default set, version 2
nativeMappings: 7                    # the built-in mapping table this lock was written with
jdk: 25
variants:
  server@jvm-3:
    fullContext: true                # written only by the sidecar (§5.2.1)
    runtimeClasspath:
      - org.typelevel:cats-core_3:2.13.0
      - io.netty:netty-transport-native-epoll:4.1.115.Final
      - { artifact: io.netty:netty-transport-native-epoll:4.1.115.Final:linux-aarch_64, systems: [linux-aarch64] }
      - { artifact: io.netty:netty-transport-native-epoll:4.1.115.Final:linux-x86_64, systems: [linux-x86_64] }
      - …
artifacts:                           # every system's files, one entry each, all pinned by sha256
  io.netty:netty-transport-native-epoll:4.1.115.Final:linux-aarch_64: { repo: central, sha256: …, size: … }
  io.netty:netty-transport-native-epoll:4.1.115.Final:linux-x86_64: { repo: central, sha256: …, size: … }
```

- **Projection.** A system's classpath is the list filtered to the entries without `systems` or listing that system, in list order. It equals coursier's order for that system.
- **Canonical merge.** The sidecar merges the per-system orders deterministically, so every machine writes the same bytes:
  - Entries are identified by `(group, artifact, version, classifier, extension)`, the fields `artifact_key` uses.
  - The merge is a Kahn topological sort over the union of the per-system orders, with an edge from each entry to the next one in every system's list.
  - Among the entries ready at each step, the one earliest in the first system of the sorted header goes first (entries absent there come after those present), then the smallest key.
  - Each system's projection is then exactly its own order.
- **`bySystem`.** Only when that union has a cycle is the classpath written as `bySystem: {<system>: [...]}`, and it then replaces the whole classpath, not part of it. A cycle needs two systems to order shared entries differently, which can happen whenever a node that appears on only some systems has transitive dependencies shared with the others. Tests cover both forms.
- **Version differences.** A library that resolves to different versions on different systems (a profile pulls in a newer one) appears as two keys, each with its own `systems`.
- **No churn for jar-only projects.** A project with no system-specific dependency, which covers most JVM-only Scala projects, has no `systems` entry anywhere. Its classpaths read exactly as a single-system lock's, and only the header differs.
- Boot closures use the same form. `boot.jdk` already has one entry per system, and now has one for each system in the set.
- `artifacts` holds every system's files, so any machine can fetch any system's variant without re-resolving: for example, to package for Linux from a Mac.

**Using it.**

- **The host system is the project JDK's system** (default; user may revisit): the JDK of `toolchain.jdk` (§6.3), which runs `run` and `test`, the only tasks that load project natives. It is read from the `OS_NAME` and `OS_ARCH` lines of that JDK's `release` file, not from the OS and arch scpm's own binary was built for. An x86_64 JDK under Rosetta therefore gets x86_64 natives. Every consumer below (classpaths, the restore projection and the CI cache key) uses this one JDK's system, so a task never gets natives for a different JVM than the one it runs on. The compile worker's JDK (the one `detect_java` finds) may be a different JDK, but **it must be the same system**: macros and compiler plugins run inside the worker and can load natives from the compile classpath, and the worker's boot closures are restored for the host system too. A mismatch (say an aarch64 worker JDK next to an x86_64 project JDK) is an error naming both JDKs, their systems and `SCPM_JAVA`, raised before anything is compiled or restored. So one system governs the project's classpaths, the boot closures and the macros alike. If the project JDK isn't available yet, scpm falls back to the OS and arch it runs on, with a note.
- **Classpaths.** `compile`, `run`, `test`, `classpath`, BSP and `mbt.json` use the host system's projection. `--system <s>` selects another system for `classpath`, `run` and packaging.
- **A host outside the set** (default; user may revisit) is an error only when the lock has system-specific entries. The error names the host and suggests adding it to `lock.systems`; scpm doesn't guess a nearby system. When the lock has no system-specific entries, scpm uses the common projection and prints a note. `install --frozen` makes this check before downloading anything.
- **Restores.** A frozen restore downloads only the host system's files: the projection of every variant and boot entry. CI therefore downloads what it runs and no more. `scpm install --system <s>[,…]` or `--all-systems` also fetches others. The `setup-scpm` cache key includes the host system, computed by the same rule (the project JDK's `release` file, else scpm's own OS and arch), so the project JDK must be set up before that step.
- **Packaging** (Phase 4) uses the target system's projection.
- **`why`** explains the host system's classpath, or the one `--system` names (from M2; in M1 only the host's, §5.2.1).
- **Cost.**
  - A re-lock resolves every classpath once per system, concurrently, in one sidecar call (§5.2.1). Each extra system costs about one warm resolve: ≈10–25 ms for the spike's graph, still to be measured on a large workspace.
  - A re-lock also **downloads every system's files**, because Rust hashes them for the lock. JavaFX or LWJGL natives are then fetched five times over (once per system), on a re-lock only. Restores still fetch only the host's.

**This needs the resolver sidecar.** `cs` takes only `--jdk-version`. OS and arch profile activation follow the machine running it, so it can't resolve for another system. The native mappings also need per-node control over artifacts that its flags don't offer.

- **Both resolvers write lockfile version 2 from M1.**
  - `cs` writes only a single-system set. With `cs`, a set of more than one system is an error that names the sidecar.
  - **`cs` applies no native mappings and writes neither `fullContext` nor `nativeMappings`.** It doesn't strip them either: a `cs` re-lock keeps the existing `nativeMappings` header, and the markers on the variants (and the boot section) it didn't re-lock.
- **`fullContext: true` is per variant**, and written only by the sidecar, which applies the declared OS and arch. The boot section carries one such flag for all its entries, because `bootInputs` re-resolves them together.
  - A `cs` re-lock writes its variants without the marker, because under `cs` OS and arch profile activation follow the host (the Phase 1 limit below).
  - The next non-frozen sidecar run re-locks **only the variants without the marker** and sets it on them. That covers a single-system workspace whose profiles followed the host under `cs` and follow the declared system under the sidecar. It is also how native mappings first come into a lock that `cs` wrote.
  - The **boot section** follows the same rule: a non-frozen sidecar run re-resolves an unmarked boot section, whatever `bootInputs` says, and marks it. A `cs`-written boot closure may hold the host's profile choices, and `bootInputs` alone would never notice.
  - A frozen run uses unmarked variants and an unmarked boot section as they are, with a note.
- **Converting a v1 lock** (Phase 1: one `resolutionContext: {os, arch, jdk}`) gives `systems: [<os-arch>]` and `jdk`, plus `defaultSystems: 1` when the context was Phase 1's default. Conversion **keeps every `inputsHash`**: a single-system set is hashed exactly as v1's context was, absent overrides add nothing, and built-in mappings aren't in the hash, so converting alone re-locks nothing. Converted variants carry no `fullContext`, so the first sidecar run re-locks them. Phase 1's workspace key `lock.resolutionContext` is replaced by `lock.systems` and `lock.jdk` (scpm is unreleased, so there is no alias).
- **Until M2 (§5.2.1) the default set for new locks is one system, `[linux-x86_64]`** (default set version 1). With `SCPM_RESOLVER=sidecar` (M1), a project can set more systems.
  - **Phase 1 limit:** `cs` applies only the JDK part of the context. The lock doesn't record the resolving machine, because that would make otherwise identical locks differ by who resolved them. `scpm install` reports it instead (`resolvedOn` in `--json`) and warns when it differs from the declared system, only when it actually re-locked something.
  - **Local-runtime trade-off:** with the single `linux-x86_64` system, a macOS developer's `run` and `test` get the artifacts a Linux profile selects, including OS-classifier natives. Natives that differ by OS have to be declared by hand, one classifier per OS, in `runtimeDependencies`, and every one of them lands on every classpath.
- **At M2 the default set becomes the standard set** (version 2).
  - A new lock gets it.
  - An existing lock whose header came from default set 1 (`defaultSystems: 1`, `lock.systems` unset) is upgraded by **the first non-frozen `scpm install`** with an M2 binary, which says so in its summary (default; user may revisit). That keeps the low-effort default the user asked for, shows up as a reviewed lock diff, and never happens in CI (frozen). A lock with an explicit `lock.systems` is left alone.
  - That same install suggests removing hand-declared native classifiers that a built-in mapping now covers.
  - Under `SCPM_RESOLVER=cs`, the M2 default-set upgrade, and any change to more than one system, is skipped with a note. `cs` then works only for a workspace whose set has a single system.
  - **Version skew:** an M1 binary on `cs` running a non-frozen install over a lock that already has several systems (written by an M2 binary, or by the sidecar in M1) stops with the "needs the sidecar" error. A frozen run with it works, because restores need no resolver.

**As implemented (M1, lockfile version 2).** Details the text above leaves open, settled in the implementation:

- The boot section's flag is a top-level `bootFullContext: true` next to `bootInputs` (the `boot` map holds only entries). A re-lock that resolves any boot entry without the full context clears it.
- **`cs` and several systems:** the "needs the sidecar" error comes only when `cs` would resolve something (a stale variant or a missing boot entry). An install over a several-system lock with nothing to re-lock only restores, as a frozen one does.
- **The frozen note about unmarked variants** is given only for a lock that already mixes them with sidecar-written ones (a variant or the boot section marked, or a `nativeMappings` header), or when the sidecar resolves. Under `cs` every variant is unmarked, and a note on every frozen CI run would say nothing new.
- **Notes** (a host outside the set, unmarked variants, the project-JDK fallback) are part of `scpm install`'s summary (`notes` in `--json`). Commands that install first (`compile`, `test`, …) don't print them; errors are always printed.
- **The project JDK** is, until scpm provisions `toolchain.jdk` (§6.3), the JDK that runs `run` and `test` today: the one `detect_java` finds. The worker-JDK check is in place (before restoring and compiling) but can't fail until the two are separate JDKs.
- `scpm install --system <s>[,…]`, `--all-systems` and `--reset-systems`, and `scpm classpath --system <s>` exist; `scpm why --system <s>` accepts only the host system (M1 scope, §5.2.1). `classpath --system <s>` restores only that system's files and works from a host outside the set (packaging for Linux on a Mac). `run --system` is deferred (it would run another system's natives on the host's JVM; packaging, Phase 4, is the real consumer).
- **Converting a v1 lock** maps Phase 1's free-form `os`/`arch` to the canonical system (`Linux`/`amd64` → `linux-x86_64`, which counts as default set 1). Only the canonical spellings (`linux`, `macos`, `windows`; `x86_64`, `aarch64`) hash as v1 did; a lock written with another spelling re-locks once.

#### Resolution context and repositories

- **Resolution context:** the system set and `lock.jdk` (above), never the machine's own. The sidecar passes them to coursier once per system (§5.2.1).
- **Mirrors** are applied at fetch time and never recorded. They are machine settings (§5.9).
- **Which repositories serve which artifacts comes from config rules, never from discovering where bytes came from.** An artifact's identity is its key and sha256; where the bytes come from doesn't change what is built. This is the pattern of npm/pnpm scopes (scope → registry), Cargo (a logical source, with source replacement), Gradle (`content { includeGroup }`) and Maven 3.9 (repository filtering by group prefix).
  - **`groups:`** on a repository (`- {id: acme, url: …, groups: [com.acme, org.acme.internal]}`) lists group-id prefixes, matched at a dot boundary: `com.acme` covers `com.acme` and `com.acme.util`, not `com.acmes`. An artifact whose group a repository's `groups` covers comes only from the repositories with the **longest** covering prefix (several, in declared order, when they list the same one): it is resolved, locked and restored from them and never requested from any other repository, so internal names don't reach Central. That holds for the repositories scpm is configured with: a machine's catch-all mirror (a Nexus group proxying Central, say) can still forward a request for an internal name upstream, which is that machine's choice and outside what `groups` can see. Artifacts no rule covers use the repositories **without** `groups`, in declared order; a repository with `groups` never serves them, and an artifact it does serve anyway is refused (below). If every repository has `groups` and none covers an artifact, that is an error naming the group. `central` is an ordinary repository here: the built-in name has no `groups`, a full `{id: central, url: …, groups: […]}` may, and scpm always passes `--no-default`, so coursier's own default repositories never take part. An empty `groups` list and anything but dot-separated names are config errors. `groups` are part of `inputsHash` and `bootInputs`, so changing them re-locks (a repository without `groups` hashes as it did before `groups` existed).
  - **Vendor repositories with mixed groups.** Some vendor repositories host forks of public groups next to their own, such as Confluent's `org.apache.kafka:kafka-clients:*-ccs` builds beside `io.confluent`. Because `groups` route both ways, no `groups` setup says "this group from Central *or* the vendor": `groups: [org.apache.kafka]` on the vendor would keep every Kafka artifact away from Central. Leave such a vendor repository **without** `groups`: it is then one of the repositories for uncovered (public) names, in declared order, while internal grouped names are never restored from it (with `cs`, a re-lock can still ask it about one; see the resolution limit below). A non-exclusive form (like Gradle's plain `includeGroup` next to `exclusiveContent`), which would add a repository for a group without taking the group away from the others, is a possible future addition (see "Later" below).
  - **Later** (tracked, not designed further):
    1. Rename `groups:` to a name that says it is exclusive. Candidates: `exclusiveGroups`, `owns`, `onlyGroups`. Until then the key stays `groups`.
    2. Add the non-exclusive form above, e.g. `alsoServes:`, for vendor repositories such as Confluent's (`-ccs` Kafka builds). The resolver sidecar's repository wrapper takes a routing predicate, so it can support both forms (§5.2.1, "`groups` inside resolution").
  - **Resolution limit (`cs` 2.1.27):** `cs` has no per-repository group filter. scpm hands it the repositories with `groups` first, and `cs` takes each module from the first repository that has it, so an internal artifact is found in its own repository before any other is asked. Version listing for a range (§5.5) is scpm's own and asks only the allowed repositories, in both directions. Putting the repositories with `groups` first means `cs` asks them about public names too (direction 2), until they answer 404; that can't be avoided with `cs`, but what they answer is refused, never locked. `scpm why` runs `cs` the same way and records nothing. IDE sources jars (fetched on demand, above) aren't fetched by this version yet; they will follow the same rule. A re-lock can still ask another repository about an internal name when its own repository lacks that version, or, for a range inside a POM, when its own repository has no listing for the module: coursier then takes the first other repository whose listing succeeds (it merges listings across repositories only for `latest.integration`). Restores never do. The resolver sidecar (Phase 3) applies the rule inside resolution: each repository is wrapped so it answers not-found, without a request, for modules it may not serve, in both directions and for coursier's own version listing too. That removes everything in this paragraph except the refusal, which stays as a second check (§5.2.1).
- **`repo`** in the lock is a **hint**: the repository a restore asks first, one request instead of N, and a privacy aid (public repositories aren't asked about internal names). It is derived on every re-lock from the artifact's URL, matched against the configured repository URLs (and their mirrors) by prefix, longest match first, among the repositories allowed for its group: an artifact already in the lock keeps its id if that is still one of them (repositories sharing a mirror can't be told apart by URL), else the first in declared order. `groups` route both ways, for every request about a dependency, so a file that came from a repository its group may not come from is **refused, not locked**: the re-lock fails before the lock is written. For a group a rule covers, its own repository lacked it and `cs` found a copy elsewhere, which may be dependency confusion; the error names the key, the repository it came from (and the user config mirror it came through, which is then what to fix) and the repositories its group is restricted to. For a group no rule covers, a repository with `groups` served it, typically because its URL is a combined one that also proxies Central; locking those bytes under Central's name would let a tampered public jar uploaded to the internal side pass as Central's. The error names the artifact and the grouped repository, and gives the two correct setups: point the grouped repository at the internal-only repository URL (`…/repository/acme-releases/`, not the combined group URL), and reach Central through the company server with a mirror of `central` (user config today; project-level `required` mirrors later, §5.9). A URL that matches no configured repository is an error. Nothing is probed and nothing fails because of a shared mirror.
  - **Restore** asks the recorded repository first, if the artifact's group may come from it; on a 404 (or 410), or when a source can't be asked at all (its credential variable is unset, or its scheme's proxy is unusable), it asks the other allowed repositories, in declared order, each with its own credentials, cache path and proxy route; any other answer, such as a 401 or 403, ends the search, and a source's problem is reported only if no source has the file; the sha256 decides that the file is the locked one. A recorded id that is no longer configured is simply not asked, instead of being an error. A copy already cached from the first of them is used as is (like any cache hit); one cached from another allowed repository is used once its sha256 matches the lock, and classpaths then name it.
  - **Trade-off:** after a wrong hint, a restore may ask a later repository about the name once. `groups:` is how internal names are kept from repositories that must not see them.
  - Changing repositories still re-locks every variant and boot entry (they are in `inputsHash` and `bootInputs`), which re-derives every hint.

#### Staleness and commands

A variant is **stale** when its recomputed `inputsHash` differs from the lock's.

- `scpm install` — re-lock stale variants, then fetch whatever is missing.
- `scpm install --frozen` — the default when `CI=true`; a stale variant is an error.
- `scpm install --verify` — also re-hash cached files against the lock and re-download any that don't match.
- A corrupt or merge-conflicted lock is re-locked from scratch (with a warning); with `--frozen` (and so in CI) it is an error suggesting to resolve the conflict, or to re-create the lock locally with a non-frozen `scpm install` and commit it.
- `scpm compile|test|run` — locally, a stale lock is installed first (aube-style install-before-run) and scpm prints what changed. With `CI=true` or `--frozen`, a stale lock is an error.
- `scpm ci` — `scpm install --frozen`, then `compile` and `test` over the whole workspace; from Phase 3, `scpm install --frozen` then `scpm pipeline`.
- `scpm update [selector]` — re-resolve within the declared ranges.
- `scpm outdated` — list newer versions.
- `scpm why <coord>` — explain why a coordinate is on the classpath. It asks coursier: it rebuilds the variant's widest request (the test runtime classpath) at the locked versions and runs `cs resolve --what-depends-on org:name` (the sidecar's `graph` request from Phase 3, §5.2.1), printing each chain from a direct dependency (and which variant declared it). If the artifact is on the classpath but coursier reports no chain, it says so.
- `scpm dedupe` — collapse duplicate versions.

### 5.5 Version ranges without breaking the ecosystem

The risk is that ranges in **published** POMs are bad for Maven consumers: they make builds non-reproducible, and Maven's handling of pre-releases and nearest-wins is surprising. So the rules are:

1. **Manifest syntax:**
   - exact: `1.2.3`, the default; `scpm add` writes exact versions unless `--save-range` is given
   - `^1.2.3`, `~1.2.3`, `>=1.2 <2`, `1.2.x`; bounds must be versions, and unions (`||`) are not supported
   - `catalog`/`catalog:<name>`, `workspace:*` (publishing always writes concrete versions, so there is no `workspace:^` or `workspace:~`)
2. **Bounds use Scala's early-semver rules**, or the dependency's `info.versionScheme` when it declares one:
   - `^1.2.3` → `>=1.2.3 <2.0.0`
   - `^0.14.10` → `>=0.14.10 <0.15.0`
   - `~1.2.3` → `>=1.2.3 <1.3.0`
3. **scpm selects the version itself and hands coursier an exact one.** For each range it:
   1. lists the available versions (coursier's version listing, or `maven-metadata.xml` per repository);
   2. drops pre-releases (and dynver `+<hash>` builds) unless the spec names one;
   3. honours qualifier flavours: if a bound carries a non-pre-release qualifier (`^33.0.0-android`), only versions with the same qualifier match; otherwise plain versions win over qualified ones (`42.2.29` over `42.2.29.jre7`);
   4. drops versions younger than `minimumReleaseAge` (§5.6);
   5. keeps those within the bounds and picks the highest.

   Ranges are **not** passed to coursier as Maven intervals: in Maven ordering `2.0.0-RC1 < 2.0.0`, so `[1.2.3,2.0.0)` would admit `2.0.0` pre-releases (MNG-3092). A catalog range is selected once per (platform, Scala binary version) for the whole workspace (§5.4); a Java coordinate (`org:name`) once per platform, shared by every Scala version.
4. **Only direct dependencies and BOMs get ranges.** Transitive versions follow coursier's normal reconciliation. Ranges *inside* third-party POMs are honoured, because coursier does, and then pinned by the lock. A transitive version that violates `minimumReleaseAge` is an error naming the dependency path; fix it with an `overrides` pin or an exclusion.
5. **Ranges are resolved at lock time and never re-resolved implicitly.** A spec whose text is unchanged keeps its locked version even when its variant re-locks for another reason; only new or edited specs are selected (and `scpm update`).
6. **Publishing rewrites every spec to a concrete version.** The default is the **locked** version, which is what was actually tested, so Maven consumers see an ordinary POM. Option: publish the range's lower bound instead.
7. **Conflict policy:** after resolution, check every eviction against the evicted library's `info.versionScheme` (early-semver, pvp, semver-spec, strict, always), as sbt does. Fail with a readable diff that suggests an override.

Prior art: Gradle (dynamic versions + `gradle.lockfile`) is the only mainstream JVM tool with both ranges and locking. Bazel's `rules_jvm_external` locks via coursier's JSON report.

### 5.6 Supply-chain policy (cheap wins borrowed from pnpm 11 / aube)

- **sha256 in the lock**, verified on every fetch. Central reliably serves `.sha1` and `.md5` but not always `.sha256`, so we compute sha256 at lock time.
- **`minimumReleaseAge`.** Refuse to lock a version published less than N ago: ranges skip young versions, and a young transitive version is an error (§5.5). Use the POM's `Last-Modified` header (one HEAD request, only for *new* versions at lock time), the Central search API timestamp, or Artifactory's `createdAt` (§5.7). Overrides for security fixes are below.
- **`trustPolicy: no-downgrade`** for PGP or Sigstore signatures (later).
- **Vulnerability audit.** `scpm audit` checks every locked artifact in one `POST https://api.osv.dev/v1/querybatch`. OSV covers Maven, including GHSA data. OSV's Maven package names are `groupId:artifactId`, so Scala artifacts are queried with the suffixed artifactId, e.g. `org.typelevel:cats-core_3`.

#### Overriding the release-age gate for CVE fixes

Prior art:
- **pnpm**: `minimumReleaseAgeExclude` takes names, globs or exact versions. `pnpm audit --fix` *writes* an exact-version exclusion for you when the gate would block a patched version. Resolution itself has no automatic bypass.
- **scala-steward**: `updates.cooldown.minimumAge` with `skipForVulnerabilityCheck = true` (the default). Updates that fix a known OSV vulnerability skip the cooldown.
- **aube**: no advisory bypass at all.

scpm follows pnpm: **the bypass is explicit (`scpm audit --fix`) and always written down.** Resolution never calls an advisory database by default. A network lookup inside `install` would make locking depend on OSV's availability and on when an advisory happened to be published, and the lock should depend only on its inputs and the repositories. The scala-steward behaviour (security updates skip the cooldown automatically) lives in the update bot (§5.8), which runs `audit --fix` itself.

```yaml
policy:
  minimumReleaseAge: 1d
  minimumReleaseAgeExclude:
    - com.acme:*                                   # our own artifacts: always exempt
    - coord: com.fasterxml.jackson.core:jackson-databind@2.20.2   # exact version only
      reason: GHSA-xxxx-xxxx-xxxx                   # written by `scpm audit --fix` / the update bot
      expires: 2026-12-24                          # removed by `scpm audit --prune` once the version ages in
  securityBypass: off                              # default; `advisory-fixed` = also bypass during install (opt-in)
```

How it works:

0. `scpm audit` sends every locked artifact to OSV in one `querybatch` request and only reports. When `install` rejects a version for being too young, the error suggests `scpm audit --fix` but doesn't look anything up itself.
1. `scpm audit --fix` (or the update bot, §5.8) finds the **smallest version that the advisory lists as fixed** for each vulnerable locked artifact.
2. If the age gate blocks that version, scpm adds an **exact-version** exemption with the advisory ID and an expiry, then updates the lock. The exemption never covers a package glob or a range, so a later malicious release isn't exempted too.
3. The exemption shows up in the PR diff, where a reviewer sees *why* a 3-hour-old jar is being pulled.
4. A one-off override for local experiments is `scpm add … --allow-young`. It is never valid with `--frozen`.

The trust anchor is the advisory database's "fixed in" data (curated GHSA/OSV), not the package author's claim. That's what makes the bypass acceptable, explicit or automated.

**Opt-in `securityBypass: advisory-fixed`.** For teams that want scala-steward's behaviour during ordinary installs: when the gate would block a version that is being selected, scpm queries OSV for that artifact. Results are cached briefly. If an advisory affecting the current version lists the blocked version as fixed, scpm allows it and writes the same exemption entry. Never with `--frozen`. If OSV is unreachable there is no bypass, just a warning, and the install never fails because of it.

- **Compile-time code execution.** Macros and compiler plugins run arbitrary code at compile time, so an `allowBuilds` analogue can't be enforced the way npm's lifecycle scripts can. At minimum, report which deps contribute compiler plugins or annotation processors.

### 5.7 Enterprise repositories (optional fast paths)

**Credentials.** A repository's `credentials` is either `env:VAR` (sent as a bearer token) or `{user: env:U, password: env:P}` (basic auth). Phase 1's Rust fetcher reads those, plus coursier's `COURSIER_CREDENTIALS` / `credentials.properties` for matching hosts. Coursier's credential sources only express basic auth, so when `cs` resolves a stale lock scpm passes a bearer token as a basic-auth password of the user `token` (which Artifactory and GitHub Packages accept) **[verify per repository type in Phase 1]**. Secrets never go on `cs`'s command line (a repository URL with `user@` is rejected): scpm writes them to a private (0600) temporary file that `COURSIER_CREDENTIALS` names for the `cs` process, scoped to the repository's scheme, host and port (coursier 2.1.27's `scheme://host:port` form). Coursier's own entries are read by scpm, matched the same way (a realm-scoped one only after a challenge naming its realm) and passed to `cs` through that file, so both paths apply one policy. Credentials in the Scala CLI config (`scala-cli config repositories.credentials`) are not used: coursier would read them alongside that file, so `cs` gets a `SCALA_CLI_CONFIG` that names no file. Where machine-local credentials, mirrors and proxies come from, and how `cs` is kept from reading other sources, is §5.9. From Phase 3 the resolver sidecar takes credentials in its request, so no credentials file is needed. A bearer token keeps going out as basic auth for the user `token`, so cache paths stay the same on every path (§5.2.1).

**Plain HTTP.** Credentials only go over HTTPS by default. Configured `credentials` on an `http://` repository are a config error unless the repository sets `allowInsecureCredentials: true`; coursier entries that would go over plain HTTP (`https-only=false`, or an `http://` host) are dropped for every repository that hasn't opted in. With the opt-in, credentials go to exactly that repository's scheme, host and port, never to the rest of the host. The opt-in is per repository, but coursier scopes credentials by origin only, so it can't cover a peer: when another repository reached at the same `http://` origin doesn't opt in, no coursier or user config entry goes to that origin, and a repository there with its own credentials is an error for a re-lock (the fetcher sends them to that repository alone). A user config mirror may opt in too (`allowInsecureCredentials: true` on the mirror entry, §5.9): it covers exactly the mirror's own origin, for the repositories reached through that mirror, and nothing else.

The environment variables are read only when needed: a frozen restore and `classpath` with every artifact cached work without them. `why` and re-locking start `cs`, which passes the credentials it can read and needs them for any POM it has not cached (a Rust restore fetches jars, not POMs). There a missing or empty variable leaves that repository's credentials out and is reported only as a note if `cs` then fails, so a successful `why` or re-lock doesn't show the credentials work. A Rust restore that must download from the repository fails on it.

Correctness always comes from coursier. These only speed up `outdated`, version listing and checksums:

**Artifactory**

- **OneModel GraphQL** (`POST /onemodel/api/v1/graphql`, Bearer token) is the batch path. One request covers many packages using GraphQL aliases, one per package:
  ```graphql
  query {
    cats: storedPackages { searchPackageVersions(
      where: { hasPackageWith: { name: "cats-core_3", typeIn: ["maven"] } },
      first: 50, orderBy: { field: VERSION, direction: DESC }) {
        edges { node { version createdAt } } } }
    circe: storedPackages { searchPackageVersions(
      where: { hasPackageWith: { name: "circe-core_3", typeIn: ["maven"] } },
      first: 50, orderBy: { field: VERSION, direction: DESC }) {
        edges { node { version createdAt } } } }
  }
  ```
  - `createdAt` also feeds `minimumReleaseAge`.
  - Requires Artifactory **7.104+**. Self-managed instances must set `metadata.onemodel.enabled: true`.
  - Alias batching is standard GraphQL, but JFrog's docs don't show it **[verify]**.
  - **Caveat: it indexes *stored* packages** (local repos and the cached part of remote repos), most likely not everything upstream on Central **[verify]**. So it can't be the only source for `outdated`.
  - Plan: query OneModel first, and fall back to `maven-metadata.xml` through the remote repo for anything it doesn't know.
  - The older Metadata GraphQL (`/metadata/api/v1/query`) exposes `latestVersion` and `versionsCount` on older instances.
  - AQL works too, but is capped at 3 concurrent queries and a 6,000-character body.
- `GET /api/search/latestVersion?g=&a=&repos=` — latest version, one package per call.
- `GET /api/storage/{repo}/{path}` — sha256 without downloading the file.
- Preemptive auth by default, because "hide existence of unauthorized resources" returns 404 instead of 401.

**GitHub Packages**
- `maven.pkg.github.com/OWNER/REPO`.
- Auth is required even for public packages, and only classic PATs or `GITHUB_TOKEN` work. Coordinates must be lowercase.
- **GraphQL for packages is deprecated for Maven.** Use REST `GET /orgs/{org}/packages/maven/{name}/versions`, one package per call; there is no batch API.
- `scpm login github` could write coursier-format credentials from `gh auth token`.

**Maven Central**
- Search API for `outdated`.
- Send a tool-specific User-Agent (Central asks tools to identify themselves, and rate-limits with 429).

### 5.8 Keeping dependencies up to date (a scala-steward-style bot)

What exists today:

| Tool | Can it update an scpm workspace? |
|---|---|
| **pnpm** | Has no bot of its own: just `pnpm outdated` and `pnpm update -i`. pnpm users rely on Renovate or Dependabot, both of which understand pnpm catalogs. |
| **Dependabot** | **No.** It added sbt in May 2026 (version updates only, no security PRs), but it has no custom, regex or plugin ecosystem. A new ecosystem means a PR to `dependabot-core` that GitHub has to accept. The Dependency Submission API gives Dependabot *alerts* (not PRs) from our lockfile. |
| **Renovate** | **Mostly, today.** A regex or JSONata custom manager on `scpm.yaml` and the workspace catalog works with the `maven` datasource. JSONata can read the module's Scala version to build `cats-core_3` from `org.typelevel::cats-core`. The catch is the **lockfile**: regenerating it needs `postUpgradeTasks`, which require self-hosted Renovate or a paid Mend plan. The free hosted app can't run commands. |
| **scala-steward** | **Not without a code PR.** Each build tool is a `BuildToolAlg` in scala-steward that runs the tool to extract dependencies, and there is no plugin interface. It's a small PR given `scpm deps --json`, and it would bring scala-steward's real advantage: **Scalafix migrations on version bumps**. |

The manifest format already helps: every dependency is one `coord: version` line in YAML. Any bot can do a textual bump; the hard part is only the lockfile and the Scala suffix.

**Recommendation: layer it.**

1. **Phase 2: Renovate preset + lock fix-up action.**
   - Publish a shareable Renovate preset (`github>scpm-build/renovate-config`) with the JSONata manager.
   - Publish a small GitHub Action that runs on bot branches: `scpm install` (regenerates the lock), then `scpm audit`, then pushes a commit. This sidesteps `postUpgradeTasks`, so it works with the free hosted Renovate app.
2. **Phase 3: Dependency Submission** from the lockfile, for Dependabot alerts and the dependency graph.
3. **Phase 4: first-party `scpm upgrade` + `scpm-upgrade` GitHub Action.**
   - Built on `scpm outdated --json`, which already knows the lock, catalogs, ranges, `versionScheme`, release-age policy and advisories.
   - **Groups updates by catalog entry.** One catalog bump updates every module that uses it in one PR.
   - **Applies Scalafix migrations**, reusing scala-steward's public migration list.
   - **Labels PRs by semver risk**, using `versionScheme` and MiMa against the old version.
   - **Security fixes skip the cooldown:** the bot runs `scpm audit --fix`, so the exemption entry appears in the PR diff (§5.6).
   - **Opens PRs** with release notes and changelog links.
   - Because it runs inside the tool, it handles the lock, ranges and policy natively. No external bot can.
4. **Any time: upstream a scala-steward `BuildToolAlg` for scpm.** It's cheap and reaches teams who already run steward.

### 5.9 Machine-local settings

Some settings legitimately differ between a laptop and CI: a developer reaches Artifactory through the office mirror and a keychain token, while CI uses a secret in an environment variable. Other settings must never differ, because they change what a build resolves. scpm keeps the two apart, and it is the **only** program that reads the machine-local ones.

**Two kinds of setting.**

| Kind | Examples | Where it lives | Can it change the lock? |
|---|---|---|---|
| **Project**: *what* resolves | repositories, catalogs, BOMs, overrides, `lock.systems` and `lock.jdk`, native overrides (`natives:`, `lock.nativeClassifiers`), release-age policy | `scpm.yaml`, recorded in `scpm-lock.yaml` | Yes, and that is the point: it is reviewed and committed |
| **Machine**: *how the bytes are reached* | credentials, mirrors, proxies, the cache directory | the environment and the user config (below) | **Not a fresh lock**: changing one never makes the lock stale, a frozen restore downloads exactly what the lock pins (repository ids and sha256 values, §5.4), and mirror URLs are never recorded. A re-lock that goes through a mirror can still pick different versions (below) |

A machine setting therefore decides whether a download *succeeds*, not *what* a restore downloads: the lock's sha256 values settle that. A difference between a laptop and CI fails loudly (a 401 naming the repository) instead of silently building something else. One caveat for re-locking: a mirror is another server and may not hold exactly what the repository does. A mirror for one id that points at a Nexus group can serve artifacts the real repository lacks (the lock then names a repository that, reached directly, answers 404; a restore without the mirror then asks the other repositories allowed for the artifact, §5.4, and fails only if none has it), and stale mirror metadata can change which version a range or `latest` picks. The re-lock is reviewed like any other lock change. sbt draws this line less firmly: `~/.sbt/1.0/global.sbt` can add plugins and change settings, so the same commit builds differently per machine. scpm has no user-level file that can do that.

**The real risk is two downloaders, not two machines.** scpm downloads through two paths: `cs` when it re-locks (and for `why`), and the Rust fetcher when it restores (§5.2). Coursier reads a lot of ambient configuration by itself. Every source `cs` reads that scpm doesn't is a place where the two paths disagree on one machine: a re-lock succeeds because `cs` quietly found a mirror or a password, then a restore, or CI, fails, and nothing in the repository says why. Verified against `cs` 2.1.27:

| Source coursier reads | What it can set | Today |
|---|---|---|
| `COURSIER_REPOSITORIES`, Scala CLI `repositories.default` | default repositories | ignored: scpm passes `--no-default` and its own `--repository` list |
| `COURSIER_MIRRORS`, `COURSIER_EXTRA_MIRRORS`, `mirror.properties` in coursier's config directory (`COURSIER_CONFIG_DIR`) | mirrors | ignored: `cs` gets a `COURSIER_MIRRORS` naming no file (which also disables `mirror.properties`), and no `COURSIER_EXTRA_MIRRORS` (read even then) |
| Maven `settings.xml` (`~/.m2`, `$CS_MAVEN_HOME/settings.xml`, `$MAVEN_HOME/settings.xml`, `COURSIER_MAVEN_SETTINGS`) | mirrors, `<server>` credentials for mirrors, proxies | ignored: `cs` gets `COURSIER_MAVEN_SETTINGS=false`, which stops mirrors and servers but not proxies (coursier's proxy setup reads `settings.xml` on its own, from `$CS_MAVEN_HOME`, else `$MAVEN_HOME` or `~/.m2`), so `cs` also gets a `CS_MAVEN_HOME` naming an empty directory |
| Scala CLI config (`SCALA_CLI_CONFIG`) | credentials, mirrors, default repositories | ignored since M2: `cs` gets a `SCALA_CLI_CONFIG` that names no file |
| `COURSIER_CREDENTIALS`, `credentials.properties` | credentials | read by scpm and passed on, one policy for both paths (§5.7) |
| JVM properties (`https.proxyHost`, …) | proxies | replaced: `cs` ignores `HTTPS_PROXY`, so scpm passes its resolved proxy as JVM properties (below), and the Rust fetcher gets the same proxy explicitly with reqwest's environment lookup off |
| `COURSIER_CACHE_SERVER` (with `_USER`, `_PASSWORD`) | a server every download goes through, with its own mirrors, credentials and proxy | ignored: removed from `cs`'s environment |
| `COURSIER_TTL`, `COURSIER_MODE`, `COURSIER_CHECKSUMS` | cache policy | ignored: removed from `cs`'s environment |
| any other `COURSIER_*` or `CS_*` variable | whatever a coursier version reads | ignored: `cs` keeps only `COURSIER_PROGRESS` and `COURSIER_NO_TERM` (output only) and the variables scpm sets |
| `JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS`, `JDK_JAVA_OPTIONS`, `JAVA_OPTS` (read by a JVM launcher, not the native one) | proxies and other JVM properties | ignored: removed from `cs`'s environment; for the JVM launcher `JAVA_TOOL_OPTIONS` is set again, to exactly scpm's resolved proxy |

A mirror from `settings.xml` with `<mirrorOf>*</mirrorOf>`, common in companies, used to break re-locking: `cs` downloads from the mirror, the URL matches no configured repository, and the lock refuses it (§5.4). The error was loud but baffling.

**Rule: `cs` runs sealed.** scpm resolves every machine setting itself, then hands `cs` exactly that and nothing else:

- `--no-default` and one `--repository` per configured repository, with mirrors already applied (so `cs` asks the mirror directly);
- `COURSIER_CREDENTIALS` naming scpm's private credentials file (§5.7);
- `SCALA_CLI_CONFIG` and `COURSIER_MIRRORS` naming files that don't exist, `CS_MAVEN_HOME` naming an empty directory, `COURSIER_MAVEN_SETTINGS=false`, and every other inherited `COURSIER_*` and `CS_*` variable (`COURSIER_CACHE_SERVER`, `COURSIER_EXTRA_MIRRORS`, `COURSIER_REPOSITORIES`, `COURSIER_TTL`, …; all but `COURSIER_PROGRESS` and `COURSIER_NO_TERM`) and the JVM option variables removed: an allowlist, so a variable scpm doesn't know about can't leak in;
- the resolved proxy as JVM properties (`https.proxyHost`, `http.nonProxyHosts`, …), in the one form the launcher takes. Coursier ships two kinds of `cs`, and neither takes the other's: the native launcher sets `-J-Dname=value` arguments itself and reads no Java option variable, while the JVM launcher (a jpackage image, e.g. `cs-x86_64-pc-linux-jvm`, which is what mise installs on Linux) hands every argument to coursier, which rejects `-J…`, but its JVM reads `JAVA_TOOL_OPTIONS`. scpm asks once per run, only when a proxy applies: if `cs version -J-Dscpm.probe=true` succeeds, the properties go in as `-J-D…` arguments (coursier's `sh` launcher passes those to `java` too); otherwise `JAVA_TOOL_OPTIONS` is set to exactly them, and the JVM's `Picked up JAVA_TOOL_OPTIONS` line is dropped from `cs` errors.

The Rust fetcher gets the same resolved values: the same mirror URL, the same credentials and an explicit proxy, instead of reqwest's own environment lookup. Both paths see identical inputs by construction. The compatibility suite (§5.2) runs a re-lock and a restore under a hostile environment (every source above set to something wrong) and checks they still agree. From Phase 3 the resolver sidecar replaces this sealing (§5.2.1): scpm starts it with a cleared environment (a short allowlist), private `user.home`, coursier config directory and Scala CLI config, and passes every setting to the coursier API explicitly, so nothing in the table above reaches it.

**Where machine settings come from**, highest precedence first:

1. **The project declares the need; the machine supplies the secret.** A repository's `credentials: env:ACME_TOKEN` in `scpm.yaml` (§5.7). The requirement is reviewed and committed, only the value is local, and a missing value fails with the variable's name: `repository acme needs ACME_TOKEN, which is not set`. This is the preferred form for anything CI must reach.
2. **The user config**, scpm's own file for machine settings: `<config dir>/scpm/config.yaml` (`~/.config/scpm` on Linux, `~/Library/Application Support/scpm` on macOS, `%APPDATA%\scpm` on Windows), or the file `SCPM_CONFIG` names. It holds only machine settings, never anything that makes the lock stale or changes what a frozen restore downloads:
   ```yaml
   credentials:                       # matched by scheme, host and port, like §5.7; a path prefix narrows it
     - url: https://acme.jfrog.io
       token: env:ACME_TOKEN          # bearer; or user/password, each `env:VAR`, `file:PATH` or a literal
     - url: https://nexus.example.com/repository/
       user: alice
       password: file:~/.secrets/nexus
   mirrors:                           # applied at fetch time, never recorded (§5.4)
     - url: https://nexus.example.com/repository/maven-public
       mirrorOf: [central]            # repository ids, URL prefixes, or "*"
       # allowInsecureCredentials: true   # only for an http:// mirror; covers exactly its origin (below)
   resolverMirror: https://artifacts.example.com/scpm-releases/   # optional; base URL for the resolver sidecar's own jars (§5.2.1), still sha256-pinned; anonymous access
   proxy:
     https: http://proxy.example.com:3128
     noProxy: [localhost, .example.com]
   ```
   Literal secrets are allowed, as in `credentials.properties`, but scpm warns when the file is readable by group or others. A credential for a mirror is matched against the mirror's URL, not the repository it replaces.

   Details settled in the implementation:
   - The file is parsed strictly (an unknown key is an error, like the workspace files); a missing file, even one `SCPM_CONFIG` names, means no settings (with a warning when `SCPM_CONFIG` names it). `scpm schema user-config` prints its JSON Schema.
   - Item 1's `credentials` live on the repository definitions in `scpm-workspace.yaml`.
   - When several credential entries match a URL, the longest path wins. A `file:` value is read without one trailing newline; `~` is the home directory and a relative path is relative to the user config's directory.
   - **One origin, one set of credentials.** `cs` scopes credentials by scheme, host and port only (default ports ignored), so a path can choose which credentials a repository gets but can't keep them from another URL on the same origin. A repository or mirror without credentials of its own therefore gets its origin's, on both paths: the Rust fetcher sends what `cs` would. Credentials that can't be read (an unset variable) count as none, and only matter once a download fails. When two repositories or mirrors on one origin have different readable credentials, scpm refuses to run `cs` (exit code 2) and names both URLs; two GitHub Packages organisations with different tokens hit this, one token for both doesn't. A restore isn't affected: the fetcher then sends each repository its own. The resolver sidecar (Phase 3, §5.2.1) keeps this rule: its credentials are coursier `DirectCredentials` matched by origin and re-matched after redirects, like `cs` and the fetcher, so the clash error stays for re-locks.
   - A mirror whose `mirrorOf` names the repository's id or a prefix of its URL wins over one for `*`; otherwise the first entry wins.
   - **A mirrored repository's workspace credentials are not sent to the mirror**, which is another host: they were declared for the repository. Its credentials are the user config's entry for the mirror's URL, else coursier's for the mirror's host.
   - **Plain HTTP:** user config credentials for an `http://` URL need the repository's `allowInsecureCredentials`, or the mirror's it is reached through, as coursier's entries do (§5.7). For a mirrored repository the opt-in covers the mirror's scheme, host and port, the origin actually contacted, and nothing else. One repository's opt-in can't cover a peer: coursier scopes credentials by origin, so every repository reached at a plain-HTTP origin that gets credentials must opt in itself. A user config mirror of `central` onto a plain-HTTP Nexus that also hosts an opted-in private repository therefore needs an opt-in for `central` too; otherwise a re-lock is refused, and the error says how. The natural one is the mirror's: `mirrors: [{url: 'http://nexus.local:8081/repository/group/', mirrorOf: [central], allowInsecureCredentials: true}]`. It covers exactly that mirror's scheme, host and port, for the repositories reached through it: credentials for that origin (from the user config, or coursier's own sources) may then go there, on both paths and for restore fallbacks. It covers no other origin, and doesn't opt in those repositories anywhere else. It belongs to the machine because the mirror does: the workspace never chose to send anything over plain HTTP, the machine that routes Central through an `http://` server did. Without it, credentials stay off plain HTTP as before. The no-peer-cover rule still holds for a repository reached at that origin directly, not through the mirror: it must opt in itself (`cs` would send it the origin's credentials). `central` written out in full with its own `allowInsecureCredentials: true` works too. `scpm config explain` shows where an opt-in comes from (`repository`, or `user config, mirror <url>`).
   - Repositories that share one mirror (`mirrorOf: ["*"]`) can't be told apart by URL; `cs` gets the mirror once. The lock's `repo` is only a hint (§5.4): an artifact already in the lock keeps its id if that is one of them, and a new one gets the first of them allowed for its group, in declared order. Nothing is probed and nothing fails (except an artifact refused because its group is restricted, §5.4). Hints can differ between machines behind such a mirror for an artifact first locked on each, a round of harmless churn until an existing id is kept. A restore without the mirror that misses at the hint asks the other allowed repositories, and the sha256 decides. So the mirror never decides which repositories an artifact may come from (`groups` do), only which one is asked first.
   - **Future: project-level mirrors (not in this PR).** `mirrors:` would also be allowed in the workspace config (and in shared configs, once they exist), with the same schema as the user config's, and could be marked `required`, so that a user config can add mirrors but not bypass that one, like Gradle's `FAIL_ON_PROJECT_REPOS`, a committed `.npmrc`, or Cargo's committed source replacement. Credentials stay machine-only. This is safe because a mirror can't change what is built: the lock pins sha256 values and never records mirrors.
   - **Proxies** are `http://host[:port]` (port 80 by default): `cs`'s JVM properties speak plain HTTP to the proxy, so `https://` and `socks5://` proxies are refused rather than handled differently by the two paths. A proxy URL with a user (an `@` anywhere after `://`) is refused too (proxy authentication is out of scope). No error about a proxy, credential or mirror URL repeats the value, which may hold a password. A `proxy` key in the user config replaces the environment entirely, even with no server in it. Of the environment variables, the upper-case form wins. A bad proxy in the user config is a configuration error; a bad one in the environment only fails requests of its own scheme that `noProxy` doesn't cover, so a `socks5://` `HTTP_PROXY` doesn't stop `https://` downloads. The fetcher checks each download it makes; `cs`'s requests can't be checked one by one, so a re-lock (or `why`) fails before `cs` starts when any repository it is given needs the unusable proxy, even if `cs` would have found everything in its cache.
   - `noProxy` entries cover a host and its subdomains (`*.domain` means `.domain`; `domain|*.domain` for `cs`), `*` covers everything, and loopback (`localhost`, `127.0.0.1`, `::1`) is always direct, as in the JVM's default. Address ranges (`10.0.0.0/8`), other wildcards and entries with a port (`nexus.corp:8081`, which neither path matches) can't be applied the same way by both paths, so both drop them and `scpm config explain` lists them. The fetcher and `explain` decide with one matcher; a test checks `cs`'s `http.nonProxyHosts` against it on real host names.
   - **Redirects** from HTTPS to plain HTTP are refused by both paths (coursier's default), so neither credentials nor artifacts travel unencrypted after a redirect.
3. **Compatibility imports**, read by scpm and put through the same policy: coursier's `COURSIER_CREDENTIALS` / `credentials.properties` (§5.7), and the standard `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` variables when the user config sets no proxy. People already have these for sbt and coursier, so they keep working, but scpm reports when it uses them.

**Ignored on purpose:** Maven `settings.xml`, coursier's mirror files and variables, the Scala CLI config, and JVM proxy properties. They belong to other tools, live in places nobody looks (coursier's macOS config directory is `~/Library/Application Support/Coursier`), and reading them live is how the two paths drifted. When one of them configures something for a repository or host the current command uses, scpm prints one notice naming the file and the setting, and how to move it (a planned `scpm config import maven` would convert `settings.xml` mirrors, servers and proxies into the user config, once and explicitly). There is no notice for sources that don't touch the repositories in use.

**Seeing it: `scpm config explain`.** Prints, per configured repository, the URL, the effective URL after mirrors, where its credentials come from (the variable or file name, never the value), and the proxy that applies, then every ignored foreign source it found that would have applied. From Phase 3 it also names the hosts the resolver sidecar's own jars come from (`github.com` and the host it redirects asset downloads to (`release-assets.githubusercontent.com`, formerly `objects.githubusercontent.com`), or `resolverMirror`) and the proxy that applies to them. `--json` gives the same for scripts and agents (§9.3).

```text
$ scpm config explain
central  https://repo1.maven.org/maven2/
         via mirror https://nexus.example.com/repository/maven-public/  (user config, mirrorOf central)
         credentials: user alice, password from file ~/.secrets/nexus  (user config)
         proxy: none (nexus.example.com matches noProxy)
acme     https://acme.jfrog.io/artifactory/maven/
         credentials: bearer token from env ACME_TOKEN (not set)  (scpm-workspace.yaml)
         proxy: http://proxy.example.com:3128  (user config)
config   ~/Library/Application Support/scpm/config.yaml
ignored  ~/.m2/settings.xml: mirror "corp" for * (https://corp.example) (would apply to central, acme)
```

As implemented: credentials name their source (`scpm-workspace.yaml`, `user config`, `COURSIER_CREDENTIALS` or `credentials.properties`) and say when a variable is not set or a file is missing; notes say when a user config credential is held back by the plain-HTTP policy or workspace credentials are not sent to a mirror. `scpm config [targets]` still prints variants; a module named `explain` is selected with `-F explain`. Foreign sources are found by a shallow read: `settings.xml` mirrors (Maven `mirrorOf` rules), servers (by repository or mirror id) and active proxies (assumed to apply to every repository; `nonProxyHosts` is not evaluated); `mirror.properties` (`COURSIER_MIRRORS`, else coursier's config directory) and `COURSIER_EXTRA_MIRRORS` by `from` prefix; the Scala CLI config's `repositories.credentials` (by `host`) and `repositories.mirrors` (by `from`), matching every repository when entries have another shape. Inherited `COURSIER_*` and `CS_*` variables `cs` doesn't get (other than those naming the files above, and those scpm reads itself) are listed by name only, never by value, as applying to every repository, and only when the command ran `cs`. JVM proxy properties can't be detected. Other tools' URLs are shown as `scheme://host[:port]` only, since they may hold credentials, and not at all when a numeric "port" could be the start of a password. The notice names the first applying setting and counts the rest, and is printed (on stderr, and as `notices` in `--json`) only when the command contacted a repository (a re-lock, a download, `why`) or failed while doing so; `scpm config import maven` is planned, not implemented.

**Not in scope:** proxy authentication, and per-repository proxies. Both wait for a user who needs them; `cs`'s JVM proxy-auth properties need verifying first **[verify]**.

---

## 6. Compilation: daemon or not?

### 6.1 Measurements

*Measured* on an M2 Pro, JDK 25, a synthetic 40-file module of ~3k lines; numbers are noisy, about ±15%:

| | Scala 3.9 | Scala 2.13.18 |
|---|---|---|
| Cold first compile (fresh JVM, in-process) | 12.9–15.7 s | 9.9 s |
| Warm steady state (10th+ iteration) | ~3.2 s | ~2.9 s |
| Cold ÷ warm | **4–5×** | **3.4×** |
| Hello-world process | 1.5 s | 1.5 s |
| + JDK 25 AOT cache (JEP 483/514/515): hello | 0.78 s | 0.65 s |
| + JDK 25 AOT cache: first compile | 7.6 s (−41%) | 5.9 s (−40%) |

There is no maintained native-image `scalac` (the only one we found was a 2018 Scala 2.12 experiment).

**Caveat on the AOT rows.** The JDK AOT cache covers only classes loaded by the built-in class loaders from a class path matching the training run. These numbers ran scalac directly on the application class path. Zinc loads scalac through a per-Scala-version `ScalaInstance` class loader, so the gain may not carry over; the Phase 0 spike must measure through Zinc.

### 6.2 Conclusion

- **A warm JVM matters for interactive dev.** A 3.4–5× difference in compile time is the difference between a fast edit-compile loop and a slow one. There is no way around JIT warmup today: the AOT cache halves *startup*, but warm code still needs profiling. JEP 544 (AOT code compilation, JDK 28 EA) and CRaC (Linux) may change this later.
- **A daemon does *not* matter for CI, as long as one job uses one JVM.** What kills CI is a JVM per module (sbt forks, Maven reactor plugins) or `sbt` boot plus project loading. A foreground worker that lives for the whole `scpm pipeline` invocation pays warmup once per job.
- **The largest CI win is not compiling at all**: remote cache plus affected-only selection, as bleep's 22 min → 16 s result shows. Then come the AOT cache, the warm coursier cache, and the cached JDK.

### 6.3 Design

**Two JDKs:**

| | Worker JDK | Project JDK |
|---|---|---|
| Chosen by | scpm: the JDK pinned per scpm release (25+), or the project JDK if that is newer; obtained with `cs java-home` in Phase 1 | `toolchain.jdk` in the workspace |
| Runs | `scpmd`: Zinc, scalafmt, scalafix, linkers | tests and `run` (forked JVMs) |
| Also | The AOT cache applies here | Java release target: scalac `-release` / `-java-output-version`, javac `--release`; a module's `release:` overrides the default (its major version) |

scpm picks a worker JDK at least as new as the project JDK, so any release target can be compiled.

**Locally:**
- One shared `scpmd` per `(scpm version, worker JDK)` per user. It serves every workspace and worktree.
- It listens on a unix socket in `~/.cache/scpm/daemons/`.
- It shuts down after 1 h idle, with a heap cap and ZGC (bleep's defaults).
- `scpm daemon ls|top|stop|logs` with OOM reporting, so stale daemons don't pile up.
- `--no-daemon` runs the worker in the foreground.

**In CI** (detected via `CI=true`):
- Foreground worker, never detached.
- Started **in parallel with** lockfile restore and remote-cache restore, so JVM boot overlaps network I/O.
- It is started **only if** at least one compile task misses the cache.

**AOT cache:**
- Generated on first run per `(worker JDK, scpm version, Scala version)` and stored in `~/.cache/scpm/aot/`.
- The `setup-scpm` action caches it.
- We could also pre-build AOT caches per release for the common JDK/Scala pairs **[needs validation that the AOT cache is portable across machines with the same JDK build]**.

**Zinc 2.1, embedded directly:**
- **Portable analysis**: a `MappedFileConverter` maps paths to `${WS}/`, `${CACHE}/`, `${JDK}/` markers, so analysis can be shared across machines, worktrees and the remote cache.
- Scala 3 pipelining (now fixed in 2.1) for more parallelism.

**Why not Bloop:**
- We need structured diagnostics, cancellation, our own scheduling and portable analysis.
- bleep and Mill both moved off it or never used it, and bleep documented why.
- Its last remaining reason to exist (Metals v1 integration) is covered by BSP plus `mbt.json`.

### 6.4 Can the daemon be Rust?

**Partly, and that is the recommended shape.** Scheduling, file watching, hashing and caching could live in a long-running Rust process (`scpm watch`, `scpm bsp`). The compiler itself must run in a JVM, because scalac and Zinc are JVM programs and no native compiler exists. So:

- a Rust orchestrator, optionally long-lived for watch and BSP;
- with JVM compile workers behind a narrow protocol.

A Rust daemon is not needed for speed: the Rust CLI starts in about 1 ms.

---

## 7. Worktrees

Inspired by aube's and pnpm's global virtual store. In the JVM world most of it is free, because classpaths point into the shared coursier cache and nothing is symlinked per project.

| Resource | Sharing across worktrees |
|---|---|
| Dependency jars | Coursier cache (already global). |
| JDKs, compiler, tools | Coursier / `cs java` cache. |
| Compile daemon | One per `(scpm, worker JDK)` per user; workspaces are keyed by absolute path. |
| Build outputs + Zinc analysis | **Per worktree** (`.scpm/out/`), but **seeded** on creation (below). |
| Task-result cache | Global local cache at `~/.cache/scpm/tasks`, keyed by content hash, so a hit in one worktree is a hit in all. |

`scpm worktree add <path> <branch>` does the following:

1. Runs `git worktree add`.
2. Finds the nearest worktree, or remote cache entry, whose outputs were built from the merge base.
3. Clones those outputs copy-on-write: APFS `clonefile` or Linux reflink, with a plain copy as fallback.
4. Rewrites nothing, because the Zinc analysis is portable.

The first compile in the new worktree is then an *incremental* compile of the diff. That's the JVM equivalent of aube's "install after `git pull` in 192 ms", and it matters most for agents that create many short-lived worktrees.

`scpm worktree prune` cleans up the local task cache entries no worktree references.

---

## 8. Tasks, pipelines, filtering

We copy pnpm 12's semantics almost verbatim, so the docs can say "same as pnpm".

**Tasks**
- A task is identified as `<module>#<task>`.
- In `dependsOn`, `^task` means that task in each upstream module; a bare `task` means the same module.
- Tasks are scheduled as soon as their dependencies finish, not by topological level.
- `concurrencyGroup` limits apply machine-wide across processes, e.g. to limit concurrent test JVMs.
- `--dry-run --json` prints the graph; `--resume-from <module>` reruns a failed run.

**Built-in tasks** are implemented natively rather than as shell commands, and need no `run:`. A custom task that wants a built-in with arguments writes `run: builtin:<name> <args>`:
- `compile`, `test`, `run`, `console`
- `fmt`, `fmt:check`, `lint` (scalafix)
- `package` (jar, assembly), `link` (Scala.js / Native)
- `mima`, `doc`, `publish`

**Custom tasks** are either:
- shell commands (`run:`), or
- **script modules**: an ordinary module whose `mainClass` is run with its classpath (`run: scpm run codegen -- …`). This is bleep-style scripting without compiling the build definition.

**Filtering** uses pnpm's selector syntax:
- `-F core...` — core and its dependencies
- `-F ...core` — core and its dependents
- `-F '[origin/main]'` — modules changed since the merge base
- `-F ./modules/**` — by path
- `!` excludes
- Variant-aware: `core@jvm-3`, `core@jvm-*`; a bare `core` selects every variant.

**`scpm pipeline`** (the CI entry point from Phase 3; `scpm ci` = `scpm install --frozen`, then `scpm pipeline`):
1. Install: auto-install locally, an error on a stale lock in CI (§5.4).
2. Affected modules plus dependents, since `pipelineBase`. A module also counts as changed when one of its catalog entries changes.
3. Restore from cache.
4. Run the graph, continuing past failures so every error is reported in one run.
5. Push to the cache.
6. Render reports.

**Caching**
- Opt-in via declared `outputs`; `outputs: []` makes tests and lints cacheable.
- The key covers:
  - inputs (git blob OIDs of tracked files, plus hashes of untracked non-ignored files)
  - the task definition
  - upstream task keys
  - the variant's lock entry
  - toolchain IDs
  - declared env vars
- On a hit we restore outputs and **replay the event log**, so annotations still appear on a cached run.
- Remote cache: speak an existing protocol, either the Turborepo Remote Cache HTTP API (what pnpm 12 uses) or the Bazel remote cache. Entries are signed and verified against `trustedKeys`. An unreachable cache never fails the build. GitHub Actions cache can also serve as a backend.

---

## 9. Reporting (the plugin API's centre of gravity)

### 9.1 Event stream

Every run writes `.scpm/runs/<run-id>.ndjson` and streams the same events to reporters. Sketch of the schema:

```jsonc
{"t":"run.start","run":"r_…","argv":[…],"ci":"github"}
{"t":"task.start","task":"core@jvm-3#compile","cached":false}
{"t":"diagnostic","task":"core@jvm-3#compile","severity":"error","file":"modules/core/src/Foo.scala",
 "range":{"start":{"line":41,"col":7},"end":{"line":41,"col":19}},"code":"E007","category":"type-mismatch",
 "message":"Found: Int\nRequired: String","rendered":"…ansi…","actions":[{"title":"Insert .toString","edit":…}]}
{"t":"test.case","task":"core@jvm-3#test","suite":"FooSuite","name":"parses","status":"failed","durationMs":12,
 "failure":{"message":"…","file":"…","line":88}}
{"t":"dependency.fact","module":"core@jvm-3","coord":"io.circe:circe-core_3:0.14.14","scope":"compile","direct":true}
{"t":"task.end","task":"core@jvm-3#compile","status":"failed","durationMs":2140}
{"t":"run.end","status":"failed","summary":{…}}
```

Diagnostics carry Scala 3 error codes and structured actions (Scala 3's `-rewrite` / actionable diagnostics), so agents can apply fixes mechanically.

### 9.2 Reporters

| Reporter | Notes |
|---|---|
| `tty` | Default for interactive use; pnpm-like compact progress. |
| `plain` | Default when not a TTY; terse; tuned for agents and token budgets. |
| `json` | `--json` on every command, stable schema, documented exit codes. |
| `github` | Automatic when `GITHUB_ACTIONS=true`; see below. |
| `junit` | Per test task. |
| `sarif` | Upload to code scanning, for persistent findings such as warnings and lint. |
| `mcp` | `scpm mcp`, see §9.3. |

**GitHub specifics:**
- Workflow-command annotations are capped (reported as 10 per type per step and 50 per job, **[verify]**) and truncate silently. scpm therefore:
  1. ranks diagnostics: errors first, then files changed in the PR, then warnings;
  2. emits up to the cap;
  3. writes the full table to `$GITHUB_STEP_SUMMARY`;
  4. states "N more" explicitly;
  5. can optionally create a **Check Run** via the Checks API, which allows 50 annotations per update and multiple updates.
- **Dependency Submission API**: upload a dependency snapshot from the lockfile, so GitHub's dependency graph and Dependabot alerts work for Scala. sbt only gets this through a separate action.

### 9.3 MCP server (`scpm mcp`)

Agents are a primary user, so the MCP server is a first-class front-end next to the CLI and BSP, not an afterthought. It is a thin layer: every MCP tool maps onto a CLI command's `--json` output, and the same event stream backs both, so the two can't drift apart.

**Design rules**

- **Token-frugal by default.** A green compile returns about one line (bleep reports 25 tokens, versus 31k for `mvn test`). Failures return structured diagnostics, capped and ranked like the GitHub reporter, with a `more` cursor to page through the rest.
- **Long operations stream progress.** They return a run ID that can be polled or cancelled, so an agent is never blocked waiting on a 3-minute test run.
- **Uses the shared daemon.** The MCP server is a Rust process. From Phase 2 it reuses the warm `scpmd`, so it starts nothing heavy and serves every worktree an agent opens. In Phase 1, before the daemon exists, `scpm mcp` owns a foreground worker for its lifetime.
- **Edits go through the same code paths as the CLI.** `add_dependency` edits the YAML (preserving comments) and updates the lock, exactly like `scpm add`.
- **Tools are scoped to the workspace root it was started in.** It never touches paths outside that root.

**Tool surface (first cut)**

| Tool | Returns |
|---|---|
| `compile(filter?)` | status, diagnostics (with Scala 3 codes and fix actions) |
| `test(filter?, testName?)` | status, failed cases with message and location |
| `diagnostics(file?)` | current diagnostics from the last run, without rebuilding |
| `graph(filter?)`, `why(coord)` | module graph, dependency path to a coordinate |
| `config(module, resolved?)` | the module's manifest, optionally fully expanded |
| `add_dependency`, `remove_dependency`, `update(selector)` | manifest and lock diff |
| `outdated(filter?)` | available updates, with advisory info |
| `classpath(module, scope)` | for agents that want to run tools themselves |
| `run(module, main?, args)` | output, capped |
| `explain(code)` | long-form explanation of an scpm or compiler error code |

**Resources:** `scpm://workspace` (module list), `scpm://module/<id>` (resolved config), `scpm://run/<id>` (event log of a run).

### 9.4 Plugins: out-of-process, data-first

Ordered by how much we expect people to need each kind:

1. **Shell tasks and script modules** cover most "plugin" needs: codegen, buildinfo, docker, assembly.
2. **Reporter plugins**: executables that read the NDJSON event stream from stdin. They are trivial to write in any language.
3. **Provider plugins**, launched over a JSON-RPC protocol. These are executables or coursier-launched JVM apps that the tool can *ask* for things:
   - source and resource generators (`generate(module) -> files`)
   - extra facts: licences, SBOM entries, vulnerability data
   - extra task kinds
   They're declared in the workspace, and the plugin artifacts are pinned in the lockfile's boot section.
4. **No in-process API.** Plugins never reach into the build model at runtime. This keeps startup fast and the config inspectable, and avoids the binary-compatibility treadmill sbt plugins live on.

---

## 10. IDE integration

### 10.1 Metals

- **Metals v2: write `.metals/mbt.json`** on `install` and whenever config changes. Metals v2's language server reads only that file, never the build files directly. It contains dependency modules (jar and sources), per-namespace sources, `dependsOn`, scalac and javac options, `javaHome`, and main and test classes. This puts us on Metals v2's fast path with no build-server round trips.

**Built (M8): `scpm ide metals`.** The format matches scalameta/metals branch `main-v2` at commit `aa5a3bac` (2026-11-25, after v2.0.0-RC1): `docs/build-tools/mbt.schema.json` plus the Gson classes Metals actually reads it into (`MbtBuild`, `MbtNamespace`, `MbtDependencyModule`, `MbtMainClass`), and the namespaces its built-in Maven importer (`MbtMojoImpl`) writes.

- **Namespaces.** Each variant is a namespace keyed by its id (`core@jvm-3`), and its tests a second namespace keyed `core@jvm-3:test`. Metals treats a namespace as a test target by that `:test` suffix, and its Maven importer writes the same pair. The test namespace `dependsOn` the main one.
- **Fields.**
  - `sources` are workspace-relative and `/`-separated.
  - `scalacOptions` and `javacOptions` are exactly what `scpm compile` passes: the variant's own options, the release target, and `-Xplugin:<jar>` for each compiler plugin.
  - `scalaVersion` picks the presentation compiler. Java-only variants have none.
  - `javaHome` is the JDK scpm runs (`SCPM_JAVA_HOME`, `JAVA_HOME`, then `java` on `PATH`).
  - `classDirectories` are scpm's own outputs, `.scpm/out/<id>/classes` and `test-classes`.
  - `mainClasses` comes from `mainClass`.
- **Classpaths come from the lock**, following the Maven importer's split. A main namespace gets the compile classpath (compile + provided). A test namespace gets the test compile classpath plus anything that is only on the test runtime classpath. Upstream workspace variants become `dependsOn` (transitively, as on the locked classpath). Jars are `file:` URIs into the coursier cache, and each dependency module is listed once and shared.
- **Sources jars are not in the lock**, as §5.4 says.
  - For each artifact, scpm uses `<artifact>-<version>-sources.jar` if it is already next to the jar in the cache.
  - Only the explicit `scpm ide …` commands download missing ones, in a single `cs fetch --sources --intransitive` call that verifies them against the repository's checksums.
  - Modules that publish no sources jar are listed in `.scpm/ide/no-sources` so they aren't requested again. The list is tagged with a fingerprint of the repositories (ids and URLs) and the user each one authenticates as, never a secret, and is dropped when that changes. `--refresh-sources` asks again regardless.
  - A failed fetch (offline, HTTP 429, …) is a warning, and nothing is recorded, so the next `scpm ide …` retries.
  - `--no-sources` uses only cached jars.
- **When it runs.**
  - `scpm ide metals [--output FILE] [--no-sources | --refresh-sources] [--frozen]` installs first if the lock is stale, then always writes the file (if its content changed).
  - Automatic refreshes run after a non-frozen `scpm install` and after the install-before-run step of `compile`, `test`, `run`, `why` and `classpath` (never with `--frozen` or in CI). Config edits that don't re-lock (`scalacOptions`, sources, `release`, `mainClass`) reach the file this way too.
  - A refresh regenerates the file only if scpm wrote its current content, which it checks against a sha256 kept in `.scpm/ide/mbt.json.sha256`, or if the workspace sets `ide: {metals: true}`. So when Metals or another importer replaces the file, scpm leaves it alone until `scpm ide metals` runs again.
  - When a Metals script importer (`*.mbt.sh`, `*.mbt.scala`, `*.mbt.java`, `*.mbt.bat`) is at the workspace root, Metals merges and writes the file itself, so refreshes skip it, with a warning if `ide.metals` is set.
  - Refreshes use cached sources jars only and write only when the content changes. A no-op refresh costs a few file reads and stats, starts no `cs` and no JVM, and returns at once in a workspace with no IDE files.
- **`--output`** suits a Metals script importer: a `scpm.mbt.sh` at the workspace root containing `exec scpm ide metals --output "$MBT_OUTPUT_FILE"`.
- **Deviations from the published schema.**
  - `scalaVersion` and `classDirectories` are not in `mbt.schema.json` (which says `additionalProperties: false`), but `MbtNamespace` reads both and Metals' own importers write both. Without `scalaVersion`, every namespace would get Metals' fallback Scala version.
  - `testClasses` are not written: scpm only knows them after compiling. Metals discovers tests itself.
  - Run and debug in Metals v2 go through a build-tool launcher (Maven, Gradle, Bazel or sbt), and scpm doesn't have one yet. That is BSP territory (§10.3).
- **Cross-built modules** give two namespaces the same shared source directories (`src/main/scala`). As with sbt cross builds over BSP, Metals picks one of them for a shared file.
- **Validation.** The generated files for the `app`, `basic` and `java-deps` fixtures were loaded with Metals v2.0.0-RC1's own `MbtBuild.fromWorkspace` and `mbtTargets`, which parsed them without warnings and derived the expected targets, Scala versions, classpaths and dependency sources. A full editor session (navigation through the language server) has not been tried yet.
- **Scala versions.** v2 (2.0.0-RC1) is **not Scala 2-only**, although its build file lists only 2.12/2.13: that list covers the presentation compilers Metals builds itself.
  - Scala 3's presentation compiler ships inside scala/scala3 and v2 uses it.
  - v2's test suite runs mbt.json with mixed 2.13 and 3 modules.
  - **v2's sourcepath-based fast navigation needs Scala 3.3.8+ (LTS) or 3.8.4+.** Older Scala 3 compilers silently ignore the sourcepath and probably fall back to slower behaviour **[inferred]**.
  - `scpm doctor` should warn when a module's Scala 3 version is below that floor.
- **Metals v1** is still maintained in parallel (1.6.9, with features landed twice), and the Metals site recommends it for smaller projects. It needs BSP, below.

### 10.2 IntelliJ: why BSP feels like a downgrade, and what to do about it

What actually differs between IntelliJ's *native* sbt import and a BSP import (from the intellij-scala `bsp-builtin` sources and the sbt 2 docs):

- **Same in both:** highlighting (IntelliJ's own type-checker by default), the project/module/library model (BSP import is itself an IntelliJ "ExternalSystem"), PSI refactorings and inspections.
- **Different:**
  - *Build*: native uses JPS plus IDEA's in-process Zinc; BSP sends `buildTarget/compile` to the server.
  - *Run/test/debug*: IntelliJ's own runners **only if** the server implements `jvmRunEnvironment` / `jvmTestEnvironment` well.
  - *Source jars*: quality depends on the server's `dependencySources`.
  - *Sync*: IntelliJ **ignores `buildTarget/didChange`**, so every build change needs a manual "Refresh". bleep#306, open since 2023.
  - *Other*: no sbt shell, and worksheets need a target picked by hand.
- sbt's own 2.x docs rate BSP import "less mature" than native import.

So the downgrade is mostly **server quality plus IntelliJ's weaker BSP sync UX**, not highlighting. To find out which baseline you're comparing to, import one project three ways: native sbt, sbt-BSP and bleep. If sbt-BSP is as bad as bleep, the problem is IntelliJ's BSP client; if only bleep is bad, it's bleep's server.

**Plan: offer three IntelliJ paths, cheapest first.**

1. **A good BSP server (Phase 2)**, needed anyway for Metals v1 and other editors.
   - Implement `jvmRunEnvironment`, `jvmTestEnvironment`, `dependencySources` and `javacOptions`/`scalacOptions` carefully, since those are what drive the native runners.
   - Its compile shares the daemon's warm Zinc, so there is no double compile.
2. **`scpm idea` (Phase 2)**, which generates `.idea/` module, library and SDK XML directly from the resolved config, like Mill's GenIdea.
   - Gets IntelliJ's native JPS build and runners today, with no plugin.
   - Costs: re-run on config changes (the CLI can do that automatically after `install`); IDE and CLI compile separately into different output dirs; generated sources must be produced first.
   - Mill's docs call GenIdea "probably the preferred way" for polyglot or unusual projects.

   **Built (M8): `scpm idea`** (also `scpm ide idea`). The XML follows Mill's GenIdea: `runner/idea/src/mill/idea/GenIdeaImpl.scala` and the expected files under `integration/feature/gen-idea/resources/`, at com-lihaoyi/mill commit `cf391cdb` (2026-11-25).
   - **One IntelliJ module per scpm module**, written to `.idea/modules/<module>.iml`. It is built from the module's *primary* variant: the first variant (the first Scala version listed), or the one `ide: {primary: <selector>}` in the module file names. A selector that matches no variant is a config error at `ide.primary`. IntelliJ can't have two modules share a content root, so a cross-built module's other variants are not IntelliJ modules.
     - **Deviation from GenIdea:** main and test sources share one module (`isTestSource`), as Maven and Gradle imports do. GenIdea writes a separate test module instead.
   - **Dependencies come from the lock's four classpaths.**
     - An entry on both the compile and runtime classpaths is COMPILE. One on the compile classpath only is PROVIDED, on the runtime classpath only is RUNTIME, and on the test classpaths only is TEST.
     - External artifacts are project libraries named `scpm: <lock key>`, in `.idea/libraries/scpm__*.xml`, with sources jars attached as for Metals.
     - Workspace dependencies become module dependencies. They are not exported, because each module already lists its whole jointly resolved classpath. Following GenIdea, they come after the libraries.
     - When a dependency points at a variant that isn't its module's primary, the module gets a module library on scpm's output for that variant (`.scpm/out/<id>/classes`) instead, and scpm warns that `scpm compile` must produce it.
   - **Scala SDKs.** Each Scala version gets a Scala SDK library, `scpm: scala-sdk-<version>` (`type="Scala"`). It has `<language-level>Scala_<major>_<minor>` (intellij-scala's `ScalaLanguageLevel`) and a `<compiler-classpath>` taken from the lock's `scala-compiler@<version>` boot entry. Scala 3 SDKs also get `<compiler-bridge-binary-jar>` from the `compiler-bridge@<version>` boot entry, as the sbt and Gradle importers set. Non-jar artifacts (`@aar`, `@exe`) are left out of IntelliJ and of mbt.json. Scalac options and plugin jars go into `scala_compiler.xml` profiles. A module whose own `release` differs from the project JDK gets `LANGUAGE_LEVEL="JDK_<n>"`.
   - **Output directories.** IntelliJ's JPS compiles into `.scpm/idea-out/<id>/{classes,test-classes}`, never into scpm's `.scpm/out`.
   - **Ownership.** scpm generates these files, and each starts with a "Generated by scpm" comment:
     - `modules.xml`, keeping entries for modules scpm didn't generate;
     - the `.iml` files;
     - `libraries/scpm__*.xml`;
     - `scala_compiler.xml`;
     - `misc.xml` (project JDK named by its major version, e.g. `25`).

     **An existing file is replaced only if scpm owns it:** it still carries the comment, or it is listed in `.scpm/idea-out/generated`, the files scpm wrote last time. The manifest is needed because IntelliJ drops comments when it re-saves a file. Any other existing file, such as a hand-made `util.iml` or a `scala_compiler.xml` with the user's own options, is left alone with a warning naming it; `scpm idea --force` replaces it.
     - `misc.xml` is stricter, because the IDE keeps the project JDK there: it is written only when it doesn't exist or still carries the comment (or with `--force`).
     - Everything else in `.idea` (`workspace.xml`, run configurations, …) is never touched.
     - Modules and libraries that scpm generated before and no longer generates are deleted.
     - Two modules sharing an out-of-tree source directory (`../shared`): only the first gets it as a content root, and scpm warns.
     - Files are written `0644` (a replaced file keeps its mode), and only when their content changes.
     - After `scpm idea` has run once (or always, with `ide: {idea: true}`), the same automatic refreshes as for Metals regenerate the files, with cached sources jars only.
   - **Not validated in IntelliJ yet.** The output is checked structurally (tests on the fixtures) and against GenIdea's shapes, but no IntelliJ instance has opened it. Javac options per module (`compiler.xml`) and generated sources are not written yet.
3. **A native IntelliJ ExternalSystem plugin (Phase 5).** This is what Gradle, Maven and sbt (via sbt-structure) do, and the real "native" experience: IntelliJ owns the project model and sync, with a choice of JPS or scpm builds.
   - **Much cheaper for us than for sbt.** sbt's importer has to run sbt plus a plugin to extract the structure. Ours just runs `scpm config --resolved --json` (the same model `mbt.json` comes from) and watches the YAML files, so sync is instant and automatic.
   - **This is a concrete differentiator.** bleep's IntelliJ plugin is only a BSP launcher and wrapper, not a native importer.
   - The cost is maintaining a Kotlin plugin against IntelliJ API churn. Start it once the config model has stabilised.

### 10.3 BSP scope

`scpm bsp` is a Rust front-end that delegates compile, test, run and debug to `scpmd`. The spec has been stagnant since 2.2.0-M3, so we implement a narrow, stable subset well rather than all of it.

**Built (M9): `scpm bsp`**, BSP 2.1.0 over stdio with LSP-style `Content-Length` framing. The subset is what the two clients that matter actually send. That was taken from their sources: intellij-scala `bsp-builtin/bsp` (`BspProjectResolver`, `BspResolverLogic`, `BspTask`, `BspJvmEnvironment`, `BspTestRunner`) and Metals `main` (`BuildServerConnection`, `ImportedBuild`, `Diagnostics`, `ForwardingMetalsBuildClient`, `BuildTargetClasses`).

- **Connection file.**
  - `scpm bsp --setup` writes `.bsp/scpm.json`: `name`, `version`, `bspVersion`, `languages` (`scala`, `java`), and `argv`, which is scpm plus `bsp`. Clients start that command in the workspace root.
  - The binary is recorded by the path it was invoked through, made absolute but **not resolved**: `argv[0]` if it is a path, else where `PATH` finds it. A Homebrew or mise shim therefore stays a shim, and upgrades, which delete the versioned target, keep working.
  - Ownership follows the mbt.json rule. An existing file is replaced only if its sha256 matches `.scpm/ide/bsp.json.sha256` (what scpm last wrote), or with `--force`.
    - While writing, the marker lists both the old and the new hash, so a write interrupted at any step leaves the file owned and repairable.
    - `--setup` and install refreshes hold `.scpm/ide/bsp.json.lock` from the ownership check through both writes, so a refresh can't overwrite a concurrent `--setup`.
  - The automatic IDE refresh after a non-frozen install only repairs an owned file. If the recorded binary no longer exists, it is re-pointed at the scpm running now. If the recorded binary is this same binary (compared after resolving symlinks), only the `version` is brought up to date. Any other scpm binary leaves it alone, so `scpm2 install` doesn't take over the workspace. The refresh never creates the file.
  - `--setup` warns if the workspace also has native IntelliJ files from `scpm idea`, because the two setups conflict.
- **Lifecycle.**
  - `build/initialize` (every capability flag is sent explicitly, because both clients unbox them), `build/initialized`, `build/shutdown`, `build/exit` and `$/cancelRequest`.
  - Requests before `initialize` get -32002, and requests after `shutdown` get -32600, and so do builds that were still waiting for the build lock when `shutdown` came. `exit` without `shutdown` exits 1. On `exit` (or the end of input) in-flight work is cancelled, and the server waits at most 5 s for it: a request that can't be cancelled, such as a resolve in progress, doesn't hold the exit.
  - Unknown methods get -32601. Errors are JSON-RPC errors, and a cancelled request is answered with -32800.
  - **A broken config** (or a failing resolve) doesn't take the server down. The last good model keeps being served, and the client gets one `build/showMessage` (error) with the diagnostic per distinct failure. An invalid config is not reloaded again until a file changes; a transient failure (a resolve or download, e.g. offline) is also retried every 30 s. The server recovers by itself once the problem is gone. The failure is recorded against the revision that was tried: if a file changed during the attempt, the next request tries again. An explicit `workspace/reload` answers with the error. With no good model yet, requests fail with -32603.
  - Only protocol messages reach stdout. On Unix the real stdout is moved to a private descriptor (close-on-exec), and descriptor 1 is pointed at stderr, so stray output from scpm or from a child process can't corrupt the stream. Logs go to stderr.
- **Targets** (`workspace/buildTargets`).
  - Each variant is a target with displayName `core@jvm-3`. Its tests are a second target, `core@jvm-3:test`, if a test source directory exists.
  - Target ids are `<module dir URI>/?id=<displayName>`.
  - Both targets have the module directory as `baseDirectory`. IntelliJ merges targets that share a base directory into one module, putting the `test`-tagged one in its test scopes. That is also why the test target is separate: its classpath carries the test-only dependencies. Metals ignores tags and uses `scalaTestClasses`.
  - The test target lists the main target first in `dependencies`. IntelliJ turns classpath entries that equal a dependency's `classDirectory` into module dependencies.
  - Tags are `application` (the variant has a `mainClass`), `library` or `test`.
  - Data:
    - `scala` for Scala variants: `scalaOrganization`, `scalaVersion`, `scalaBinaryVersion`, platform JVM, `jars` (the locked `scala-compiler@<version>` boot closure), and `jvmBuildTarget` with `javaHome` and `javaVersion`.
    - `jvm` (`jvmBuildTarget`) for Java-only variants.
  - **Deviation:** variants of a cross-built module other than its `ide.primary` one carry the spec's `no-ide` tag. Otherwise IntelliJ would merge them into the primary's module and pick the higher Scala version, with both versions' jars on the classpath. This matches `scpm idea`. Metals honours the tag too, so like sbt's BSP it only sees one Scala version per module.
- **Queries.**
  - `buildTarget/sources`: source directories, `kind: 2`, `generated: false`, with a trailing slash (IntelliJ needs it).
  - `buildTarget/resources` and `buildTarget/inverseSources`. The latter also matches files reached through a symlinked source directory, comparing resolved paths.
  - `buildTarget/outputPaths`: `.scpm/out/<id>/`, or for a module at the workspace root, the whole of `.scpm/`, `.metals/` and `.bsp/`.
    - IntelliJ applies outputPaths only under the module's own base directory, and only to modules with a source root there (`BspResolverLogic.createModuleNode`). Its synthetic `<dir>-root` module, which covers the workspace root when no module lives there, gets only `.bsp`, `.bloop` and its own output directory excluded.
    - **Gap:** in a workspace without a root module, IntelliJ indexes `.scpm/`. Mark it excluded once in IntelliJ (Mark Directory as → Excluded), (not verified to survive a re-import). A synthetic root target would fix it, but every client would see that target.
  - `buildTarget/scalacOptions` (Scala targets only) and `buildTarget/javacOptions` (all targets). Options come from `compiler_options`, and the classpath and `classDirectory` are exactly what `scpm compile` compiles with. A test target's classpath is the main classes directory plus the test-compile classpath.
  - **SemanticDB** (`ide: {semanticdb: true}` in the workspace file). Metals v1 needs it for references, rename, implementations and run/test code lenses. `compiler_options` adds it, so `scpm compile`, `scpm bsp` and mbt.json all get the same options and fingerprints don't thrash between them.
    - Output goes to the classes directory, under `META-INF/semanticdb/<path relative to the workspace root>.semanticdb`, which is where Metals looks (`FileSystemSemanticdbs`, `ScalaTarget.targetroot`).
    - Scala 3: `-Xsemanticdb -sourceroot <root> -semanticdb-target <classes>`.
    - Scala 2: the `org.scalameta:semanticdb-scalac_<full>` plugin, locked as the boot entry `semanticdb@<full>+<version>`. The plugin is published per Scala patch version, and each scalameta release covers only a window of them, so scpm maps each version to the newest scalameta release publishing it (`4.17.4` for 2.13.15–18 and 2.12.18–21, older ones for older patches). A Scala 2 version outside the table (before 2.12.4, or newer than scpm knows) is an install error naming it, plus `-Yrangepos -P:semanticdb:sourceroot:<root> -P:semanticdb:targetroot:<classes> -P:semanticdb:failures:warning`.
    - The source and target roots are explicit because the compiler runs in the daemon's directory. They are absolute, so a worktree at another path recompiles once.
    - `scpm idea` leaves these options out, since IntelliJ's own compiler must not write into scpm's outputs. Java-only modules get no SemanticDB (no semanticdb-javac yet).
    - When the client is Metals (its `displayName`, or `semanticdbVersion` in the initialize data) and the setting is off, the server sends one `build/showMessage` naming the setting.
  - `buildTarget/dependencySources`: sources jars as for `scpm ide …`, cached ones plus a single `cs fetch --sources` for missing ones, with the same no-sources memory.
  - `buildTarget/dependencyModules`: `maven` data with the jar and, if cached, the `sources` artifact.
  - `buildTarget/jvmRunEnvironment` and `buildTarget/jvmTestEnvironment`, which drive IntelliJ's and Metals' own runners.
    - The classpath is `scpm run`'s or `scpm test`'s runtime classpath, with outputs listed even before they exist.
    - Also: the variant's `jvmOptions` and `env`.
    - The working directory is the module directory for tests (as `scpm test`) and the workspace root for programs.
    - `mainClasses` (IntelliJ ignores them).
  - `buildTarget/scalaMainClasses`: the configured `mainClass` first, then every public class with `public static void main(String[])` found by reading the compiled class files' method tables. This covers Scala objects, `@main` methods and Java.
  - `buildTarget/scalaTestClasses`: the test runner's new `discover` method (worker protocol). It loads the compiled test classes with the same framework fingerprints `scpm test` uses, without running anything, and gives one item per framework. Results are cached per variant until its test-compile fingerprint changes. Uncompiled tests list nothing. Discovery that takes more than 60 s is killed and lists nothing, with a `build/logMessage`.
- **Builds.** Builds run one at a time on the shared worker slot (the daemon when available, as for `scpm mcp`), holding the workspace's `.scpm/out/.lock`. A cancel also breaks the wait for that lock.
  - `buildTarget/compile` runs `compile::compile`, with test targets as `--tests`, and returns a `StatusCode`. A compile of no targets, which is Metals' liveness ping, is answered at once without waiting for a running build.
    - Every request gets a `build/taskStart`/`taskFinish` pair, with `build/taskProgress` after each task. Each scpm task gets `compile-task` / `compile-report` (`errors`, `warnings`, `time`, `noOp` for up-to-date tasks; Metals needs both). `eventTime` is always set.
    - Each task starts and finishes once per request. A request with both test and main targets compiles in two passes, and the second pass's up-to-date repeats of tasks already reported are dropped.
    - Tasks stopped by a cancel finish as cancelled (status 3), with no error log. That covers tasks killed mid-compile and those skipped after them.
    - After each compile task, `build/publishDiagnostics` goes out per file with `reset: true`. Files that had diagnostics and are now clean get an empty reset.
    - An up-to-date task republishes the diagnostics `scpm compile` persisted for it, so warnings survive a server restart. A skipped or crashed task leaves them alone.
    - Lines and characters are 0-based. Single-file quick fixes become `scala` diagnostic data (`ScalaAction`).
  - `buildTarget/test` compiles like `scpm test`, then runs `testing::run_tests` per variant.
    - `scala-test` (`testClasses`, as IntelliJ's BSP test runner sends), `scala-test-suites` and `scala-test-suites-selection` select suites and cases. A `scala-test` selection whose targets no longer exist is rejected (-32602) instead of running every suite. Cases chosen per suite run as one runner request per suite, each with its own report (`test-reports/junit.xml`, `junit-2.xml`, …).
    - Output goes out as `build/logMessage`. Each case gets a `test-start`/`test-finish` pair with the failure location, and each variant a `test-task`/`test-report`.
  - `buildTarget/run` compiles, then runs `scpm run`'s command, with `scala-main-class` data (class, arguments, JVM options, environment), `arguments`, `environmentVariables` and `workingDirectory`. Output lines become `build/logMessage`s, and the exit code becomes the status.
    - The run ends when the JVM exits. If a process it started keeps its stdout or stderr open, the pipes get a 2 s grace period, then the program's process group is killed (with a warning). The build and workspace locks are never held for such a process.
    - The target, main class, JVM options and environment are resolved from the same model the build compiles with, even if the model reloads while the request waits for the build lock. `compile` and `test` re-resolve their targets the same way, and fail (status 2, with a message) if one of them no longer exists. A request for no targets stays a no-op.
  - Cancellation:
    - Compiles are cancelled in the daemon.
    - Test JVMs are killed.
    - A program's process group is killed.
  - `buildTarget/cleanCache` deletes a target's classes, analysis, fingerprint and saved diagnostics, under the build lock, and answers `cleaned: true` (IntelliJ refuses to rebuild otherwise).
- **Reloading.** The server keeps the loaded workspace and lock. The automatic IDE refresh that comes with a reload updates `.metals/mbt.json` and `.bsp/scpm.json` but **not** the native IntelliJ files of `scpm idea`, which would fight a BSP import. Before each request it stats the workspace file, the module files the `modules` globs match now (so a new or deleted module counts), the test source directories and the lock.
  - The stamp is taken after a load. A config file whose modification time is at or after the load's start (less 1 s, for coarse timestamps) is recorded as unknown, so an edit made while loading triggers a reload on the next request. The lock is exempt, because the load writes it itself. If one changed, it reloads, installing a stale lock as every CLI command does, and sends `buildTarget/didChange` for the targets whose answers changed (created, changed or deleted). `workspace/reload` reloads unconditionally, without waiting for a running build (builds keep the model they started with), and also notifies only on real changes: IntelliJ calls `reload` at the start of every import and re-imports on `didChange`, so always notifying would loop.
- **Deviations and gaps.**
  - No debugging (`debugProvider` is empty, so Metals runs locally from the JVM environments).
  - No `buildTarget/wrappedSources`, `jvmCompileClasspath` or `run/readStdin`; a program's stdin is empty.
  - `arguments` of `compile` and `test` are ignored.
  - `test`'s `environmentVariables` and `workingDirectory` are not applied: tests run with the variant's own `env` in the module directory, like `scpm test`.
  - SemanticDB is opt-in (`ide.semanticdb`, above). It is not switched on per client, because the options must stay exactly what `scpm compile` uses.
  - In a workspace without a root module, IntelliJ indexes `.scpm/` (above).
- **Validation.**
  - `crates/scpm/tests/bsp.rs` drives `scpm bsp` over its real stdio against copies of the `app`, `basic` and `java-deps` fixtures. It covers:
    - the lifecycle;
    - targets, sources and options, the environments, discovery and `didChange`;
    - each task finishing exactly once;
    - diagnostics published and cleared through the daemon;
    - tests and runs;
    - cancellation of a running program, of a test request and of a compile mid-way (reported as cancelled);
    - SemanticDB options and files for Scala 3 and 2.13, with `scpm compile` up to date afterwards, and the Metals hint;
    - a broken config that keeps the last good model.
  - A bsp4j 2.1.1 client (the library IntelliJ and Metals use), with lsp4j's strict message validation on, ran every supported request and accepted every response and notification.
  - IntelliJ and Metals themselves have not been tried yet.

---

## 11. Release management

Copied from pnpm 11.11+ with JVM-specific checks added:

- **`scpm change`** writes changesets-format intent files to `.changes/*.md`.
- **`scpm change check`** runs on every PR. Beyond pnpm, it **validates the declared bump against reality**:
  - runs MiMa on every Scala and Java module (bytecode compatibility), plus tasty-mima for Scala 3 modules (TASTy compatibility), against the last published version;
  - a binary-breaking change declared as a patch or minor fails;
  - this is checked against the module's `versionScheme`, which is also written to the POM as `info.versionScheme`.
- **`scpm version -r`**:
  - bumps versions;
  - propagates bumps through `workspace:` edges;
  - writes changelogs;
  - appends to an append-only **ledger** (`.changes/ledger.yaml`), so cherry-picks between release branches stay safe;
  - creates no commit.
- **Lanes**: a module on a lane releases pre-releases alongside stable modules. The format must sort correctly under **Maven/coursier version ordering**: `1.4.0-RC1`, `1.4.0-M2`, not pnpm's `-rc.1` **[verify coursier ordering of the chosen format]**. `scpm lane main -F mod` graduates a module back to stable.
- **Fixed groups** and **epics**, as pnpm defines them.
- **`scpm publish -r`**:
  - rewrites `workspace:`, `catalog:` and ranges to concrete versions;
  - signs with PGP (Central requires it), optionally adding Sigstore;
  - publishes to the Central Portal Publisher API (bundle upload, then poll status), GitHub Packages or Artifactory.
  - It may embed `scala/scala-publish`.

---

## 12. Answers to the open questions

**Single root config vs per-module?**
Split (§4). Explicitness is kept by explicit `workspace:` edges plus `scpm graph`. Sharing is via catalogs, root task defaults, presets with `extends:` and, later, config dependencies. There is no implicit directory inheritance.

**CLI language?**
Rust.
- Scala Native is out: there is no coursier core for it (only version-parsing and coordinate-parsing artifacts), multithreading only stabilised in 0.5.12, and Windows support is weak.
- scala-cli with GraalVM native-image is a credible second choice (bleep, Mill, sbtn and cs all use it). It would let the CLI share model code with the daemon. But it gives up the Intel-mac binary, needs one native build per OS and architecture, and makes the JVM-free fast paths (fetching, hashing, HTTP cache) less natural to write.
- The decisive argument is not the 1 ms vs 10–70 ms startup. It is that a Rust CLI can own *all* the non-compile work without a JVM.

**Can we call coursier from Rust?**
- Not directly: there is no C ABI.
- Spawn the native `cs` (~40–130 ms per warm resolve), then move to a resolver sidecar on the coursier API: a JVM jar first, native-image later (§5.2.1).
- Skipping coursier means reimplementing Maven resolution: months of edge cases, with results that diverge from the rest of the ecosystem. Don't.

**Rust CLI with a Scala daemon?**
Yes. That's the recommended architecture, with the daemon kept deliberately narrow.

**A Rust daemon calling scalac some other way?**
Only as an orchestrator. Compilation has to happen in a JVM (no native scalac), so it's a Rust orchestrator plus JVM workers.

**Is a daemon needed, or can Zinc start fast enough?**
- Locally: a daemon is needed. Cold is 3.4–5× slower than warm, and still ≈2–2.4× with an AOT cache (§6.1).
- In CI: no detached daemon. Use one foreground worker per job, an AOT cache, and above all the remote cache.

**How much can coursier do; where are its limits?**
See §5.1. It does resolution, BOMs, overrides, cache, mirrors, credentials and the JVM index. Its limits: no lockfile, no sha256 in reports, no semver policy, no publish, no non-JVM API, no range unions.

**Artifactory / GitHub extras?**
Optional fast paths for latest-version and checksum lookups, plus better auth UX (§5.7). There's also GitHub Dependency Submission and the Checks API (§9.2).

**Can we allow npm-style ranges without breaking the ecosystem?**
Yes, if ranges stay in our manifests and lockfile and never reach published POMs (§5.5).

---

## 13. Things you didn't ask about

1. **The name: `scpm`.** The original working name, `spm`, is Swift Package Manager. Collision check for the candidates (Homebrew, crates.io, npm, Repology, GitHub, Scaladex; trademarks **not** checked):

   | Name | Collisions | Notes |
   |---|---|---|
   | **stige** | None found | Norwegian/Danish for "ladder", and *scala* is Italian for "ladder/staircase". Distinctive and greppable. |
   | **scpm** | Only an obsolete SUSE 9-era binary | Self-explanatory ("Scala package manager"), unique token, a bit dry. |
   | **skal** | Small 2022 npm package | 4 letters, home row, "skål" pun. |
   | rung | Small Rust CLI with a `rung` binary, npm packages | Common word, so grep is noisy. |
   | spam | crates `spam` | Cute, but embarrassing in logs and search results. |
   | scap, scalino, cask, sk, scl | Real conflicts | Wikimedia/NIST SCAP, an active Scala Native tool, Homebrew Cask, skim, Red Hat SCL. Avoid. |

   Chose **scpm**. Before anything is public, still to do: check trademarks, and claim the crates.io, npm and GitHub org names and the `scpm.dev` domain used in the schema URL.
2. **bleep overlap.** Decide deliberately whether to compete, collaborate or fork (§2). At the very least, read bleep's compile-server and portable-analysis code before writing ours.
3. **JDK management** is part of "fast CI". Pin the project JDK in the lock (`boot.jdk`), so `actions/setup-java` isn't needed. Phase 1 obtains it with `cs java-home --jvm <id>` (native, cached); a Rust fetcher reading coursier's JVM index replaces that in Phase 3.
4. **Test running** is its own subsystem:
   - sbt `test-interface` for munit, ScalaTest, weaver and ZIO Test, in forked JVMs;
   - structured per-test events;
   - test sharding across CI matrix jobs (`scpm test --shard 2/4`, balanced by recorded durations);
   - flaky-test retry with reporting.
5. **Formatting and linting are first-class**, as they are in the TS world (`biome`, `eslint`):
   - scalafmt and scalafix run *inside the warm worker*, not via coursier-launched JVMs;
   - versions are pinned in the lock;
   - `scpm fmt` should feel instant.
6. **sbt import** decides adoption. Most target repos are sbt, so we need `scpm import sbt`:
   - run sbt once to dump settings, as bleep does;
   - generate the workspace, module manifests and catalogs;
   - map common sbt plugins (buildinfo, assembly, native-packager, scalajs-bundler, sbt-ci-release) to built-ins or script modules.
7. **Mixed Java and Kotlin.** Java is free via Zinc. Kotlin is a real ask in mixed shops; bleep cites it as a reason it dropped Bloop. Plan for Kotlin in the worker protocol even if it's not implemented.
8. **Scala.js and Scala Native linking.** These are long-running, memory-hungry tasks that belong in the worker too, with incremental linker state cached like Zinc analysis.
9. **Dependency update automation.** This is the first thing people automate. See §5.8 for the Renovate preset, lock fix-up action, and first-party `scpm upgrade` bot.
10. **Windows.** Unix sockets exist on Windows 10+, `fcntl` locking does not (coursier uses `LockFileEx` via the JDK there), and copy-on-write clones are only possible on ReFS. Decide early whether Windows is tier 1.
11. **Tool version pinning and self-bootstrap (Phase 3).** `scpm: 0.4.2` in the workspace file means any `scpm` binary re-execs the pinned version (bleep does this; so do pnpm's `packageManager` field and Corepack). Distribute via mise, Homebrew, `curl | sh`, a GitHub Action, and `cs install`.
12. **Telemetry.** None by default. Write local timing data to the event log instead. `scpm report --since 7d` gives people the performance insight without phoning home.
13. **Hermeticity limits.** Macros, annotation processors and `sourceGenerators` can read the filesystem and network. The cache key can't see that, so provide `inputs`/`env` declarations and an `--no-cache` escape hatch, and say so plainly in the docs.

---

## 14. Risks

| Risk | Mitigation |
|---|---|
| Scope: this is pnpm + sbt + changesets + turbo. | Strict phasing (see [roadmap](roadmap.md)): each phase must be useful on its own. Phase 1 alone (lockfile, catalogs, fast compile, GitHub annotations) is already better than the status quo. |
| bleep closes the gap first. | Differentiate on the package-manager half (lock, ranges, catalogs, releases); consider collaborating. |
| Writing to the coursier cache from Rust corrupts it. | The Phase 1 fetcher ships behind a compatibility suite that runs `cs` and Rust concurrently on one cache (locks, `.part` files). |
| Daemons accumulate memory (bleep's experience). | Idle timeout, heap governor, `scpm daemon top`, and one daemon per worker JDK rather than per workspace. |
| Zinc portable analysis has edge cases (Zinc 2.1 just fixed several). | Fall back to a full compile on analysis mismatch, and report it as an event so it's visible. |
| Ranges confuse users coming from Maven/sbt. | Exact versions by default; ranges opt-in per dependency. |
| Comment-preserving YAML round-trip editing is weak in Rust; it affects `scpm add` and the MCP edit tools. | Edit by span: parse with positions, splice the minimal text change into the original, re-parse to check. Refuse with a diff rather than reformat the file. |
