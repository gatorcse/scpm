//! scpm: a fast, package-manager-first workspace tool for Scala.
//!
//! See `docs/design.md` in the repository for the design this crate implements.

// Diagnostics are deliberately rich values; they are only built on error paths.
#![allow(clippy::result_large_err)]

pub mod cli;
pub mod config;
pub mod coord;
pub mod diag;
pub mod filter;
pub mod graph;
pub mod merge;
pub mod platform;
pub mod selector;
pub mod version;
pub mod workspace;
