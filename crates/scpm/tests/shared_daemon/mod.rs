//! One warm compile daemon shared by the integration tests that don't test the daemon or the
//! worker itself, so they don't each start a cold worker JVM.
//!
//! Sharing works across processes (nextest runs every test in its own process): every test
//! points `SCPM_DAEMON_DIR` at the same short directory per checkout, and the daemon's own
//! start lock makes the first client start it and the others connect (see
//! `parallel_cold_starts_share_one_daemon`). Nothing stops it explicitly: it exits after
//! `IDLE_SECS` without compiles. A daemon left by an earlier run is reused if it serves the
//! same scpm binary, worker jar and JDK (the daemon key); after a rebuild the new binary gets a
//! new key and the old daemon idles out.
//!
//! `SCPM_TEST_SHARED_DAEMON=0` turns sharing off: every test then starts its own worker.

#![allow(dead_code)]

use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// How long the shared daemon outlives the last compile.
const IDLE_SECS: &str = "120";

/// Whether tests use the shared daemon.
pub fn enabled() -> bool {
    scpm::daemon::SUPPORTED && std::env::var_os("SCPM_TEST_SHARED_DAEMON").is_none_or(|v| v != "0")
}

/// The shared daemon directory: short enough for socket paths wherever the checkout is, and
/// one per checkout (its `CARGO_TARGET_TMPDIR`), so worktrees don't stop each other's.
pub fn dir() -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    env!("CARGO_TARGET_TMPDIR").hash(&mut h);
    Path::new("/tmp").join(format!("scpm-it-{:08x}", h.finish() as u32))
}

/// Environment for a command that should use the shared daemon (or no daemon, when sharing is
/// off). Apply after the defaults: it overrides `SCPM_NO_DAEMON`.
pub fn env() -> Vec<(&'static str, OsString)> {
    if !enabled() {
        return vec![("SCPM_NO_DAEMON", "1".into())];
    }
    let dir = dir().into_os_string();
    let mut env = vec![
        ("SCPM_NO_DAEMON", "0".into()),
        ("SCPM_DAEMON_DIR", dir.clone()),
        // Never the user's own `/tmp/scpm-<uid>`.
        ("SCPM_DAEMON_FALLBACK_DIR", dir),
        ("SCPM_DAEMON_IDLE_SECS", IDLE_SECS.into()),
    ];
    // The worker jar's path is part of the daemon key (and of compile fingerprints): spell it
    // the same way in every test (an `scpm` that looks for the jar itself may spell it
    // differently and get a second daemon). Not canonicalized: sbt links it into its cache.
    if let Ok(jar) = scpm::worker::helper_jar("SCPM_WORKER_JAR", "scpm-compile-worker") {
        env.push(("SCPM_WORKER_JAR", jar.into_os_string()));
    }
    env
}
