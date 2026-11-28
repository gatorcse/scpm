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
use crate::machine::{ProxyChoice, ProxySettings};

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
    /// Credentials sent only after an unauthenticated request is answered with a Basic
    /// challenge, as coursier does: `(realm, auth)`, the first whose realm is unset or equal to
    /// the challenge's wins.
    pub challenge: Vec<(Option<String>, Auth)>,
    /// `dest` may hold a file cached under another identity: trust it only if its sha256
    /// matches the lock.
    pub verify_cached: bool,
    /// Other repositories the file may come from, tried in order when this one answers 404 or
    /// 410 (design §5.4: the lock's repository is a hint). Each has its own URL, cache path and
    /// credentials, and none has fallbacks of its own; the sha256 is checked either way.
    pub fallbacks: Vec<FetchItem>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FetchOptions {
    /// Re-hash files already in the cache against the lock (`scpm install --verify`) and
    /// re-download those that don't match.
    pub verify: bool,
    /// The proxy (design §5.9); reqwest's own environment lookup is off. An unusable proxy for
    /// a scheme is only reported if something must be downloaded with that scheme.
    pub proxy: ProxyChoice,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchReport {
    pub present: usize,
    pub downloaded: usize,
    pub bytes: u64,
    /// Cached files re-hashed with `--verify`.
    pub verified: usize,
    /// Cached files that failed `--verify` and were downloaded again.
    pub repaired: usize,
    /// Problems that didn't stop the restore, e.g. a lock's repository that couldn't be asked
    /// (an unset credential variable) while another repository had the file.
    #[serde(skip)]
    pub warnings: Vec<String>,
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
    #[error("{0}")]
    Proxy(String),
    /// No source had the file: the first one's answer, and why other sources weren't asked.
    #[error("{error}{}", notes.iter().map(|n| format!("\n  note: {n}")).collect::<String>())]
    WithNotes { error: Box<FetchError>, notes: Vec<String> },
}

/// Every failed download, plus how many downloads were attempted per host.
#[derive(Debug)]
pub struct FetchFailure {
    pub errors: Vec<FetchError>,
    pub attempted: BTreeMap<String, usize>,
}

impl fmt::Display for FetchFailure {
    /// One line per problem: transport errors are collapsed per host (with a hint when every
    /// request to the host failed before an HTTP response), and credential and proxy errors per
    /// message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines: Vec<String> = Vec::new();
        let mut transport: BTreeMap<&str, Vec<&FetchError>> = BTreeMap::new();
        let mut credentials: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut proxy: Vec<&str> = Vec::new();
        for e in &self.errors {
            match e {
                FetchError::Transport { host, .. } => transport.entry(host).or_default().push(e),
                FetchError::Credentials { key, message } => credentials.entry(message).or_default().push(key),
                FetchError::Proxy(message) => {
                    if !proxy.contains(&message.as_str()) {
                        proxy.push(message);
                    }
                }
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
        lines.extend(proxy.into_iter().map(str::to_string));
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
    if !opts.verify {
        // Files possibly cached by another identity are only trusted after a hash check.
        let (unverified, trusted): (Vec<_>, Vec<_>) = present.into_iter().partition(|i| i.verify_cached);
        present = trusted;
        let hashes = hash_files(unverified.iter().map(|i| i.dest.as_path()).collect());
        for (item, hash) in unverified.into_iter().zip(hashes) {
            if hash.as_deref().is_ok_and(|h| h == item.sha256) {
                present.push(item);
            } else {
                missing.push((item, true));
            }
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
    let mut errors = Vec::new();
    let mut proxy_errors: Vec<String> = Vec::new();
    let mut downloads = Vec::new();
    let proxy = opts.proxy.as_ref();
    for (item, repair) in missing {
        // With fallbacks, a source that can't be asked is skipped like one answering 404
        // ([`fetch_with_fallbacks`]); its problem is reported only if no source has the file.
        if !item.fallbacks.is_empty() {
            downloads.push((item, repair));
        } else if let Err(message) = &item.auth {
            errors.push(FetchError::Credentials { key: item.key.clone(), message: message.clone() });
        } else if let Some(Err(message)) = proxy.map(|p| p.route(&item.url)) {
            // The environment's proxy for this URL's scheme is unusable; other schemes go on.
            if !proxy_errors.contains(&message) {
                proxy_errors.push(message);
            }
        } else {
            downloads.push((item, repair));
        }
    }
    errors.extend(proxy_errors.into_iter().map(FetchError::Proxy));
    if downloads.is_empty() {
        return if errors.is_empty() { Ok(report) } else { Err(FetchFailure { errors, attempted: BTreeMap::new() }) };
    }
    let client = match client(proxy) {
        Ok(client) => client,
        Err(e) => {
            errors.push(FetchError::Proxy(format!("cannot use the proxy: {}", error_chain(&e))));
            return Err(FetchFailure { errors, attempted: BTreeMap::new() });
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    // Only requests actually sent count, so "every request failed" stays true to its word.
    let (results, attempted) = runtime.block_on(download_all(&client, downloads, opts.verify, proxy));
    for (r, repair) in results {
        match r {
            Ok(Fetched { downloaded, rehashed, warnings }) => {
                for w in warnings {
                    if !report.warnings.contains(&w) {
                        report.warnings.push(w);
                    }
                }
                if rehashed && opts.verify {
                    report.verified += 1;
                }
                match downloaded {
                    Some(bytes) => {
                        report.downloaded += 1;
                        report.bytes += bytes;
                        if repair || rehashed {
                            report.repaired += 1;
                        }
                    }
                    None => report.present += 1,
                }
            }
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

/// The HTTP client: exactly the resolved proxy, never reqwest's own environment lookup.
/// Which requests use it is decided by [`ProxySettings::route`], the same matcher `scpm config
/// explain` uses and `cs`'s `http.nonProxyHosts` is built to agree with. Redirects never go
/// from HTTPS to plain HTTP, as in coursier.
fn client(proxy: Option<&ProxySettings>) -> reqwest::Result<reqwest::Client> {
    let routes = proxy.cloned();
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            match redirect_refusal(attempt.previous(), attempt.url(), routes.as_ref()) {
                Some(reason) => attempt.error(reason),
                None => attempt.follow(),
            }
        }))
        .no_proxy();
    if let Some(p) = proxy {
        // A proxy URL reqwest can't parse would silently mean "direct" inside the closure.
        for s in p.https.iter().chain(&p.http) {
            reqwest::Proxy::all(s.url())?;
        }
        let p = p.clone();
        builder = builder.proxy(reqwest::Proxy::custom(move |url| p.for_url(url.as_str()).map(|s| s.url())));
    }
    builder.build()
}

/// Up to 10 redirects, never from HTTPS to plain HTTP: the bytes are checked against the lock
/// either way, but coursier refuses such a downgrade and the fetcher must not differ. (reqwest
/// already drops `Authorization` when a redirect changes the scheme, host or port.) Nor to a
/// URL whose scheme's environment proxy is unusable: checked before the first request, it must
/// be checked again for the next, or the request would go out directly.
fn redirect_refusal(previous: &[reqwest::Url], next: &reqwest::Url, proxy: Option<&ProxySettings>) -> Option<String> {
    if previous.len() >= 10 {
        return Some("too many redirects".into());
    }
    let origin = next.host_str().map(|h| format!(" ({h})")).unwrap_or_default();
    let from_https = previous.iter().any(|u| u.scheme() == "https");
    if from_https && next.scheme() == "http" {
        return Some(format!("refused a redirect from HTTPS to plain HTTP{origin}"));
    }
    match proxy.map(|p| p.route(next.as_str())) {
        Some(Err(e)) => Some(format!("refused a redirect{origin}: {e}")),
        _ => None,
    }
}

/// Requests sent per host, for the "every request failed" help.
type Attempted = std::sync::Mutex<BTreeMap<String, usize>>;

async fn download_all(
    client: &reqwest::Client,
    items: Vec<(FetchItem, bool)>,
    verify: bool,
    proxy: Option<&ProxySettings>,
) -> (Vec<(Result<Fetched, FetchError>, bool)>, BTreeMap<String, usize>) {
    let permits = Arc::new(Semaphore::new(CONCURRENCY));
    let attempted = Arc::new(Attempted::default());
    let tasks: Vec<_> = items
        .into_iter()
        .map(|(item, repair)| {
            let client = client.clone();
            let permits = permits.clone();
            let attempted = attempted.clone();
            let proxy = proxy.cloned();
            tokio::spawn(async move {
                let _permit = permits.acquire().await.expect("semaphore open");
                (fetch_with_fallbacks(&client, &item, repair, verify, proxy.as_ref(), &attempted).await, repair)
            })
        })
        .collect();
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        out.push(t.await.expect("download task panicked"));
    }
    let attempted = attempted.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (out, attempted)
}

/// [`fetch_one`] from `item`, then from each of its fallbacks while the answer is 404 or 410
/// (design §5.4). A source whose credentials can't be read or whose scheme's proxy is unusable
/// can't be asked, and counts like a 404; any other answer (a 401 or 403, a transport error, a
/// checksum mismatch) ends the search. If no source has the file, the first one's answer is the
/// error, with a note for each source that couldn't be asked. If the first source couldn't be
/// asked but another had the file, that is a warning, so a missing token doesn't go unnoticed.
async fn fetch_with_fallbacks(
    client: &reqwest::Client,
    item: &FetchItem,
    repair: bool,
    verify: bool,
    proxy: Option<&ProxySettings>,
    attempted: &Attempted,
) -> Result<Fetched, FetchError> {
    let unusable = |source: &FetchItem| -> Option<FetchError> {
        if let Err(message) = &source.auth {
            return Some(FetchError::Credentials { key: source.key.clone(), message: message.clone() });
        }
        proxy.and_then(|p| p.route(&source.url).err()).map(FetchError::Proxy)
    };
    let not_asked = |source: &FetchItem, why: &FetchError| {
        let why = match why {
            FetchError::Credentials { message, .. } | FetchError::Proxy(message) => message.clone(),
            other => other.to_string(),
        };
        format!("{} was not asked: {why}", source.url)
    };
    let count = |source: &FetchItem| {
        *attempted.lock().unwrap_or_else(|e| e.into_inner()).entry(host_of(&source.url)).or_default() += 1;
    };
    let not_found = |r: &Result<Fetched, FetchError>| matches!(r, Err(FetchError::Http { status: 404 | 410, .. }));
    let mut skipped: Vec<String> = Vec::new();
    let first = match unusable(item) {
        Some(problem) => {
            if !item.fallbacks.is_empty() {
                skipped.push(not_asked(item, &problem));
            }
            Err(problem)
        }
        None => {
            count(item);
            let first = fetch_one(client, item, repair, verify).await;
            if !not_found(&first) {
                return first;
            }
            first
        }
    };
    let first_skipped = !skipped.is_empty();
    for fallback in &item.fallbacks {
        if let Some(problem) = unusable(fallback) {
            skipped.push(not_asked(fallback, &problem));
            continue;
        }
        count(fallback);
        let mut next = fetch_one(client, fallback, repair, verify).await;
        if let Ok(fetched) = &mut next
            && first_skipped
        {
            fetched.warnings.push(format!("{}: {}; {} had it", item.key, skipped[0], fallback.url));
        }
        if !not_found(&next) {
            return next;
        }
    }
    match first {
        // The first source's own problem is already the error; only the others need a note.
        Err(error) if first_skipped => {
            let notes: Vec<String> = skipped.into_iter().skip(1).collect();
            if notes.is_empty() { Err(error) } else { Err(FetchError::WithNotes { error: Box::new(error), notes }) }
        }
        Err(error) if !skipped.is_empty() => Err(FetchError::WithNotes { error: Box::new(error), notes: skipped }),
        other => other,
    }
}

/// What [`fetch_one`] did with a file.
struct Fetched {
    /// `Some(bytes)` if this call downloaded the file, `None` if another process did.
    downloaded: Option<u64>,
    /// A file that appeared while waiting for the lock was hashed against the lock (and
    /// downloaded again if it didn't match).
    rehashed: bool,
    /// Worth a warning though the file is present (see [`fetch_with_fallbacks`]).
    warnings: Vec<String>,
}

/// Makes `item` present. `replace` downloads even if a file of the right size is present (it
/// failed a hash check). With `verify` (or [`FetchItem::verify_cached`]), a file another process
/// wrote while this one waited for the lock is hashed before it is trusted.
async fn fetch_one(
    client: &reqwest::Client,
    item: &FetchItem,
    replace: bool,
    verify: bool,
) -> Result<Fetched, FetchError> {
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
    // Under the lock, look again: another process may have written (or repaired) the file.
    let mut rehashed = false;
    if fs::metadata(&item.dest).is_ok_and(|m| m.len() == item.size) {
        if !(replace || verify || item.verify_cached) {
            release(lock, &lock_path);
            return Ok(Fetched { downloaded: None, rehashed, warnings: Vec::new() });
        }
        // A queued repair was already counted as verified; a new file wasn't.
        rehashed = !replace;
        let dest = item.dest.clone();
        let hash = tokio::task::spawn_blocking(move || sha256_file(&dest)).await.expect("hash task");
        if hash.is_ok_and(|h| h == item.sha256) {
            release(lock, &lock_path);
            return Ok(Fetched { downloaded: None, rehashed, warnings: Vec::new() });
        }
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
                return renamed.map(|()| Fetched { downloaded: Some(bytes), rehashed, warnings: Vec::new() });
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

/// The realms of every `WWW-Authenticate: Basic realm="…"` challenge, in order.
fn basic_realms(resp: &reqwest::Response) -> Vec<String> {
    resp.headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(parse_basic_realms)
        .collect()
}

/// The realms of the Basic challenges in a `WWW-Authenticate` value, which may list several
/// challenges (`Bearer realm="api", Basic realm="packages"`, RFC 9110 §11.6.1).
fn parse_basic_realms(header: &str) -> Vec<String> {
    let token_end = |s: &str| s.find(|c: char| c == '=' || c == ',' || c.is_ascii_whitespace()).unwrap_or(s.len());
    let mut realms = Vec::new();
    let mut in_basic = false;
    let mut rest = header;
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        if rest.is_empty() {
            return realms;
        }
        let end = token_end(rest);
        if end == 0 {
            // A stray `=`.
            rest = &rest[1..];
            continue;
        }
        let token = &rest[..end];
        let after = rest[end..].trim_start_matches([' ', '\t']);
        let Some(value) = after.strip_prefix('=') else {
            // A token not followed by `=` starts a challenge (a token68 like `abc==` is skipped
            // as a parameter of the scheme before it).
            in_basic = token.eq_ignore_ascii_case("basic");
            rest = after;
            continue;
        };
        let value = value.trim_start_matches([' ', '\t']);
        let (parsed, tail) = match value.strip_prefix('"') {
            Some(quoted) => {
                // A quoted-string, with `\` escaping the next character.
                let mut out = String::new();
                let mut chars = quoted.char_indices();
                let mut close = None;
                while let Some((i, c)) = chars.next() {
                    match c {
                        '\\' => out.extend(chars.next().map(|(_, c)| c)),
                        '"' => {
                            close = Some(i);
                            break;
                        }
                        c => out.push(c),
                    }
                }
                let Some(close) = close else { return realms };
                (out, &quoted[close + 1..])
            }
            None => {
                let end = value.find(|c: char| c == ',' || c.is_ascii_whitespace()).unwrap_or(value.len());
                (value[..end].to_string(), &value[end..])
            }
        };
        if in_basic && token.eq_ignore_ascii_case("realm") {
            realms.push(parsed);
            // One realm per challenge.
            in_basic = false;
        }
        rest = tail;
    }
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
    let send = |auth: Option<&Auth>| {
        let mut req = client.get(&item.url);
        match auth {
            Some(Auth::Bearer(token)) => req = req.bearer_auth(token),
            Some(Auth::Basic { user, password }) => req = req.basic_auth(user, Some(password)),
            None => {}
        }
        req.send()
    };
    let mut resp = send(auth).await.map_err(transport)?;
    // Like coursier, credentials scoped to a realm go out only once the server asks for them.
    if resp.status().as_u16() == 401
        && auth.is_none()
        && let Some(challenge_auth) = basic_realms(&resp).iter().find_map(|realm| {
            item.challenge.iter().find(|(r, _)| r.as_deref().is_none_or(|r| r == realm)).map(|(_, a)| a)
        })
    {
        resp = send(Some(challenge_auth)).await.map_err(transport)?;
    }
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
        let len = chunk.len() as u64;
        if len > item.size - bytes {
            // Never write more than the lock's size: an oversized body could fill the disk.
            drop(file);
            let _ = fs::remove_file(part);
            return Err(FetchError::Size {
                key: item.key.clone(),
                url: item.url.clone(),
                expected: item.size,
                actual: bytes + len,
            });
        }
        hasher.update(&chunk);
        bytes += len;
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
        match lock_opened(file, lock_path) {
            Ok(Some(file)) => return Ok(file),
            Ok(None) if started.elapsed() < LOCK_WAIT => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => return Err(FetchError::LockTimeout { key: key.to_string(), path: lock_path.to_path_buf() }),
            Err(source) => return Err(FetchError::Io { key: key.to_string(), path: lock_path.to_path_buf(), source }),
        }
    }
}

/// Locks an opened lock file, keeping it only if it is still the file at `lock_path`. A
/// waiter may open the file just before its owner unlinks it in [`release`]; locking that
/// orphaned inode would not exclude a process that locks a new file at the same path, so the
/// caller retries with whatever is at the path now.
fn lock_opened(file: File, lock_path: &Path) -> io::Result<Option<File>> {
    if !try_lock(&file)? {
        return Ok(None);
    }
    Ok(is_current(&file, lock_path).then_some(file))
}

#[cfg(unix)]
fn is_current(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), fs::metadata(path)) {
        (Ok(held), Ok(at_path)) => held.dev() == at_path.dev() && held.ino() == at_path.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_current(_file: &File, path: &Path) -> bool {
    // Windows can't delete a file another handle has open, so an open lock file is still there.
    path.exists()
}

fn release(lock: File, lock_path: &Path) {
    // Coursier deletes the lock file after use; do the same, then drop (unlock) the handle.
    let _ = fs::remove_file(lock_path);
    drop(lock);
}

#[cfg(unix)]
pub fn try_lock(file: &File) -> io::Result<bool> {
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
pub fn try_lock(file: &File) -> io::Result<bool> {
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
    fn a_waiter_never_keeps_an_unlinked_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.jar.lock");
        let open = || OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).unwrap();
        // B opens the lock file, then pauses before locking it.
        let waiter = open();
        // A finishes and releases: the lock file is unlinked.
        let owner = lock_opened(open(), &path).unwrap().expect("A gets the lock");
        release(owner, &path);
        // C opens and locks a new file at the same path.
        let newcomer = lock_opened(open(), &path).unwrap().expect("C gets the new lock file");
        // B resumes: it can lock its orphaned inode, but must not use it.
        assert!(lock_opened(waiter, &path).unwrap().is_none());
        assert!(is_current(&newcomer, &path));
        release(newcomer, &path);
        // B retries from the path and gets a lock on the file that is there now.
        let retried = acquire(&path, "k").unwrap();
        assert!(is_current(&retried, &path));
    }

    #[test]
    fn basic_realms() {
        let realm = |h: &str| parse_basic_realms(h).into_iter().next();
        assert_eq!(realm("Basic realm=\"Sonatype Nexus\"").as_deref(), Some("Sonatype Nexus"));
        assert_eq!(realm("BASIC charset=\"UTF-8\", realm=\"R\"").as_deref(), Some("R"));
        assert_eq!(realm("Basic realm=\"R\", charset=\"UTF-8\"").as_deref(), Some("R"));
        assert_eq!(realm("Basic realm=R").as_deref(), Some("R"));
        assert_eq!(realm("Bearer realm=\"R\""), None);
        assert_eq!(realm("Basic charset=\"UTF-8\""), None);
        // Several challenges in one field; commas and escapes inside quoted values.
        assert_eq!(realm("Bearer realm=\"api\", Basic realm=\"packages\"").as_deref(), Some("packages"));
        assert_eq!(realm("Bearer error=\"a, Basic realm=x\", Basic realm=\"p\"").as_deref(), Some("p"));
        assert_eq!(realm("Negotiate abc==, Basic realm = \"a \\\"b\\\"\"").as_deref(), Some("a \"b\""));
        assert_eq!(realm("Basic, Bearer realm=\"R\""), None);
        // Every Basic realm, in order, so the caller can pick one it has credentials for.
        assert_eq!(
            parse_basic_realms(
                "Basic realm=\"legacy\", Bearer realm=\"x\", Basic realm=\"packages\", charset=\"UTF-8\""
            ),
            ["legacy", "packages"]
        );
        assert_eq!(parse_basic_realms("Basic realm=\"a\", realm=\"b\""), ["a"]);
        assert_eq!(parse_basic_realms("Basic realm=\"a\", Basic realm=\"unterminated"), ["a"]);
    }

    #[test]
    fn proxy_errors_are_collapsed_by_message() {
        let failure = FetchFailure {
            errors: vec![
                FetchError::Proxy("HTTPS_PROXY: bad".into()),
                FetchError::Proxy("HTTPS_PROXY: bad".into()),
                FetchError::Proxy("HTTP_PROXY: bad".into()),
            ],
            attempted: BTreeMap::new(),
        };
        assert_eq!(failure.to_string(), "HTTPS_PROXY: bad\nHTTP_PROXY: bad");
    }

    #[test]
    fn redirects_never_downgrade_to_plain_http() {
        let url = |u: &str| reqwest::Url::parse(u).unwrap();
        let https = url("https://repo.example/a.jar");
        let http = url("http://repo.example:8080/a.jar");
        assert!(redirect_refusal(std::slice::from_ref(&https), &http, None).unwrap().contains("HTTPS to plain HTTP"));
        let chain = [https.clone(), url("https://cdn.example/a.jar")];
        assert!(redirect_refusal(&chain, &http, None).is_some(), "not even later in the chain");
        assert_eq!(redirect_refusal(std::slice::from_ref(&http), &https, None), None, "upgrades are fine");
        assert_eq!(redirect_refusal(std::slice::from_ref(&https), &url("https://cdn.example/a.jar"), None), None);
        assert_eq!(redirect_refusal(std::slice::from_ref(&http), &http, None), None);
        assert!(redirect_refusal(&vec![http.clone(); 10], &http, None).unwrap().contains("too many"));

        // A redirect to a host whose scheme's proxy is unusable is refused, not sent directly;
        // a working route or a `noProxy` host is fine.
        let proxy = ProxySettings {
            https: None,
            http: Some(crate::machine::ProxyServer { host: "proxy.example".into(), port: 3128 }),
            no_proxy: vec!["cdn.internal".into()],
            ignored_no_proxy: Vec::new(),
            errors: [("https".to_string(), "HTTPS_PROXY: bad".to_string())].into(),
            source: "test".into(),
        };
        let refused = redirect_refusal(std::slice::from_ref(&http), &https, Some(&proxy)).unwrap();
        assert_eq!(refused, "refused a redirect (repo.example): HTTPS_PROXY: bad");
        assert_eq!(
            redirect_refusal(std::slice::from_ref(&http), &url("https://cdn.internal/a.jar"), Some(&proxy)),
            None
        );
        assert_eq!(redirect_refusal(std::slice::from_ref(&http), &http, Some(&proxy)), None);
    }

    #[test]
    fn hashes_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
