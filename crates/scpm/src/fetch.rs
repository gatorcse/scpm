//! Restoring locked artifacts into the coursier cache without coursier (design §5.2).
//!
//! The lock already knows every URL and sha256, so there is no resolution: missing jars are
//! downloaded in one concurrent burst (HTTP/2, no POMs). Files are written the way coursier
//! writes them: a `.<name>.part` temporary renamed into place while holding the per-file
//! `<file>.lock` that coursier's JVM side also takes (`FileChannel.tryLock`, which is a POSIX
//! `fcntl` record lock on Unix), so `cs` and `scpm` can share one cache concurrently. No
//! checksum sidecars are written: coursier fetches `.sha1` files on demand when it needs them.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::coursier::{Auth, user_agent};

/// One file to make present in the cache.
#[derive(Debug, Clone)]
pub struct FetchItem {
    pub key: String,
    pub url: String,
    pub dest: PathBuf,
    /// Lowercase hex sha256 the file must have.
    pub sha256: String,
    pub size: u64,
    /// Credentials for the URL's repository; an error (e.g. an unset environment variable) is
    /// only reported if the file actually has to be downloaded.
    pub auth: Result<Option<Auth>, String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FetchOptions {
    /// Re-hash files already in the cache against the lock (`scpm install --verify`) and
    /// re-download those that don't match.
    pub verify: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchReport {
    pub present: usize,
    pub downloaded: usize,
    pub bytes: u64,
    /// Cached files re-hashed with `--verify`.
    pub verified: usize,
    /// Cached files that failed `--verify` and were downloaded again.
    pub repaired: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{key}: {url} returned HTTP {status}{hint}")]
    Http { key: String, url: String, status: u16, hint: &'static str, retry_after: Option<Duration> },
    #[error("{key}: download from {url} failed: {message}")]
    Transport { key: String, url: String, host: String, message: String },
    #[error("{key}: {url} sent {actual} bytes, but the lock says {expected}")]
    Size { key: String, url: String, expected: u64, actual: u64 },
    #[error("{key}: checksum mismatch for {url}: expected sha256 {expected}, got {actual}")]
    Checksum { key: String, url: String, expected: String, actual: String },
    #[error("{key}: cannot write {path}: {source}")]
    Io { key: String, path: PathBuf, source: io::Error },
    #[error("{key}: timed out waiting for another process to finish downloading {path}")]
    LockTimeout { key: String, path: PathBuf },
    #[error("{key}: {message}")]
    Credentials { key: String, message: String },
}

/// Every failed download, plus how many downloads were attempted per host.
#[derive(Debug)]
pub struct FetchFailure {
    pub errors: Vec<FetchError>,
    pub attempted: BTreeMap<String, usize>,
}

impl fmt::Display for FetchFailure {
    /// One line per problem: transport errors are collapsed per host (with a hint when every
    /// request to the host failed before an HTTP response), and credential errors per message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines: Vec<String> = Vec::new();
        let mut transport: BTreeMap<&str, Vec<&FetchError>> = BTreeMap::new();
        let mut credentials: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &self.errors {
            match e {
                FetchError::Transport { host, .. } => transport.entry(host).or_default().push(e),
                FetchError::Credentials { key, message } => credentials.entry(message).or_default().push(key),
                other => lines.push(other.to_string()),
            }
        }
        for (host, errors) in transport {
            let mut line = if errors.len() == 1 {
                errors[0].to_string()
            } else {
                let FetchError::Transport { key, url, message, .. } = errors[0] else { unreachable!() };
                format!("{host}: {} downloads failed (first: {key} from {url}): {message}", errors.len())
            };
            if self.attempted.get(host).is_some_and(|n| *n == errors.len()) {
                line.push_str(&format!(
                    "\n  help: every request to {host} failed before an HTTP response; are you offline or behind a proxy?"
                ));
            }
            lines.push(line);
        }
        for (message, keys) in credentials {
            lines.push(if keys.len() == 1 {
                format!("{}: {message}", keys[0])
            } else {
                format!("{message} (needed to download {} artifacts, e.g. {})", keys.len(), keys[0])
            });
        }
        f.write_str(&lines.join("\n"))
    }
}

const CONCURRENCY: usize = 32;
const ATTEMPTS: u32 = 4;
const LOCK_WAIT: Duration = Duration::from_secs(120);
/// The longest `Retry-After` honoured on 429/503.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

fn host_of(url: &str) -> String {
    url.split_once("://").map(|(_, rest)| rest.split('/').next().unwrap_or(rest)).unwrap_or(url).to_string()
}

/// Downloads every missing item. Present files are trusted if their size matches the lock
/// (artifacts are immutable); with `verify` they are re-hashed and re-downloaded on mismatch.
pub fn fetch_all(items: Vec<FetchItem>, opts: FetchOptions) -> Result<FetchReport, FetchFailure> {
    let mut report = FetchReport::default();
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for item in items {
        if fs::metadata(&item.dest).is_ok_and(|m| m.len() == item.size) {
            present.push(item);
        } else {
            missing.push((item, false));
        }
    }
    if opts.verify {
        report.verified = present.len();
        let hashes = hash_files(present.iter().map(|i| i.dest.as_path()).collect());
        for (item, hash) in present.into_iter().zip(hashes) {
            if hash.as_deref().is_ok_and(|h| h == item.sha256) {
                report.present += 1;
            } else {
                missing.push((item, true));
            }
        }
    } else {
        report.present = present.len();
    }
    if missing.is_empty() {
        return Ok(report);
    }
    let mut attempted: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors = Vec::new();
    let mut downloads = Vec::new();
    for (item, repair) in missing {
        match &item.auth {
            Err(message) => errors.push(FetchError::Credentials { key: item.key.clone(), message: message.clone() }),
            Ok(_) => {
                *attempted.entry(host_of(&item.url)).or_default() += 1;
                downloads.push((item, repair));
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let results = runtime.block_on(download_all(downloads));
    for (r, repair) in results {
        match r {
            Ok(Some(bytes)) => {
                report.downloaded += 1;
                report.bytes += bytes;
                if repair {
                    report.repaired += 1;
                }
            }
            Ok(None) => report.present += 1,
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() { Ok(report) } else { Err(FetchFailure { errors, attempted }) }
}

/// sha256 of each file, on a few threads.
fn hash_files(paths: Vec<&Path>) -> Vec<io::Result<String>> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let chunk = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|c| s.spawn(move || c.iter().map(|p| sha256_file(p)).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("hashing thread")).collect()
    })
}

async fn download_all(items: Vec<(FetchItem, bool)>) -> Vec<(Result<Option<u64>, FetchError>, bool)> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .expect("http client");
    let permits = Arc::new(Semaphore::new(CONCURRENCY));
    let tasks: Vec<_> = items
        .into_iter()
        .map(|(item, repair)| {
            let client = client.clone();
            let permits = permits.clone();
            tokio::spawn(async move {
                let _permit = permits.acquire().await.expect("semaphore open");
                (fetch_one(&client, &item, repair).await, repair)
            })
        })
        .collect();
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        out.push(t.await.expect("download task panicked"));
    }
    out
}

/// Returns `Some(bytes)` if this call downloaded the file, `None` if another process did.
/// `replace` downloads even if a file of the right size is present (it failed `--verify`).
async fn fetch_one(client: &reqwest::Client, item: &FetchItem, replace: bool) -> Result<Option<u64>, FetchError> {
    let io_err =
        |path: &Path, source: io::Error| FetchError::Io { key: item.key.clone(), path: path.to_path_buf(), source };
    let dir = item.dest.parent().expect("cache files have a parent");
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let lock_path = lock_path(&item.dest);
    let lock = {
        let key = item.key.clone();
        let lock_path = lock_path.clone();
        tokio::task::spawn_blocking(move || acquire(&lock_path, &key)).await.expect("lock task")?
    };
    if !replace && fs::metadata(&item.dest).is_ok_and(|m| m.len() == item.size) {
        release(lock, &lock_path);
        return Ok(None);
    }
    let auth = item.auth.as_ref().ok().cloned().flatten();
    let part = part_path(&item.dest);
    let mut last_error: Option<FetchError> = None;
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            let backoff = Duration::from_millis(250 * 2u64.pow(attempt));
            let delay = match &last_error {
                Some(FetchError::Http { retry_after: Some(after), .. }) => (*after).min(MAX_RETRY_AFTER),
                _ => backoff,
            };
            tokio::time::sleep(delay).await;
        }
        match download(client, item, auth.as_ref(), &part).await {
            Ok(bytes) => {
                let renamed = fs::rename(&part, &item.dest).map_err(|e| io_err(&item.dest, e));
                release(lock, &lock_path);
                return renamed.map(|()| Some(bytes));
            }
            Err(e) if retryable(&e) => last_error = Some(e),
            Err(e) => {
                let _ = fs::remove_file(&part);
                release(lock, &lock_path);
                return Err(e);
            }
        }
    }
    let _ = fs::remove_file(&part);
    release(lock, &lock_path);
    Err(last_error.expect("at least one attempt"))
}

fn retryable(e: &FetchError) -> bool {
    match e {
        FetchError::Http { status, .. } => *status == 429 || *status >= 500,
        FetchError::Transport { .. } | FetchError::Size { .. } => true,
        _ => false,
    }
}

/// An error's message followed by its sources: `error sending request: … : Connection refused`.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = s.source();
    }
    message
}

/// `Retry-After` in seconds (the HTTP-date form is not used by Maven repositories).
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let value = resp.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

async fn download(
    client: &reqwest::Client,
    item: &FetchItem,
    auth: Option<&Auth>,
    part: &Path,
) -> Result<u64, FetchError> {
    let transport = |e: reqwest::Error| FetchError::Transport {
        key: item.key.clone(),
        url: item.url.clone(),
        host: host_of(&item.url),
        message: error_chain(&e),
    };
    let mut req = client.get(&item.url);
    match auth {
        Some(Auth::Bearer(token)) => req = req.bearer_auth(token),
        Some(Auth::Basic { user, password }) => req = req.basic_auth(user, Some(password)),
        None => {}
    }
    let resp = req.send().await.map_err(transport)?;
    let status = resp.status().as_u16();
    if status != 200 {
        let hint = match status {
            401 | 403 => " (check the repository's credentials)",
            404 => " (the artifact is missing from the repository; was the lock made against another repository?)",
            _ => "",
        };
        let retry_after = if matches!(status, 429 | 503) { retry_after(&resp) } else { None };
        return Err(FetchError::Http { key: item.key.clone(), url: item.url.clone(), status, hint, retry_after });
    }
    let io_err = |source: io::Error| FetchError::Io { key: item.key.clone(), path: part.to_path_buf(), source };
    let mut file = tokio::fs::File::create(part).await.map_err(io_err)?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        hasher.update(&chunk);
        bytes += chunk.len() as u64;
        file.write_all(&chunk).await.map_err(io_err)?;
    }
    file.sync_all().await.map_err(io_err)?;
    drop(file);
    if bytes != item.size {
        // A truncated (or padded) body: worth another attempt.
        let _ = fs::remove_file(part);
        return Err(FetchError::Size {
            key: item.key.clone(),
            url: item.url.clone(),
            expected: item.size,
            actual: bytes,
        });
    }
    let actual = hex(&hasher.finalize());
    if actual != item.sha256 {
        let _ = fs::remove_file(part);
        return Err(FetchError::Checksum {
            key: item.key.clone(),
            url: item.url.clone(),
            expected: item.sha256.clone(),
            actual,
        });
    }
    Ok(bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 of a file on disk, as lowercase hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// `<dir>/.<name>.part` (coursier's `CachePath.temporaryFile`).
fn part_path(dest: &Path) -> PathBuf {
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("download");
    dest.with_file_name(format!(".{name}.part"))
}

/// `<dir>/<name>.lock` (coursier's `CachePath.lockFile`).
fn lock_path(dest: &Path) -> PathBuf {
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("download");
    dest.with_file_name(format!("{name}.lock"))
}

/// Takes the per-file lock, waiting (polling, like coursier's retries) while another process
/// holds it.
fn acquire(lock_path: &Path, key: &str) -> Result<File, FetchError> {
    let started = Instant::now();
    loop {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|source| FetchError::Io { key: key.to_string(), path: lock_path.to_path_buf(), source })?;
        match try_lock(&file) {
            Ok(true) => return Ok(file),
            Ok(false) if started.elapsed() < LOCK_WAIT => std::thread::sleep(Duration::from_millis(50)),
            Ok(false) => return Err(FetchError::LockTimeout { key: key.to_string(), path: lock_path.to_path_buf() }),
            Err(source) => return Err(FetchError::Io { key: key.to_string(), path: lock_path.to_path_buf(), source }),
        }
    }
}

fn release(lock: File, lock_path: &Path) {
    // Coursier deletes the lock file after use; do the same, then drop (unlock) the handle.
    let _ = fs::remove_file(lock_path);
    drop(lock);
}

#[cfg(unix)]
fn try_lock(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: `flock` is plain data; zeroed is a valid initial state and we set every field
    // fcntl reads.
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as _;
    fl.l_whence = libc::SEEK_SET as _;
    fl.l_start = 0;
    fl.l_len = 0;
    // SAFETY: valid fd for the lifetime of `file`; `fl` outlives the call.
    let r = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &fl) };
    if r == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == libc::EAGAIN || code == libc::EACCES => Ok(false),
        _ => Err(err),
    }
}

#[cfg(not(unix))]
fn try_lock(file: &File) -> io::Result<bool> {
    // On Windows the JDK's FileChannel.tryLock uses LockFileEx, as does std's try_lock.
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_and_lock_names_match_coursier() {
        let dest = Path::new("/c/https/h/a/a-1.0.jar");
        assert_eq!(part_path(dest), Path::new("/c/https/h/a/.a-1.0.jar.part"));
        assert_eq!(lock_path(dest), Path::new("/c/https/h/a/a-1.0.jar.lock"));
    }

    #[test]
    fn lock_is_exclusive_across_processes() {
        // fcntl locks are per process, so prove exclusivity with a child process holding it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).unwrap();
        assert!(try_lock(&file).unwrap());
        if cfg!(unix) {
            let status = std::process::Command::new("python3")
                .args([
                    "-c",
                    "import fcntl,sys\nf=open(sys.argv[1],'r+')\ntry:\n fcntl.lockf(f,fcntl.LOCK_EX|fcntl.LOCK_NB)\n sys.exit(0)\nexcept OSError:\n sys.exit(3)",
                ])
                .arg(&path)
                .status();
            if let Ok(status) = status {
                assert_eq!(status.code(), Some(3), "another process must not get the lock");
            }
        }
    }

    #[test]
    fn hashes_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
