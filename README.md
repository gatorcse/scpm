> **Not ready for use.** scpm is an early work in progress. Please do not use it for anything yet.

# scpm

A fast, package-manager-first workspace tool for Scala. It is shaped like pnpm and aube,
uses coursier underneath, and is built for CI and agents.

Status: design exploration. Nothing is implemented yet.

- [docs/design.md](docs/design.md) covers the architecture, the config model, dependencies and lockfile, the daemon question, worktrees, reporting, and answers to the open questions.
- [docs/roadmap.md](docs/roadmap.md) lays out phased delivery, each phase with an exit criterion.

## Status of the implementation

Milestones land as a stack of PRs; see [docs/roadmap.md](docs/roadmap.md).

- **M1:** the Rust CLI, config model and module graph:
  `scpm ls`, `scpm config`, `scpm graph`, `scpm schema`, and `-F` filters.
- **M2:** dependencies: `scpm install [--frozen]` locks every variant into
  `scpm-lock.yaml` with coursier (joint resolution, ranges, catalog selections, BOMs,
  exclusions, compiler boot closures) and restores locked jars into the coursier cache with
  the Rust fetcher; `scpm why`, `scpm classpath`. Needs the native coursier CLI (`cs`) on
  `PATH` (or `SCPM_CS`); the network tests in `tests/install.rs` skip without it.
- **M3:** `scpm compile [--tests]`: variants compile in dependency order on the Zinc
  worker (`worker/`), unchanged variants are skipped by a Rust fingerprint without
  starting a JVM, and every run writes `.scpm/runs/*.ndjson`. Output is text, GitHub
  Actions annotations plus a step summary (automatic under `GITHUB_ACTIONS`), or `--json`.
  Build the worker first: `cd worker && sbt package` (or set `SCPM_WORKER_JAR`).
- **M4:** `scpm test [targets] [--suite GLOB] [-t NAME]` runs each variant's tests in a forked
  JVM (sbt test-interface: munit, ScalaTest, weaver, ZIO Test, utest, specs2, JUnit), with
  a timeout, JUnit XML in `.scpm/out/<id>/test-reports/`, failures as GitHub annotations
  and in `--json`. `scpm run <module> [--main X] -- args` runs a main class and passes its
  exit code through.
- **M5:** `scpm mcp` serves the Model Context Protocol on stdio. Its tools are `compile`,
  `test`, `diagnostics`, `ls`, `graph`, `config`, `install`, `why` and `classpath`, and
  its resources are `scpm://workspace` and `scpm://run/latest`. Results are compact: a
  green compile lists no tasks, and lists are capped with `more` counts. One compile
  worker stays warm across calls.

## Development

Tools are pinned in `mise.toml` (Rust, JDK, coursier, sbt).

```bash
mise install
cargo test
cargo run -- -C crates/scpm/tests/fixtures/basic ls
```

JSON Schemas for the config files live in `schema/`. Regenerate them after changing
`crates/scpm/src/config.rs`:

```bash
cargo run -q -- schema workspace > schema/workspace.json
cargo run -q -- schema module > schema/module.json
```
