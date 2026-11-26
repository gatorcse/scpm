# scpm roadmap

Each phase must be useful on its own and has an explicit exit criterion. Rough sizes assume
one to two people.

## Phase 0 — Spikes (2–4 weeks)

Kill the riskiest assumptions with throwaway code before committing to the architecture.

**Spikes:**

- [ ] **Config.** Rust loader for `scpm-workspace.yaml` + `scpm.yaml` with JSON Schema validation, presets, catalogs and variants, following the resolution order in design §4.4. Target: under 20 ms for 500 modules.
- [ ] **Resolve.** Check which parts of the resolution context (JDK, OS, arch) the native `cs` CLI can apply. Drive native `cs fetch -j` from Rust and turn the report into a draft `scpm-lock.yaml`, computing sha256 values.
- [ ] **Lock restore.** Rust fetches locked artifacts into a coursier-layout cache, run concurrently with `cs` against the same cache. This proves the lock and `.part` files interoperate, and seeds the Phase 1 compatibility suite.
- [ ] **Worker.** A minimal Scala JVM worker embedding Zinc 2.1, compiling one module over JSON lines on stdio (docs/worker-protocol.md) and streaming diagnostics.
- [ ] **Startup economics on a real GitHub Actions runner:**
  - cold vs warm compile
  - with and without a JDK 25 AOT cache, measured through Zinc (scalac in a `ScalaInstance` class loader), not scalac on the application class path
  - whether the AOT cache is portable between runners
- [ ] **Portable analysis.** Compile in worktree A, clone the outputs copy-on-write into worktree B, and confirm the next compile in B is incremental.
- [ ] **Metals v2.** Hand-write an `.metals/mbt.json` for a two-module project, one Scala 2.13 and one Scala 3.8.4+, and confirm v2.0.0-RC1 gives full navigation in both.
- [ ] **IntelliJ baseline.** Import one real project as native sbt, sbt-BSP and bleep-BSP, and write down concretely what is worse in each. That tells us whether the problem is IntelliJ's BSP client or the server, and how early we need `scpm idea` or a plugin.
- [ ] **Talk to the bleep maintainer** about overlap and code reuse, and check the licence.
- [x] **Pick a name**: `scpm`. Still to do: trademark check, and claim the crates.io, npm and GitHub org names and the domain.

**Exit:** a go/no-go decision on Rust CLI + Zinc worker, and numbers to put in the README.

## Phase 1 — "It builds, and CI is fast" (MVP)

**Config and dependencies**
- The full v1 schema (design §4) is accepted. Keys for features not built yet are an error, "`<key>` is not supported yet (planned for Phase N)", whenever they would change behaviour; never silently ignored.
- Workspace + module config: catalogs (default and named), BOMs, overrides, the `workspace:*` protocol, presets with the normative resolution order (§4.4), and four dependency scopes.
- Semver ranges (§5.5) in dependencies and catalogs, resolved to exact versions at lock time.
- Module settings: sbt/Maven source layout plus `sources`/`resources`/`testSources`/`testResources`, `scala: none` (Java-only), `compilerPlugins`, `javacOptions`, `release`, `mainClass`, `jvmOptions`, `env`. (`generatedSources` comes with tasks in Phase 3.)
- `scpm install` / `--frozen` and `scpm-lock.yaml` (§5.4): variant ids even for single-variant modules, four classpaths per variant, joint resolution over the workspace closure, per-variant `specifiers` and `inputsHash`, a fixed resolution context, the boot section, sha256 values.
- Stale lock: locally `compile`/`test`/`run` install first and print what changed; with `CI=true` or `--frozen` it is an error. `scpm ci` = `scpm install --frozen`, then `compile` and `test` over the whole workspace (it switches to `scpm pipeline` in Phase 3).
- Resolution through the `cs` CLI. Project JDK via `cs java-home --jvm <id>` (native, cached).
- Rust lock-restore fetcher: direct jar downloads into the coursier cache layout, no POMs, with repository `credentials` and coursier's `COURSIER_CREDENTIALS` / `credentials.properties`. Ships behind a compatibility suite that runs `cs` and Rust concurrently on one cache.

**Build commands**
- `scpm compile|test|run` on JVM Scala 3, Scala 2.13 and Java, compiling for the project JDK's release target on the worker JDK (§6.3).
- Cross-building: Scala 2.13 × 3 variants on the JVM.
- Rust input fingerprinting and no-op compile detection: a no-op `compile` starts no JVM.
- The worker runs in the foreground (no daemon yet); tests run in forked JVMs with sbt test-interface.

**Reporting**
- Event stream; reporters for `tty`, `plain`, `--json` and `github` (annotations ranked within the cap, plus step summary).
- JUnit output.

**Inspection, agents and CI**
- `scpm graph`, `scpm why`, `scpm config --resolved`.
- `scpm mcp` with the read-only and build tools (`compile`, `test`, `diagnostics`, `graph`, `why`, `config`). It falls out of `--json` almost for free, and agents are a primary user. Until the daemon lands, it owns a foreground worker for its lifetime.
- `setup-scpm` GitHub Action: install, cache coursier + JDK + AOT keyed by the lock, auto-detect the reporter.

**Exit:** a real 10–30 module open-source Scala project builds and tests in GitHub Actions faster than its sbt build, with readable annotations.

## Phase 2 — Daily driver

- Shared `scpmd` daemon with idle timeout and heap cap; `scpm daemon ls|top|stop|logs`. `scpm mcp` switches to it.
- `scpm doctor`: environment and config checks (JDKs, cache, credentials, stale daemons, Scala 3 versions below the Metals v2 floor).
- IDE: `.metals/mbt.json` generation, a narrow BSP server (`scpm bsp`) with solid run/test environments and source jars, and `scpm idea` to generate native IntelliJ project files.
- `scpm worktree add|prune` with copy-on-write seeding of outputs and analysis.
- Cross-building to Scala.js and Scala Native: `js` and `native` platforms, linking in the worker.
- `scpm fmt`, `scpm lint` (scalafmt and scalafix in the warm worker), `scpm console`.
- Dependency maintenance:
  - `scpm add|remove|update|outdated|dedupe`
  - `versionScheme` conflict checks
- MCP write tools: `add_dependency`, `remove_dependency`, `update`, `outdated`.
- `scpm import sbt` covering the common plugin mappings.
- Renovate preset (JSONata manager) plus a lock fix-up GitHub Action that works with the free hosted Renovate app (design §5.8).

**Exit:** a team can use scpm instead of sbt for a working week without opening sbt.

## Phase 3 — CI-first orchestration

- pnpm-style `tasks:`:
  - `^dep` edges, eager scheduling, `concurrencyGroup`
  - `--dry-run --json`, `--resume-from`
- `scpm pipeline` with affected selection, continue-on-failure and a single consolidated report.
- Local content-addressed task cache, with outputs restored and logs and annotations replayed.
- Remote cache:
  - Turborepo-API or Bazel-remote-cache compatible, plus a GitHub Actions cache backend
  - signed entries
  - portable Zinc analysis seeding
- Test sharding across matrix jobs, balanced by recorded duration; flaky retry reporting.
- GitHub extras: Checks API reporter for more annotations than the workflow-command cap, SARIF output, Dependency Submission.
- `scpm audit` (OSV querybatch), `minimumReleaseAge` with exact-version, expiring CVE exemptions, and `scpm audit --fix`.
- Resolver sidecar: native-image app on the coursier API giving structured errors, metadata-only resolve, version listing and repository/sha256 per artifact.
- Rust JDK fetcher from coursier's JVM index, replacing `cs java-home`; `boot.jdk` gains a URL and sha256 per `os-arch`.
- Self-bootstrap: the workspace's `scpm:` version pin makes any `scpm` binary re-exec the pinned version.

**Exit:** a no-change PR pipeline on a 50-module repo finishes with no JVM started and under 30 s wall-clock, including checkout.

## Phase 4 — Release management

- `scpm change`, `scpm change status`, and `scpm change check` with **validation of the declared bump**: MiMa for every Scala and Java module, plus tasty-mima for Scala 3.
- `scpm version -r`: bumps propagated through `workspace:`, changelogs, append-only ledger.
- Lanes using Maven-ordering-safe pre-release suffixes, fixed groups and epics.
- `scpm publish -r`:
  - rewrites `workspace:`, `catalog:` and ranges to concrete versions
  - signs with PGP (required by Central), optionally adding Sigstore
  - targets Central Portal, GitHub Packages and Artifactory, with an atomic multi-module publish
- `info.versionScheme` written to POMs automatically.
- First-party update bot: `scpm upgrade` and the `scpm-upgrade` Action. It groups updates by catalog entry, applies Scalafix migrations, labels semver risk with MiMa, and bypasses the release-age gate for security fixes.

**Exit:** a multi-module library publishes to Central from a GitHub Action using only scpm.

## Phase 5 — Ecosystem and polish

- Provider-plugin JSON-RPC protocol (generators, facts, extra task kinds) and reporter plugins over NDJSON.
- Config dependencies: presets and catalogs published as artifacts and pinned in the boot section of the lock.
- scala-steward `BuildToolAlg` contribution for scpm.
- Supply chain:
  - signature trust policy
  - a `paranoid` switch that turns on the strict settings together
  - SBOM output
- Native IntelliJ ExternalSystem plugin. It syncs from `scpm config --resolved --json` and watches the YAML, so there is no manual refresh.
- Kotlin support in the worker (maybe).
- Windows as tier 1 (decide in Phase 0).

## Explicitly later or never

- In-process plugins.
- sbt-style scoped settings.
- Our own resolution algorithm.
- Multi-language beyond the JVM, e.g. pnpm 12's Cargo/PyPI experiments.
