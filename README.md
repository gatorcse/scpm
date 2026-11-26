> **Not ready for use.** scpm is an early work in progress. Please do not use it for anything yet.

# scpm

A fast, package-manager-first workspace tool for Scala. It is shaped like pnpm and aube,
uses coursier underneath, and is built for CI and agents.

Status: design exploration. Nothing is implemented yet.

- [docs/design.md](docs/design.md) covers the architecture, the config model, dependencies and lockfile, the daemon question, worktrees, reporting, and answers to the open questions.
- [docs/roadmap.md](docs/roadmap.md) lays out phased delivery, each phase with an exit criterion.

## Status of the implementation

Milestones land as a stack of PRs; see [docs/roadmap.md](docs/roadmap.md).

- **M1 (this PR stack's base):** the Rust CLI, config model and module graph:
  `scpm ls`, `scpm config`, `scpm graph`, `scpm schema`, and `-F` filters.

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
