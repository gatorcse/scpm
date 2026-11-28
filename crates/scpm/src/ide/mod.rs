//! IDE integration (design §10): Metals v2's `.metals/mbt.json` ([`metals`]) and native
//! IntelliJ project files ([`idea`]), both generated from the workspace and the lockfile.
//!
//! Neither generator resolves anything: classpaths are the lock's, and jars resolve into the
//! coursier cache exactly as for `scpm compile`. Sources jars are not in the lock (design §5.4):
//! the explicit `scpm ide …` commands fetch missing ones with `cs fetch --sources`, which
//! verifies them against the repository's checksums. Nothing about them is recorded except a
//! local list of modules that publish none (`.scpm/ide/no-sources`), tagged with a fingerprint
//! of the repositories and credentials it was made with. The automatic refresh after installs
//! ([`refresh`]) only uses sources jars that are already cached, so it never starts `cs`.

pub mod idea;
pub mod metals;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::compile::{CompileError, Toolchain};
use crate::coursier;
use crate::credentials::CoursierCredentials;
use crate::install::InstallError;
use crate::lockfile::Lockfile;
use crate::workspace::Workspace;

#[derive(Debug, thiserror::Error)]
pub enum IdeError {
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error("cannot write {}: {source}", .path.display())]
    Write { path: PathBuf, source: std::io::Error },
    #[error("{0}")]
    Other(String),
}

/// Writes `content` to `path` (creating parent directories) unless the file already has exactly
/// that content. The write is atomic: a temporary file in the same directory renamed into place.
/// A replaced file keeps its permissions; a new one gets `0644` (temporary files are created
/// `0600`). Returns whether the file was written.
pub fn write_if_changed(path: &Path, content: &str) -> Result<bool, IdeError> {
    if fs::read(path).is_ok_and(|old| old == content.as_bytes()) {
        return Ok(false);
    }
    let err = |source: std::io::Error| IdeError::Write { path: path.to_path_buf(), source };
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(err)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut tmp = tempfile::Builder::new().prefix(&format!(".{name}.")).suffix(".tmp").tempfile_in(dir).map_err(err)?;
    tmp.write_all(content.as_bytes()).map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::metadata(path).map(|m| m.permissions()).unwrap_or_else(|_| fs::Permissions::from_mode(0o644));
        tmp.as_file().set_permissions(perms).map_err(err)?;
    }
    tmp.persist(path).map_err(|e| err(e.error))?;
    Ok(true)
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    crate::fetch::hex(&Sha256::digest(bytes))
}

/// A `file:` URI for an absolute path (`file:///a/b%20c.jar`), percent-encoding everything but
/// unreserved characters and `/`. Coursier's cache paths can contain `%` (escaped `:` in
/// `host%3Aport`), which must itself be encoded.
pub fn file_uri(path: &Path) -> String {
    // A relative path (e.g. under `COURSIER_CACHE=./cache`) is resolved against the current
    // directory first; encoded as is, `./cache` would read as `/./cache`.
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    // Only Windows uses `\\` as a separator; on Unix it is an ordinary (percent-encoded) byte.
    let text =
        if cfg!(windows) { path.to_string_lossy().replace('\\', "/") } else { path.to_string_lossy().into_owned() };
    let mut out = String::from("file://");
    let drive = !text.starts_with('/');
    if drive {
        out.push('/'); // Windows: `file:///C:/…`
    }
    for (i, b) in text.bytes().enumerate() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) || (drive && i == 1 && b == b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether a locked artifact is a jar (lock keys carry `@<type>` only for other file types,
/// such as `@aar` or `@exe`, which don't belong on an IDE classpath).
pub fn is_jar(key: &str) -> bool {
    !key.contains('@')
}

/// The Maven module a locked artifact belongs to, for finding its sources jar: `org:name:version`
/// without classifier. `None` for non-jar artifacts (`…@aar`, `…@exe`), which have no sources.
pub fn sources_module(key: &str) -> Option<String> {
    if !is_jar(key) {
        return None;
    }
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        [g, a, v] | [g, a, v, _] => Some(format!("{g}:{a}:{v}")),
        _ => None,
    }
}

/// How sources jars were found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcesReport {
    /// Already in the coursier cache.
    pub cached: usize,
    /// Downloaded now.
    pub fetched: usize,
    /// The module publishes no sources jar (or fetching was off or failed).
    pub missing: usize,
}

/// How to get sources jars that aren't cached yet.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SourcesOptions {
    /// Download them (`scpm ide …`); otherwise only cached ones are used (the install refresh).
    pub fetch: bool,
    /// Ask again for modules recorded as having no sources jar (`--refresh-sources`).
    pub refresh: bool,
}

const NO_SOURCES: &str = ".scpm/ide/no-sources";

/// A fingerprint of where sources come from: each repository's id and URL, and the user it
/// authenticates as (configured or from coursier's credentials), never a secret. A 404 seen
/// with one set of repositories or credentials says nothing about another.
fn sources_fingerprint(ws: &Workspace) -> String {
    let creds = CoursierCredentials::load();
    let mut text = String::new();
    // The repositories as fetched: mirrors and machine credentials applied (design §5.9).
    let repos = crate::install::repositories(ws).map(|(repos, _)| repos).unwrap_or_default();
    for repo in repos {
        let user = match repo.auth() {
            Ok(Some(auth)) => auth.user().to_string(),
            Ok(None) => creds.find(&repo.url).map(|c| c.user.clone()).unwrap_or_default(),
            Err(_) => "<unset>".to_string(),
        };
        text.push_str(&format!("{} {} {user}\n", repo.id, repo.fetch_url()));
    }
    sha256_hex(text.as_bytes())[..16].to_string()
}

/// Sources jars for the artifacts `keys` (lock keys), by [`sources_module`]: jars already in the
/// cache next to the artifact, plus (with `opts.fetch`) whatever one `cs` call downloads.
/// Failures to fetch are warnings, never errors: sources are optional.
pub fn sources_jars(
    tc: &Toolchain,
    keys: &BTreeSet<String>,
    opts: SourcesOptions,
    warnings: &mut Vec<String>,
) -> (BTreeMap<String, PathBuf>, SourcesReport) {
    let jars: BTreeMap<String, PathBuf> = keys.iter().filter(|k| is_jar(k)).map(|k| (k.clone(), tc.file(k))).collect();
    let fetch = |modules: &[String]| -> Result<BTreeMap<String, PathBuf>, String> {
        let (repos, machine) = crate::install::repositories(tc.ws).map_err(|e| e.to_string())?;
        let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
        coursier::fetch_sources(modules, &repos, &machine.proxy, &coursier::cache_dir(), scratch.path())
            .map_err(|e| e.to_string())
    };
    let fingerprint = if opts.fetch { sources_fingerprint(tc.ws) } else { String::new() };
    find_sources(&tc.ws.root, &jars, &fingerprint, opts, fetch, warnings)
}

/// [`sources_jars`] without the toolchain: `jars` maps lock keys to cached jar paths, and
/// `fetch` downloads sources for `org:name:version` modules.
fn find_sources(
    root: &Path,
    jars: &BTreeMap<String, PathBuf>,
    fingerprint: &str,
    opts: SourcesOptions,
    fetch: impl FnOnce(&[String]) -> Result<BTreeMap<String, PathBuf>, String>,
    warnings: &mut Vec<String>,
) -> (BTreeMap<String, PathBuf>, SourcesReport) {
    let mut report = SourcesReport::default();
    let mut found = BTreeMap::new();
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for (key, jar) in jars {
        let Some(module) = sources_module(key) else { continue };
        if found.contains_key(&module) || wanted.contains(&module) {
            continue;
        }
        let parts: Vec<&str> = module.split(':').collect();
        let candidate = jar.with_file_name(format!("{}-{}-sources.jar", parts[1], parts[2]));
        if candidate.is_file() {
            report.cached += 1;
            found.insert(module, candidate);
        } else {
            wanted.insert(module);
        }
    }
    if opts.fetch && !wanted.is_empty() {
        let file = root.join(NO_SOURCES);
        let header = format!("# repositories {fingerprint}");
        // The list only holds for the repositories and credentials it was made with.
        let mut known_missing: BTreeSet<String> = match fs::read_to_string(&file) {
            Ok(text) if !opts.refresh && text.lines().next() == Some(header.as_str()) => {
                text.lines().skip(1).map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
            }
            _ => BTreeSet::new(),
        };
        let to_fetch: Vec<String> = wanted.iter().filter(|m| !known_missing.contains(*m)).cloned().collect();
        if !to_fetch.is_empty() {
            match fetch(&to_fetch) {
                Ok(fetched) => {
                    report.fetched += fetched.len();
                    known_missing.extend(to_fetch.iter().filter(|m| !fetched.contains_key(*m)).cloned());
                    found.extend(fetched);
                }
                // Nothing is recorded: the next `scpm ide …` asks again.
                Err(e) => {
                    warnings.push(format!("could not fetch sources jars (navigation into libraries is limited): {e}"))
                }
            }
        }
        let text: String = std::iter::once(header).chain(known_missing).map(|l| format!("{l}\n")).collect();
        if let Err(e) = write_if_changed(&file, &text) {
            warnings.push(e.to_string());
        }
    }
    report.missing = wanted.iter().filter(|m| !found.contains_key(*m)).count();
    (found, report)
}

/// What [`refresh`] regenerated.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Refreshed {
    /// Files written (relative to the workspace root).
    pub written: Vec<String>,
    pub warnings: Vec<String>,
}

/// After a non-frozen install (explicit, or before `compile`/`test`/`run`): regenerates the
/// IDE files scpm maintains, writing only files whose content changed.
///
/// - `.metals/mbt.json` when scpm wrote its current content (or the workspace sets
///   `ide.metals: true`), unless a Metals importer script (`*.mbt.sh`, …) owns it.
/// - The IntelliJ files when `scpm idea` ran before (or the workspace sets `ide.idea: true`).
///
/// Only cached sources jars are used, so no `cs` process starts; failures become warnings and
/// never fail the command. With neither file in play, it returns without reading anything else.
pub fn refresh(ws: &Workspace, lock: &Lockfile) -> Refreshed {
    let mut out = Refreshed::default();
    let ide = ws.config.ide.clone().unwrap_or_default();
    let mut metals = ide.metals.unwrap_or(false) || metals::owned(&ws.root);
    if metals && let Some(script) = metals::importer_script(&ws.root) {
        metals = false;
        if ide.metals == Some(true) {
            out.warnings.push(format!(
                "not updating {}: Metals runs the importer `{script}`, which writes it; have the script run `scpm ide metals --output \"$MBT_OUTPUT_FILE\"`",
                metals::MBT_JSON
            ));
        }
    }
    let idea = ide.idea.unwrap_or(false) || idea::generated_before(&ws.root);
    if !metals && !idea {
        return out;
    }
    let tc = match Toolchain::new(ws, lock) {
        Ok(tc) => tc,
        Err(e) => {
            out.warnings.push(format!("could not update IDE files: {e}"));
            return out;
        }
    };
    let sources = SourcesOptions { fetch: false, refresh: false };
    if metals {
        match metals::generate(&tc, &metals::Options { output: None, sources }) {
            Ok(s) => {
                out.warnings.extend(s.warnings);
                if s.written {
                    out.written.push(s.path);
                }
            }
            Err(e) => out.warnings.push(format!("could not update {}: {e}", metals::MBT_JSON)),
        }
    }
    if idea {
        match idea::generate(&tc, &idea::Options { sources, force: false }) {
            Ok(s) => {
                if !s.written.is_empty() {
                    out.warnings.extend(s.skipped_warning());
                }
                out.warnings.extend(s.warnings);
                out.written.extend(s.written);
            }
            Err(e) => out.warnings.push(format!("could not update the IntelliJ project files: {e}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn file_uris_are_encoded() {
        assert_eq!(file_uri(Path::new("/a/b c/x%3A1.jar")), "file:///a/b%20c/x%253A1.jar");
        assert_eq!(file_uri(Path::new("/c/~user/a-b_c.jar")), "file:///c/~user/a-b_c.jar");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(file_uri(Path::new("cache/x.jar")), file_uri(&cwd.join("cache/x.jar")));
    }

    #[test]
    fn sources_modules() {
        assert_eq!(sources_module("org:a_3:1.0").as_deref(), Some("org:a_3:1.0"));
        assert_eq!(sources_module("io.netty:epoll:4.1:linux-x86_64").as_deref(), Some("io.netty:epoll:4.1"));
        assert_eq!(sources_module("com.x:lib:1.0@aar"), None);
    }

    #[test]
    fn unchanged_files_are_not_rewritten_and_new_ones_are_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b.json");
        assert!(write_if_changed(&path, "x").unwrap());
        assert!(!write_if_changed(&path, "x").unwrap());
        assert!(write_if_changed(&path, "y").unwrap());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "no temporary files left");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
            write_if_changed(&path, "z").unwrap();
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o664, "kept on replace");
        }
    }

    /// A workspace with two cached jars, `org:a:1` (sources cached too) and `org:b:1`.
    fn jars(dir: &Path) -> BTreeMap<String, PathBuf> {
        fs::create_dir_all(dir.join("cache")).unwrap();
        for f in ["a-1.jar", "a-1-sources.jar", "b-1.jar"] {
            fs::write(dir.join("cache").join(f), "").unwrap();
        }
        [("org:a:1", "a-1.jar"), ("org:b:1", "b-1.jar"), ("org:c:1@aar", "c-1.aar")]
            .into_iter()
            .map(|(k, f)| (k.to_string(), dir.join("cache").join(f)))
            .collect()
    }

    #[test]
    fn missing_sources_are_remembered_per_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let jars = jars(root);
        let calls = Cell::new(0);
        let none = |m: &[String]| {
            assert_eq!(m, ["org:b:1"], "only uncached jar modules are asked for");
            calls.set(calls.get() + 1);
            Ok(BTreeMap::new())
        };
        let fetch = SourcesOptions { fetch: true, refresh: false };
        let mut w = Vec::new();
        let (found, report) = find_sources(root, &jars, "fp1", fetch, none, &mut w);
        assert_eq!((found.len(), report), (1, SourcesReport { cached: 1, fetched: 0, missing: 1 }));
        assert_eq!(fs::read_to_string(root.join(NO_SOURCES)).unwrap(), "# repositories fp1\norg:b:1\n");
        // Known missing: not asked again, also not by a cached-only run.
        find_sources(root, &jars, "fp1", fetch, none, &mut w);
        find_sources(root, &jars, "", SourcesOptions::default(), none, &mut w);
        assert_eq!(calls.get(), 1);
        // Other repositories or credentials, or --refresh-sources: asked again.
        find_sources(root, &jars, "fp2", fetch, none, &mut w);
        assert_eq!(calls.get(), 2);
        find_sources(root, &jars, "fp2", SourcesOptions { fetch: true, refresh: true }, none, &mut w);
        assert_eq!(calls.get(), 3);
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn failed_fetches_warn_and_record_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let jars = jars(root);
        let fetch = SourcesOptions { fetch: true, refresh: false };
        let mut w = Vec::new();
        let (_, report) = find_sources(root, &jars, "fp", fetch, |_| Err("HTTP 429".into()), &mut w);
        assert_eq!(report.missing, 1);
        assert!(w[0].contains("could not fetch sources jars") && w[0].contains("HTTP 429"), "{w:?}");
        assert_eq!(fs::read_to_string(root.join(NO_SOURCES)).unwrap(), "# repositories fp\n");
        let fetched = root.join("cache/b-1-sources.jar");
        let (found, report) =
            find_sources(root, &jars, "fp", fetch, |m| Ok([(m[0].clone(), fetched.clone())].into()), &mut Vec::new());
        assert_eq!((report.fetched, found.get("org:b:1")), (1, Some(&fetched)), "retried next time");
    }
}
