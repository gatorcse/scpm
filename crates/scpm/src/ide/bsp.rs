//! `.bsp/scpm.json`, the BSP connection file (design §10.3, BSP spec "BSP Connection
//! Protocol"): clients such as IntelliJ and Metals find build servers by listing `.bsp/*.json`
//! at the workspace root and start the `argv` it names.
//!
//! `scpm bsp --setup` writes it with the path scpm was invoked by (a shim or symlink such as
//! Homebrew's or mise's stays a shim, so upgrades keep working). Afterwards the automatic IDE
//! refresh ([`super::refresh`]) only repairs it: when the recorded binary is gone, or to update
//! the version when it is this same binary; a different scpm binary never re-points it. As for
//! `.metals/mbt.json`, scpm only replaces a file whose content it wrote itself (a sha256 kept in
//! `.scpm/ide/bsp.json.sha256`), unless `--force` is given.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{IdeError, sha256_hex, write_if_changed};

/// The connection file, relative to the workspace root.
pub const CONNECTION_FILE: &str = ".bsp/scpm.json";
/// The BSP version scpm implements (a narrow subset of it; design §10.3).
pub const BSP_VERSION: &str = "2.1.0";

/// The sha256 of what scpm wrote, one per line: while a write is in progress it lists both the
/// old and the new content, so an interrupted write leaves either one owned.
const OWNER: &str = ".scpm/ide/bsp.json.sha256";
/// Serializes `--setup` and install refreshes across processes.
const LOCK: &str = ".scpm/ide/bsp.json.lock";

/// The connection file's content for the scpm binary `exe`.
pub fn render(exe: &Path) -> String {
    let doc = serde_json::json!({
        "name": "scpm",
        "version": env!("CARGO_PKG_VERSION"),
        "bspVersion": BSP_VERSION,
        "languages": ["scala", "java"],
        "argv": [exe.to_string_lossy(), "bsp"],
    });
    let mut text = serde_json::to_string_pretty(&doc).expect("json");
    text.push('\n');
    text
}

/// Whether `.bsp/scpm.json` exists and is exactly what scpm last wrote there.
pub fn owned(root: &Path) -> bool {
    let Ok(current) = std::fs::read(root.join(CONNECTION_FILE)) else { return false };
    let hash = sha256_hex(&current);
    std::fs::read_to_string(root.join(OWNER)).is_ok_and(|h| h.lines().any(|l| l.trim() == hash))
}

/// Holds the connection file's lock (waiting up to 30 s for another scpm process).
fn lock_connection(root: &Path) -> Result<std::fs::File, IdeError> {
    let path = root.join(LOCK);
    let err = |source: std::io::Error| IdeError::Write { path: path.clone(), source };
    std::fs::create_dir_all(path.parent().expect("has a parent")).map_err(err)?;
    let file =
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(err)?;
    let started = std::time::Instant::now();
    while !crate::fetch::try_lock(&file).map_err(err)? {
        if started.elapsed() > std::time::Duration::from_secs(30) {
            return Err(IdeError::Other(format!("{} is locked by another scpm process", path.display())));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(file)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Setup {
    /// The file, relative to the workspace root.
    pub path: String,
    /// False when it already had this content.
    pub written: bool,
    pub argv: Vec<String>,
}

/// The path scpm was invoked by, made absolute but not resolved: `argv[0]` if it names a path,
/// else where `PATH` finds it, else the running executable.
pub fn invoked_exe() -> Result<PathBuf, IdeError> {
    let argv0 = std::env::args_os().next().map(PathBuf::from);
    if let Some(argv0) = argv0 {
        if argv0.components().count() > 1 {
            let cwd = std::env::current_dir().map_err(|e| IdeError::Other(e.to_string()))?;
            let abs = cwd.join(&argv0);
            if abs.is_file() {
                return Ok(abs);
            }
        } else {
            // The shell ran the first executable match on PATH; take the entry that is this very
            // binary (skipping non-executable or relative entries, and other installs).
            // Relative entries (`.`, `bin`, an empty one) are relative to the current directory:
            // made absolute without resolving symlinks.
            let current = std::env::current_exe().ok();
            let cwd = std::env::current_dir().ok();
            let found = std::env::var_os("PATH").and_then(|p| {
                std::env::split_paths(&p)
                    .filter_map(|d| if d.is_absolute() { Some(d) } else { cwd.as_ref().map(|c| c.join(d)) })
                    .map(|d| d.join(&argv0))
                    .find(|c| is_executable(c) && current.as_ref().is_none_or(|exe| same_binary(c, exe)))
            });
            if let Some(found) = found {
                return Ok(found);
            }
        }
    }
    std::env::current_exe().map_err(|e| IdeError::Other(format!("cannot locate the scpm binary: {e}")))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Whether two paths are the same file once symlinks are resolved.
fn same_binary(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The binary an existing connection file starts.
fn recorded_exe(root: &Path) -> Option<PathBuf> {
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join(CONNECTION_FILE)).ok()?).ok()?;
    doc["argv"][0].as_str().map(PathBuf::from)
}

/// Writes the connection file for `exe` if it is missing, scpm owns it, or `force`.
pub fn setup(root: &Path, exe: &Path, force: bool) -> Result<Setup, IdeError> {
    let _lock = lock_connection(root)?;
    setup_locked(root, exe, force)
}

fn setup_locked(root: &Path, exe: &Path, force: bool) -> Result<Setup, IdeError> {
    let path = root.join(CONNECTION_FILE);
    let text = render(exe);
    let unchanged = std::fs::read(&path).is_ok_and(|c| c == text.as_bytes());
    // Present unless definitely absent: a dangling symlink is someone's file too.
    let present = match std::fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    };
    if present && !unchanged && !owned(root) && !force {
        return Err(IdeError::Other(format!(
            "{CONNECTION_FILE} exists and was not written by scpm; pass --force to replace it"
        )));
    }
    let new_hash = sha256_hex(text.as_bytes());
    // Own both versions while writing, so a failure at any step leaves the file owned.
    let old_hash = std::fs::read(&path).ok().filter(|_| owned(root)).map(|c| sha256_hex(&c));
    if let Some(old) = old_hash.filter(|o| *o != new_hash) {
        write_if_changed(&root.join(OWNER), &format!("{old}\n{new_hash}\n"))?;
    } else {
        write_if_changed(&root.join(OWNER), &format!("{new_hash}\n"))?;
    }
    let written = write_if_changed(&path, &text)?;
    write_if_changed(&root.join(OWNER), &format!("{new_hash}\n"))?;
    Ok(Setup {
        path: CONNECTION_FILE.to_string(),
        written,
        argv: vec![exe.to_string_lossy().into_owned(), "bsp".into()],
    })
}

/// The install refresh for an existing, scpm-owned connection file: if the binary it records
/// is gone, it is re-pointed at the scpm running now; if it is this same binary (maybe through a
/// shim), only the version is brought up to date; another scpm binary leaves it alone. Returns
/// whether it was written.
pub fn refresh(root: &Path) -> Result<bool, IdeError> {
    let current = std::env::current_exe().map_err(|e| IdeError::Other(e.to_string()))?;
    refresh_for(root, &current, invoked_exe)
}

fn refresh_for(
    root: &Path,
    current: &Path,
    invoked: impl FnOnce() -> Result<PathBuf, IdeError>,
) -> Result<bool, IdeError> {
    // Cheap check first (the refresh runs after every install), then again under the lock, held
    // through the write so a concurrent `--setup` is never overwritten.
    if !owned(root) {
        return Ok(false);
    }
    let _lock = lock_connection(root)?;
    if !owned(root) {
        return Ok(false);
    }
    let exe = match recorded_exe(root) {
        Some(recorded) if recorded.exists() => {
            if !same_binary(&recorded, current) {
                return Ok(false);
            }
            recorded
        }
        _ => invoked()?,
    };
    Ok(setup_locked(root, &exe, false)?.written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_not_replaced_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".bsp")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere.json"), root.join(CONNECTION_FILE)).unwrap();
        assert!(setup(root, Path::new("/opt/a/scpm"), false).unwrap_err().to_string().contains("--force"));
        assert!(setup(root, Path::new("/opt/a/scpm"), true).is_ok());
    }

    #[test]
    fn an_interrupted_write_stays_owned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        setup(root, Path::new("/opt/a/scpm"), false).unwrap();
        let old = std::fs::read(root.join(CONNECTION_FILE)).unwrap();
        let new = render(Path::new("/opt/b/scpm"));
        // The marker lists both while a write is in progress: either content is still owned.
        std::fs::write(root.join(OWNER), format!("{}\n{}\n", sha256_hex(&old), sha256_hex(new.as_bytes()))).unwrap();
        assert!(owned(root));
        std::fs::write(root.join(CONNECTION_FILE), &new).unwrap();
        assert!(owned(root));
        assert!(!setup(root, Path::new("/opt/b/scpm"), false).unwrap().written);
        assert_eq!(std::fs::read_to_string(root.join(OWNER)).unwrap(), format!("{}\n", sha256_hex(new.as_bytes())));
    }

    #[test]
    fn writes_only_files_scpm_owns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = Path::new("/opt/a/scpm");
        let s = setup(root, a, false).unwrap();
        assert!(s.written);
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(CONNECTION_FILE)).unwrap()).unwrap();
        assert_eq!(doc["argv"], serde_json::json!(["/opt/a/scpm", "bsp"]));
        assert_eq!(doc["bspVersion"], BSP_VERSION);
        assert_eq!(doc["languages"], serde_json::json!(["scala", "java"]));
        assert!(owned(root));
        // Owned: a new binary path replaces it; the same content is not rewritten.
        assert!(setup(root, Path::new("/opt/b/scpm"), false).unwrap().written);
        assert!(!setup(root, Path::new("/opt/b/scpm"), false).unwrap().written);
        // Someone else's file is left alone unless forced.
        std::fs::write(root.join(CONNECTION_FILE), "{\"name\": \"mine\"}\n").unwrap();
        assert!(!owned(root));
        assert!(!refresh_for(root, a, || unreachable!()).unwrap());
        assert!(setup(root, a, false).unwrap_err().to_string().contains("--force"));
        assert!(setup(root, a, true).unwrap().written);
        assert!(owned(root));
    }

    #[test]
    fn refresh_repairs_but_never_steals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bin = |name: &str| {
            let p = root.join(name);
            std::fs::write(&p, "").unwrap();
            p
        };
        let (mine, other) = (bin("scpm"), bin("scpm2"));
        let argv0 = || recorded_exe(root).unwrap();
        // Recorded binary exists and is another one: left alone.
        setup(root, &mine, false).unwrap();
        assert!(!refresh_for(root, &other, || Ok(other.clone())).unwrap());
        assert_eq!(argv0(), mine);
        // The same binary through a symlink (a shim): the shim path stays.
        #[cfg(unix)]
        {
            let shim = root.join("shim");
            std::os::unix::fs::symlink(&mine, &shim).unwrap();
            setup(root, &shim, false).unwrap();
            assert!(!refresh_for(root, &mine, || unreachable!()).unwrap());
            assert_eq!(argv0(), shim);
        }
        // The recorded binary is gone (an upgrade removed it): re-pointed at the running one.
        setup(root, Path::new("/nonexistent/scpm"), false).unwrap();
        assert!(refresh_for(root, &other, || Ok(other.clone())).unwrap());
        assert_eq!(argv0(), other);
    }
}
