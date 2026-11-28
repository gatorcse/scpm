//! scpm: a fast, package-manager-first workspace tool for Scala.
//!
//! See `docs/design.md` in the repository for the design this crate implements.

// Diagnostics are deliberately rich values; they are only built on error paths.
#![allow(clippy::result_large_err)]

pub mod cli;
pub mod compile;
pub mod config;
pub mod coord;
pub mod coursier;
pub mod credentials;
pub mod diag;
pub mod explain;
pub mod fetch;
pub mod filter;
pub mod foreign;
pub mod graph;
pub mod install;
pub mod lockfile;
pub mod machine;
pub mod mcp;
pub mod merge;
pub mod platform;
pub mod report;
pub mod selector;
pub mod semver;
pub mod testing;
pub mod version;
pub mod why;
pub mod worker;
pub mod workspace;
