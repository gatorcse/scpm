//! Driving the native coursier CLI (`cs`) and sharing its cache (design §5.2).

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::config::{Credentials, Repository};
use crate::credentials::CoursierCredentials;

/// Maven Central, as coursier names it.
pub const CENTRAL_URL: &str = "https://repo1.maven.org/maven2/";

pub fn user_agent() -> String {
    format!("scpm/{} (+https://github.com/gatorcse/scpm)", env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, thiserror::Error)]
pub enum CoursierError {
    #[error("could not run `{bin}`: {source}. Install coursier (https://get-coursier.io) or set SCPM_CS")]
    Spawn { bin: String, source: std::io::Error },
    #[error("coursier failed:\n{0}")]
    Failed(String),
    #[error("unexpected coursier output: {0}")]
    Output(String),
    #[error("{0}")]
    Config(String),
    #[error("{0} resolved to no file; check its `type` and `classifier`")]
    NoArtifact(String),
}

/// A configured repository. Credentials stay references (environment variable or file names)
/// until a request actually needs them (design §5.7), so cached operations work without the
/// secrets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Repo {
    pub id: String,
    /// Always ends with `/`.
    pub url: String,
    /// Credentials sent to [`Self::fetch_url`]: the workspace's, else the user config's
    /// (design §5.9). Coursier's own credentials are looked up separately.
    pub credentials: Option<RepoCredentials>,
    pub credentials_from: CredentialsFrom,
    /// `allowInsecureCredentials`: credentials may go to this `http://` repository's origin.
    pub allow_insecure_credentials: bool,
}

/// Where a repository's credentials are configured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CredentialsFrom {
    /// The repository's `credentials` in the workspace file.
    #[default]
    Workspace,
    /// The user config entry for this URL (design §5.9).
    UserConfig { url: String },
}

impl CredentialsFrom {
    pub fn describe(&self) -> &'static str {
        match self {
            CredentialsFrom::Workspace => crate::config::WORKSPACE_FILE,
            CredentialsFrom::UserConfig { .. } => "user config",
        }
    }
}

/// A repository's configured credentials, as references read only when needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoCredentials {
    Bearer { token: Secret },
    Basic { user: Secret, password: Secret },
}

/// Where one credential value comes from.
#[derive(Clone, PartialEq, Eq)]
pub enum Secret {
    /// An environment variable.
    Env(String),
    /// A file's content, without a trailing newline (user config only).
    File(PathBuf),
    /// The value itself (user config only).
    Literal(String),
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Secret::Env(var) => write!(f, "Env({var})"),
            Secret::File(path) => write!(f, "File({})", path.display()),
            Secret::Literal(_) => f.write_str("Literal(<redacted>)"),
        }
    }
}

impl Secret {
    /// Reads the value; `what` names it in errors (`token`, `user`, `password`).
    fn read(&self, repo: &Repo, what: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, CoursierError> {
        let origin = match &repo.credentials_from {
            CredentialsFrom::Workspace => String::new(),
            CredentialsFrom::UserConfig { url } => format!(", from the user config entry for {url}"),
        };
        match self {
            Secret::Env(var) => match lookup(var) {
                Some(v) if !v.is_empty() => Ok(v),
                Some(_) => Err(CoursierError::Config(format!(
                    "repository `{}`: environment variable `{var}` ({what}{origin}) is empty",
                    repo.id
                ))),
                None => Err(CoursierError::Config(format!(
                    "repository `{}`: environment variable `{var}` ({what}{origin}) is not set",
                    repo.id
                ))),
            },
            Secret::File(path) => {
                let text = fs::read_to_string(path).map_err(|e| {
                    CoursierError::Config(format!(
                        "repository `{}`: cannot read {what} file {} ({e}){origin}",
                        repo.id,
                        path.display()
                    ))
                })?;
                let value = text.strip_suffix('\n').map(|t| t.strip_suffix('\r').unwrap_or(t)).unwrap_or(&text);
                if value.is_empty() {
                    return Err(CoursierError::Config(format!(
                        "repository `{}`: {what} file {} is empty{origin}",
                        repo.id,
                        path.display()
                    )));
                }
                Ok(value.to_string())
            }
            Secret::Literal(value) => Ok(value.clone()),
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum Auth {
    Bearer(String),
    Basic { user: String, password: String },
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Auth::Basic { user, .. } => {
                f.debug_struct("Basic").field("user", user).field("password", &"<redacted>").finish()
            }
        }
    }
}

impl Auth {
    /// The user name coursier sends (and puts in cache paths): bearer tokens go as the
    /// password of the user `token` (design §5.7).
    pub fn user(&self) -> &str {
        match self {
            Auth::Bearer(_) => "token",
            Auth::Basic { user, .. } => user,
        }
    }

    fn password(&self) -> &str {
        match self {
            Auth::Bearer(token) => token,
            Auth::Basic { password, .. } => password,
        }
    }
}

impl Repo {
    /// `scheme://host[:port]` of the URL.
    pub fn origin(&self) -> Option<&str> {
        let (scheme, rest) = self.url.split_once("://")?;
        let authority = rest.split('/').next().unwrap_or(rest);
        Some(&self.url[..scheme.len() + 3 + authority.len()])
    }

    /// The configured credentials, read from the environment. A missing or empty variable is
    /// an error, which callers raise only when the repository is actually contacted.
    pub fn auth(&self) -> Result<Option<Auth>, CoursierError> {
        self.auth_with(&|var| env::var(var).ok())
    }

    /// [`Self::auth`] with the variables read through `lookup`.
    fn auth_with(&self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Option<Auth>, CoursierError> {
        Ok(match &self.credentials {
            None => None,
            Some(RepoCredentials::Bearer { token }) => Some(Auth::Bearer(token.read(self, "token", lookup)?)),
            Some(RepoCredentials::Basic { user, password }) => Some(Auth::Basic {
                user: user.read(self, "user", lookup)?,
                password: password.read(self, "password", lookup)?,
            }),
        })
    }

    /// The user in coursier's cache path for configured credentials, if known without the
    /// password (`token` for bearer tokens; the user for basic auth).
    pub fn configured_user(&self) -> Option<String> {
        match &self.credentials {
            None => None,
            Some(RepoCredentials::Bearer { .. }) => Some("token".to_string()),
            Some(RepoCredentials::Basic { user, .. }) => user.read(self, "user", &|var| env::var(var).ok()).ok(),
        }
    }
}

/// Resolves repository config (`central` or full definitions). Credentials must be
/// `env:VARIABLE`; the variables are read later, when needed.
pub fn repositories(config: &[Repository]) -> Result<Vec<Repo>, CoursierError> {
    let env_name = |id: &str, value: &str, what: &str| -> Result<String, CoursierError> {
        let Some(var) = value.strip_prefix("env:") else {
            return Err(CoursierError::Config(format!(
                "repository `{id}`: {what} must be `env:VARIABLE` so secrets stay out of the workspace file"
            )));
        };
        let var = var.trim();
        if var.is_empty() {
            return Err(CoursierError::Config(format!("repository `{id}`: {what} is `env:` without a variable name")));
        }
        Ok(var.to_string())
    };
    config
        .iter()
        .map(|r| match r {
            Repository::Named(name) if name == "central" => {
                Ok(Repo { id: "central".into(), url: CENTRAL_URL.into(), ..Default::default() })
            }
            Repository::Named(name) => Err(CoursierError::Config(format!(
                "unknown repository `{name}`; the only built-in name is `central`, others need `{{id, url}}`"
            ))),
            Repository::Full(def) => {
                let credentials = match &def.credentials {
                    None => None,
                    Some(Credentials::Bearer(token)) => {
                        Some(RepoCredentials::Bearer { token: Secret::Env(env_name(&def.id, token, "token")?) })
                    }
                    Some(Credentials::Basic { user, password }) => Some(RepoCredentials::Basic {
                        user: Secret::Env(env_name(&def.id, user, "user")?),
                        password: Secret::Env(env_name(&def.id, password, "password")?),
                    }),
                };
                // `user:password@` would reach `cs`'s command line (and the lock's URLs).
                if def.url.split_once("://").is_some_and(|(_, rest)| {
                    rest.split(['/', '?', '#']).next().is_some_and(|authority| authority.contains('@'))
                }) {
                    return Err(CoursierError::Config(format!(
                        "repository `{}`: the URL must not contain credentials (`user@`); use `credentials: {{user: env:U, password: env:P}}`",
                        def.id
                    )));
                }
                if credentials.is_some() && is_plain_http(&def.url) && !def.allow_insecure_credentials {
                    // Only the origin: a path or query may carry a token.
                    let (scheme, rest) = def.url.split_once("://").unwrap_or(("http", ""));
                    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
                    return Err(CoursierError::Config(format!(
                        "repository `{}`: credentials would be sent over plain HTTP to {scheme}://{authority}; use an https:// URL, or set `allowInsecureCredentials: true` to send them to exactly this scheme, host and port",
                        def.id
                    )));
                }
                let url = if def.url.ends_with('/') { def.url.clone() } else { format!("{}/", def.url) };
                Ok(Repo {
                    id: def.id.clone(),
                    url,
                    credentials,
                    allow_insecure_credentials: def.allow_insecure_credentials,
                    ..Default::default()
                })
            }
        })
        .collect()
}

pub(crate) fn is_plain_http(url: &str) -> bool {
    url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://"))
}

/// The origins (`http://host[:port]`) of plain-HTTP repositories that opted in to receiving
/// credentials (`allowInsecureCredentials`); credentials go over plain HTTP nowhere else.
pub fn insecure_credential_origins(repos: &[Repo]) -> Vec<String> {
    repos
        .iter()
        .filter(|r| r.allow_insecure_credentials && is_plain_http(&r.url))
        .filter_map(|r| r.origin().map(str::to_string))
        .collect()
}

/// The coursier cache directory: `COURSIER_CACHE`, the legacy `~/.coursier/cache/v1` if it
/// exists, then the platform default.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = env::var_os("COURSIER_CACHE").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(home) = dirs::home_dir() {
        let legacy = home.join(".coursier/cache/v1");
        if legacy.is_dir() {
            return legacy;
        }
    }
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".cache"));
    if cfg!(target_os = "macos") {
        base.join("Coursier/v1")
    } else if cfg!(windows) {
        base.join("Coursier/Cache/v1")
    } else {
        base.join("coursier/v1")
    }
}

/// Where coursier stores the file for `url` (mirrors `coursier.paths.CachePath.localFile`).
/// `None` if the path would leave the cache (`.`/`..` or empty segments, or an absolute path).
pub fn cache_path(cache: &Path, url: &str, user: Option<&str>) -> Option<PathBuf> {
    let (protocol, rest) = url.split_once("://")?;
    let user_part = user.map(|u| format!("{u}@")).unwrap_or_default();
    let rel = escape(&format!("{protocol}/{user_part}{rest}"));
    // A `\\` (the high escape character of U+0250..U+025F) is a separator only on Windows.
    let contained = rel
        .split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !(cfg!(windows) && seg.contains('\\')))
        && Path::new(&rel).is_relative();
    contained.then(|| cache.join(rel))
}

/// The cached file for `url` downloaded with *some* user (`<protocol>/<user>@<host>/…`), for
/// when the user isn't known (its environment variable is unset) but the file may be cached.
pub fn cached_with_any_user(cache: &Path, url: &str) -> Option<PathBuf> {
    let (protocol, rest) = url.split_once("://")?;
    let (host, path) = rest.split_once('/')?;
    let suffix = escape(&format!("@{host}"));
    let rel_path = escape(path);
    std::fs::read_dir(cache.join(protocol))
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(&suffix)))
        .map(|e| e.path().join(&rel_path))
        .find(|p| p.is_file())
}

/// Coursier's escaping: unsafe characters become `%XX`; `/` is kept.
fn escape(input: &str) -> String {
    let to_hex =
        |n: u16| char::from_u32(if n < 10 { '0' as u32 + n as u32 } else { 'A' as u32 + n as u32 - 10 }).unwrap_or('?');
    let mut out = String::with_capacity(input.len());
    for unit in input.encode_utf16() {
        let unsafe_char = unit > 128 || char::from_u32(unit as u32).is_some_and(|c| " %$&+,:;=?@<>#".contains(c));
        if unsafe_char {
            out.push('%');
            out.push(to_hex(unit / 16));
            out.push(to_hex(unit % 16));
        } else {
            out.push(char::from_u32(unit as u32).unwrap_or('?'));
        }
    }
    out
}

/// The URL a cached file was downloaded from (inverse of [`cache_path`]).
pub fn url_from_cache_path(cache: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(cache).ok()?;
    let rel = rel.to_str()?;
    let rel = if cfg!(windows) { rel.replace('\\', "/") } else { rel.to_string() };
    let (protocol, rest) = rel.split_once('/')?;
    let rest = unescape(rest);
    // Drop a `user@` prefix on the host.
    let rest = match rest.split_once('/') {
        Some((host, path)) => match host.rsplit_once('@') {
            Some((_, h)) => format!("{h}/{path}"),
            None => format!("{host}/{path}"),
        },
        None => rest,
    };
    Some(format!("{protocol}://{rest}"))
}

/// Inverse of [`escape`]: each `%HL` is one UTF-16 code unit `16 * H + L` (coursier escapes
/// units, not UTF-8 bytes, so `é` is `%E9`). `H` is a digit or `'A' + n - 10`, which runs past
/// `F` for units above U+00FF (`Ā` is `%G0`).
fn unescape(s: &str) -> String {
    let digit = |c: char| -> Option<u32> {
        match c {
            '0'..='9' => Some(c as u32 - '0' as u32),
            c if c >= 'A' => Some(c as u32 - 'A' as u32 + 10),
            _ => None,
        }
    };
    let mut units: Vec<u16> = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let mut ahead = chars.clone();
            if let (Some(high), Some(low)) = (ahead.next().and_then(digit), ahead.next().and_then(digit))
                && low < 16
                && let Ok(unit) = u16::try_from(high * 16 + low)
            {
                units.push(unit);
                chars = ahead;
                continue;
            }
        }
        let mut buf = [0u16; 2];
        units.extend_from_slice(c.encode_utf16(&mut buf));
    }
    String::from_utf16_lossy(&units)
}

/// The `cs` binary: `SCPM_CS` or `cs` on `PATH`.
fn cs_binary() -> String {
    env::var("SCPM_CS").unwrap_or_else(|_| "cs".to_string())
}

/// One coursier resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ResolveRequest {
    /// Dependencies in coursier's string syntax, e.g. `org:name_3:1.0,exclude=org%x`.
    pub dependencies: Vec<String>,
    /// Dependencies fetched without their own dependencies (`--intransitive`).
    pub intransitive: Vec<String>,
    pub boms: Vec<String>,
    /// Forced versions, `org:name:version`.
    pub forced: Vec<String>,
    pub jdk_version: Option<String>,
}

/// A resolved artifact, in classpath order.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedArtifact {
    /// `group:artifact:version[:classifier][@type]` (design §5.4).
    pub key: String,
    pub file: PathBuf,
    pub url: String,
}

#[derive(Deserialize)]
struct Report {
    dependencies: Vec<ReportDep>,
}

#[derive(Deserialize)]
struct ReportDep {
    coord: String,
    file: Option<String>,
}

/// A root dependency string's parts: `(org, name, classifier, type)`.
fn dependency_parts(dep: &str) -> (&str, &str, Option<&str>, Option<&str>) {
    let mut params = dep.split(',');
    let mut coord = params.next().unwrap_or_default().split(':');
    let (org, name) = (coord.next().unwrap_or_default(), coord.next().unwrap_or_default());
    let (mut classifier, mut kind) = (None, None);
    for p in params {
        if let Some(c) = p.strip_prefix("classifier=") {
            classifier = Some(c);
        } else if let Some(t) = p.strip_prefix("type=") {
            kind = Some(t);
        }
    }
    (org, name, classifier, kind)
}

/// Artifact types coursier keeps (`-A`): its defaults plus whatever the roots declare, so
/// `sources`/`javadoc` classifiers and types such as `exe` or `aar` are not dropped.
fn artifact_types(req: &ResolveRequest) -> Vec<String> {
    let mut types: Vec<String> = ["jar", "bundle", "test-jar"].map(String::from).to_vec();
    for dep in req.dependencies.iter().chain(&req.intransitive) {
        let (_, _, classifier, kind) = dependency_parts(dep);
        let implied = match classifier {
            Some("sources") => Some("src"),
            Some("javadoc") => Some("doc"),
            _ => None,
        };
        for t in [kind, implied].into_iter().flatten() {
            if t != "pom" && !types.iter().any(|x| x == t) {
                types.push(t.to_string());
            }
        }
    }
    types
}

/// `(group, artifact, classifier)` of a report coordinate (`g:a:v` or `g:a:type:classifier:v`).
fn report_parts(coord: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<&str> = coord.split(':').collect();
    match parts.as_slice() {
        [g, a, _] | [g, a, _, _] => Some((g, a, "")),
        [g, a, _, c, _] => Some((g, a, c)),
        _ => None,
    }
}

/// Runs `cs fetch` and returns the classpath with each artifact's URL.
pub fn resolve(
    req: &ResolveRequest,
    repos: &[Repo],
    cache: &Path,
    scratch: &Path,
) -> Result<Vec<ResolvedArtifact>, CoursierError> {
    let report_path = scratch.join(format!("report-{}.json", unique_suffix()));
    let mut cmd = Command::new(cs_binary());
    cmd.arg("fetch").arg("--quiet").arg("--classpath").arg("--json-output-file").arg(&report_path);
    cmd.arg("--artifact-type").arg(artifact_types(req).join(","));
    let common = add_common_args(&mut cmd, repos, cache)?;
    add_request_args(&mut cmd, req);
    let out = cmd.output().map_err(|source| CoursierError::Spawn { bin: cs_binary(), source })?;
    if !out.status.success() {
        let _ = fs::remove_file(&report_path);
        return Err(common.failure(&out.stderr));
    }
    let report_text = fs::read_to_string(&report_path).map_err(|e| CoursierError::Output(e.to_string()))?;
    let _ = fs::remove_file(&report_path);
    let report: Report = serde_json::from_str(&report_text).map_err(|e| CoursierError::Output(e.to_string()))?;

    // A root that asks for a specific artifact (a classifier or a non-jar type) must yield a
    // file. Plain coordinates may legitimately have none: `<packaging>pom</packaging>`
    // aggregators such as groovy-all.
    for dep in req.dependencies.iter().chain(&req.intransitive) {
        let (org, name, classifier, kind) = dependency_parts(dep);
        if classifier.is_none() && kind.is_none_or(|k| matches!(k, "jar" | "bundle" | "pom")) {
            continue;
        }
        let want = classifier.unwrap_or(if kind == Some("test-jar") { "tests" } else { "" });
        let found = report.dependencies.iter().any(|d| {
            d.file.is_some() && report_parts(&d.coord).is_some_and(|(g, a, c)| g == org && a == name && c == want)
        });
        if !found {
            return Err(CoursierError::NoArtifact(dep.clone()));
        }
    }

    let mut by_file: BTreeMap<&str, String> = BTreeMap::new();
    for dep in &report.dependencies {
        let Some(file) = &dep.file else { continue };
        let key = artifact_key(&dep.coord, Path::new(file))
            .ok_or_else(|| CoursierError::Output(format!("coordinate `{}`", dep.coord)))?;
        by_file.insert(file.as_str(), key);
    }
    let classpath = String::from_utf8_lossy(&out.stdout);
    let sep = if cfg!(windows) { ';' } else { ':' };
    let mut result = Vec::new();
    for entry in classpath.trim().split(sep).filter(|e| !e.is_empty()) {
        let file = PathBuf::from(entry);
        let key = by_file
            .get(entry)
            .ok_or_else(|| CoursierError::Output(format!("classpath entry {entry} missing from the report")))?;
        let url = url_from_cache_path(cache, &file).ok_or_else(|| {
            CoursierError::Output(format!("{entry} is not inside the coursier cache {}", cache.display()))
        })?;
        result.push(ResolvedArtifact { key: key.clone(), file, url });
    }
    Ok(result)
}

/// Asks coursier which dependency chains in `req` lead to `module` (`org:name`), via
/// `cs resolve --what-depends-on`. Each chain runs from a root of the request to the module.
pub fn what_depends_on(
    req: &ResolveRequest,
    module: &str,
    repos: &[Repo],
    cache: &Path,
) -> Result<Vec<Vec<String>>, CoursierError> {
    let mut cmd = Command::new(cs_binary());
    cmd.arg("resolve").arg("--quiet").arg("--what-depends-on").arg(module);
    let common = add_common_args(&mut cmd, repos, cache)?;
    add_request_args(&mut cmd, req);
    let out = cmd.output().map_err(|source| CoursierError::Spawn { bin: cs_binary(), source })?;
    if !out.status.success() {
        return Err(common.failure(&out.stderr));
    }
    let roots: Vec<String> = req
        .dependencies
        .iter()
        .chain(&req.intransitive)
        .map(|d| {
            let (org, name, _, _) = dependency_parts(d);
            format!("{org}:{name}")
        })
        .collect();
    Ok(reverse_tree_chains(&String::from_utf8_lossy(&out.stdout), &roots))
}

/// Turns coursier's reversed tree (the module at the root, dependees as children) into chains
/// from each root of the resolution (`org:name` in `roots`, or a leaf) down to the module.
fn reverse_tree_chains(tree: &str, roots: &[String]) -> Vec<Vec<String>> {
    let nodes: Vec<(usize, String)> = tree
        .lines()
        .map(strip_ansi)
        .filter_map(|line| {
            let start = line.char_indices().find(|(_, c)| c.is_ascii_alphanumeric())?.0;
            let depth = line[..start].chars().count() / 3;
            let coord = line[start..].split_whitespace().next()?.to_string();
            (depth > 0).then(|| (depth - 1, coord))
        })
        .collect();
    let mut chains: Vec<Vec<String>> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    for (i, (depth, coord)) in nodes.iter().enumerate() {
        stack.truncate(*depth);
        stack.push(coord.clone());
        let leaf = nodes.get(i + 1).is_none_or(|(next, _)| next <= depth);
        let module: String = coord.split(':').take(2).collect::<Vec<_>>().join(":");
        if leaf || roots.contains(&module) {
            let chain: Vec<String> = stack.iter().rev().cloned().collect();
            if !chains.contains(&chain) {
                chains.push(chain);
            }
        }
    }
    chains
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Lists the available versions of `org:name` (ascending, as coursier prints them).
pub fn list_versions(org: &str, name: &str, repos: &[Repo], cache: &Path) -> Result<Vec<String>, CoursierError> {
    let mut cmd = Command::new(cs_binary());
    cmd.arg("complete-dep").arg("--quiet");
    let common = add_common_args(&mut cmd, repos, cache)?;
    cmd.arg(format!("{org}:{name}:"));
    let out = cmd.output().map_err(|source| CoursierError::Spawn { bin: cs_binary(), source })?;
    if !out.status.success() {
        return Err(common.failure(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn add_request_args(cmd: &mut Command, req: &ResolveRequest) {
    for bom in &req.boms {
        cmd.arg("--bom").arg(bom);
    }
    for forced in &req.forced {
        cmd.arg("--force-version").arg(forced);
    }
    if let Some(jdk) = &req.jdk_version {
        cmd.arg("--jdk-version").arg(jdk);
    }
    for dep in &req.intransitive {
        cmd.arg("--intransitive").arg(dep);
    }
    cmd.args(&req.dependencies);
}

/// State that must outlive a `cs` invocation: the credential file, and notes about
/// credentials that could not be read (reported only if coursier then fails).
struct CommonArgs {
    _credential_file: Option<tempfile::NamedTempFile>,
    /// A private, empty directory: the (absent) Scala CLI config and mirror file `cs` is
    /// pointed at live in it.
    _sealed_dir: Option<tempfile::TempDir>,
    missing_credentials: Vec<String>,
}

impl CommonArgs {
    fn failure(&self, stderr: &[u8]) -> CoursierError {
        let mut msg = clean_stderr(stderr);
        for note in &self.missing_credentials {
            msg.push_str(&format!("\n  note: {note}"));
        }
        CoursierError::Failed(msg)
    }
}

/// Environment variables through which coursier would pick up machine settings scpm doesn't
/// apply itself (design §5.9): extra mirror files, default repositories and cache policy.
const UNSEALED_ENV: &[&str] = &["COURSIER_EXTRA_MIRRORS", "COURSIER_REPOSITORIES", "COURSIER_TTL", "COURSIER_MODE"];

/// Cache, repositories and credentials, with `cs` sealed off from every other source of machine
/// settings (design §5.9). Secrets never go on the command line: every credential `cs` may use
/// is written to a private (0600) temporary file that `COURSIER_CREDENTIALS` names for the
/// child, which also stops `cs` from reading coursier's own sources unfiltered.
/// `SCALA_CLI_CONFIG` and `COURSIER_MIRRORS` name files that don't exist, so `cs` skips the Scala
/// CLI config's credentials and mirrors (coursier reads those even with `COURSIER_CREDENTIALS`
/// set) and the mirror files (a `COURSIER_MIRRORS` naming a missing file also disables
/// `mirror.properties` in coursier's config directory; `COURSIER_EXTRA_MIRRORS` is read
/// regardless, so it is removed). `COURSIER_MAVEN_SETTINGS=false` turns off Maven `settings.xml`.
fn add_common_args(cmd: &mut Command, repos: &[Repo], cache: &Path) -> Result<CommonArgs, CoursierError> {
    cmd.arg("--cache").arg(cache).arg("--no-default").arg("--user-agent").arg(user_agent());
    for repo in repos {
        cmd.arg("--repository").arg(&repo.url);
    }
    let mut missing_credentials = Vec::new();
    let properties =
        credential_properties(repos, &|var| env::var(var).ok(), &CoursierCredentials::load(), &mut missing_credentials);
    // NamedTempFile is created with mode 0600 on Unix.
    let mut file = tempfile::Builder::new()
        .prefix("scpm-credentials-")
        .suffix(".properties")
        .tempfile()
        .map_err(|e| CoursierError::Config(format!("cannot create a credentials file: {e}")))?;
    file.write_all(properties.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| CoursierError::Config(format!("cannot write a credentials file: {e}")))?;
    cmd.env("COURSIER_CREDENTIALS", credentials_env_value(file.path()));
    let sealed = tempfile::Builder::new()
        .prefix("scpm-sealed-")
        .tempdir()
        .map_err(|e| CoursierError::Config(format!("cannot create a temporary directory: {e}")))?;
    cmd.env("SCALA_CLI_CONFIG", sealed.path().join("config.json"));
    cmd.env("COURSIER_MIRRORS", sealed.path().join("mirror.properties"));
    cmd.env("COURSIER_MAVEN_SETTINGS", "false");
    for var in UNSEALED_ENV {
        cmd.env_remove(var);
    }
    Ok(CommonArgs { _credential_file: Some(file), _sealed_dir: Some(sealed), missing_credentials })
}

/// `COURSIER_CREDENTIALS` naming a file: an absolute path, or (on Windows) a `file:` URL, which
/// coursier reads with `new URI`, so characters a URI path can't hold are percent-encoded.
fn credentials_env_value(path: &Path) -> String {
    let text = path.display().to_string();
    if text.starts_with('/') {
        return text;
    }
    let mut url = String::from("file:///");
    for b in text.replace('\\', "/").bytes() {
        if b.is_ascii_alphanumeric() || b"/:-._~!$&'()*+,;=@".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// The credentials file for `cs`: each repository's configured credentials, scoped to its
/// origin, then coursier's own entries restricted to scpm's plain-HTTP policy.
/// `lookup` reads the environment variables configured credentials name.
fn credential_properties(
    repos: &[Repo],
    lookup: &dyn Fn(&str) -> Option<String>,
    coursier: &CoursierCredentials,
    missing: &mut Vec<String>,
) -> String {
    let mut properties = String::new();
    let mut entry = |prefix: String, host: &str, user: &str, password: &str, extra: &[(&str, String)]| {
        properties.push_str(&format!(
            "{prefix}.host={}\n{prefix}.username={}\n{prefix}.password={}\n",
            escape_property(host),
            escape_property(user),
            escape_property(password)
        ));
        for (key, value) in extra {
            properties.push_str(&format!("{prefix}.{key}={}\n", escape_property(value)));
        }
    };
    for (i, repo) in repos.iter().enumerate() {
        let Some(origin) = repo.origin() else { continue };
        match repo.auth_with(lookup) {
            // `scheme://host[:port]` scopes the credentials to the repository's scheme, host and
            // port (coursier 2.1.27 ignores `https-only` for a host with a scheme).
            Ok(Some(auth)) => {
                entry(format!("scpm{i}"), origin, auth.user(), auth.password(), &[("auto", "true".into())])
            }
            Ok(None) => {}
            Err(e) => missing.push(e.to_string()),
        }
    }
    let allowed = coursier.restricted(&insecure_credential_origins(repos));
    for (i, c) in allowed.entries.iter().enumerate() {
        let mut extra = vec![("auto", c.auto.to_string()), ("https-only", c.https_only.to_string())];
        if let Some(realm) = &c.realm {
            extra.push(("realm", realm.clone()));
        }
        entry(format!("coursier{i}"), &c.host_spec(), &c.user, &c.password, &extra);
    }
    properties
}

/// Escapes a value for a Java properties file.
fn escape_property(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' | '=' | ':' | '#' | '!' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // Java's reader drops leading tabs and form feeds of a value; escaped, they survive.
            '\t' => out.push_str("\\t"),
            '\u{c}' => out.push_str("\\f"),
            c => out.push(c),
        }
    }
    if out.starts_with(' ') {
        out.insert(0, '\\');
    }
    out
}

/// Drops JVM/native-image noise (e.g. `sun.misc.Unsafe` warnings) from coursier's stderr.
fn clean_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).lines().filter(|l| !l.starts_with("WARNING:")).collect::<Vec<_>>().join("\n")
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Builds a lock key from coursier's report coordinate (`g:a:v` or `g:a:type:classifier:v`)
/// and the file's extension.
pub fn artifact_key(coord: &str, file: &Path) -> Option<String> {
    let parts: Vec<&str> = coord.split(':').collect();
    let (group, artifact, classifier, version) = match parts.as_slice() {
        [g, a, v] | [g, a, _, v] => (*g, *a, None, *v),
        [g, a, _ty, c, v] => (*g, *a, (!c.is_empty()).then_some(*c), *v),
        _ => return None,
    };
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("jar");
    let mut key = format!("{group}:{artifact}:{version}");
    if let Some(c) = classifier {
        key.push(':');
        key.push_str(c);
    }
    if ext != "jar" {
        key.push('@');
        key.push_str(ext);
    }
    Some(key)
}

/// The Maven repository path for a lock key: `g/r/o/u/p/artifact/version/artifact-version[-classifier].ext`.
/// `None` for a malformed key, including components that would navigate paths (empty, `.`,
/// `..`, or containing `/` or `\`), since lock keys become cache paths.
pub fn maven_path(key: &str) -> Option<String> {
    let (coords, ext) = match key.split_once('@') {
        Some((c, e)) => (c, e),
        None => (key, "jar"),
    };
    let parts: Vec<&str> = coords.split(':').collect();
    let (group, artifact, version, classifier) = match parts.as_slice() {
        [g, a, v] => (*g, *a, *v, None),
        [g, a, v, c] => (*g, *a, *v, Some(*c)),
        _ => return None,
    };
    let safe = |s: &str| !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\']);
    if !(group.split('.').all(safe) && [artifact, version, ext].into_iter().chain(classifier).all(safe)) {
        return None;
    }
    let suffix = classifier.map(|c| format!("-{c}")).unwrap_or_default();
    Some(format!("{}/{artifact}/{version}/{artifact}-{version}{suffix}.{ext}", group.replace('.', "/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_round_trip() {
        let cache = Path::new("/c");
        let url = "https://repo1.maven.org/maven2/org/typelevel/cats-core_3/2.13.0/cats-core_3-2.13.0.jar";
        let path = cache_path(cache, url, None).unwrap();
        assert_eq!(
            path,
            Path::new("/c/https/repo1.maven.org/maven2/org/typelevel/cats-core_3/2.13.0/cats-core_3-2.13.0.jar")
        );
        assert_eq!(url_from_cache_path(cache, &path).unwrap(), url);
    }

    #[test]
    fn escaping_matches_coursier() {
        assert_eq!(escape("https/host:8443/a b+c"), "https/host%3A8443/a%20b%2Bc");
        let path = cache_path(Path::new("/c"), "https://h.example/p/x.jar", Some("alice")).unwrap();
        assert_eq!(path, Path::new("/c/https/alice%40h.example/p/x.jar"));
        assert_eq!(url_from_cache_path(Path::new("/c"), &path).unwrap(), "https://h.example/p/x.jar");
    }

    #[test]
    fn keys_and_paths() {
        let key =
            artifact_key("io.netty:netty-transport-native-epoll:jar:linux-x86_64:4.1.115.Final", Path::new("x.jar"))
                .unwrap();
        assert_eq!(key, "io.netty:netty-transport-native-epoll:4.1.115.Final:linux-x86_64");
        assert_eq!(
            maven_path(&key).unwrap(),
            "io/netty/netty-transport-native-epoll/4.1.115.Final/netty-transport-native-epoll-4.1.115.Final-linux-x86_64.jar"
        );
        let aar = artifact_key("g.x:a:1.0", Path::new("a-1.0.aar")).unwrap();
        assert_eq!(aar, "g.x:a:1.0@aar");
        assert_eq!(maven_path(&aar).unwrap(), "g/x/a/1.0/a-1.0.aar");
    }

    #[test]
    fn lock_keys_cannot_leave_the_cache() {
        for key in [
            "g:a:..",
            "g:..:1",
            "..:a:1",
            "g..x:a:1",
            "g:a/b:1",
            "g:a:1:../../x",
            "g:a:1@../x",
            "g:a:1@",
            "g:a:1\\x",
            "g::1",
        ] {
            assert_eq!(maven_path(key), None, "{key}");
        }
        let cache = Path::new("/c");
        assert!(cache_path(cache, "https://h/a/../../../etc/passwd", None).is_none());
        assert!(cache_path(cache, "https://h/a//b", None).is_none());
        assert!(cache_path(cache, "https://h/./a", None).is_none());
        assert!(cache_path(cache, "https://h/a/b.jar", None).is_some());
    }

    #[test]
    fn non_ascii_cache_paths_round_trip() {
        let cache = Path::new("/c");
        let url = "https://h.example/dépôt/g/a/1/a-1.jar";
        let path = cache_path(cache, url, None).unwrap();
        assert_eq!(path, Path::new("/c/https/h.example/d%E9p%F4t/g/a/1/a-1.jar"));
        assert_eq!(url_from_cache_path(cache, &path).unwrap(), url);
        // Above U+00FF the high "digit" runs past `F`; astral characters are two units.
        let url = "https://h.example/Ā/日本/\u{1F600}/a-1.jar";
        let path = cache_path(cache, url, None).unwrap();
        assert!(path.starts_with("/c/https/h.example/%G0/"), "{}", path.display());
        assert_eq!(url_from_cache_path(cache, &path).unwrap(), url);
        // U+0250's high escape character is `\`, an ordinary filename character on Unix.
        if cfg!(unix) {
            let url = "https://h.example/\u{250}/a-1.jar";
            let path = cache_path(cache, url, None).unwrap();
            assert_eq!(path, Path::new("/c/https/h.example/%\\0/a-1.jar"));
            assert_eq!(url_from_cache_path(cache, &path).unwrap(), url);
        }
    }

    #[test]
    fn repository_urls_must_not_carry_credentials() {
        let repo = |url: &str| {
            repositories(&[Repository::Full(crate::config::RepositoryDef {
                id: "acme".into(),
                url: url.into(),
                credentials: None,
                allow_insecure_credentials: false,
            })])
        };
        let err = repo("https://alice:hunter2@acme.example/maven").unwrap_err().to_string();
        assert!(err.contains("must not contain credentials") && !err.contains("hunter2"), "{err}");
        assert!(repo("https://acme.example/maven/@scope/").is_ok(), "an `@` in the path is fine");
    }

    #[test]
    fn configured_credentials_are_scoped_to_the_repository_origin() {
        let repos = vec![Repo {
            id: "r".into(),
            url: "https://repo.example:8443/maven/".into(),
            credentials: Some(RepoCredentials::Bearer { token: Secret::Env("REPO_TOKEN".into()) }),
            allow_insecure_credentials: false,
            ..Default::default()
        }];
        let lookup = |var: &str| (var == "REPO_TOKEN").then(|| "tok3n".to_string());
        let mut missing = Vec::new();
        let text = credential_properties(&repos, &lookup, &CoursierCredentials::default(), &mut missing);
        assert!(missing.is_empty(), "{missing:?}");
        let parsed = crate::credentials::CoursierCredentials { entries: crate::credentials::parse_properties(&text) };
        assert_eq!(parsed.find("https://repo.example:8443/maven/x.jar").unwrap().password, "tok3n");
        assert!(parsed.find("https://repo.example/maven/x.jar").is_none(), "another port");
        assert!(parsed.find("https://repo.example:9443/maven/x.jar").is_none(), "another port");
    }

    fn repo_with(credentials: Credentials) -> Result<Vec<Repo>, CoursierError> {
        repositories(&[Repository::Full(crate::config::RepositoryDef {
            id: "acme".into(),
            url: "https://acme.example/maven".into(),
            credentials: Some(credentials),
            allow_insecure_credentials: false,
        })])
    }

    #[test]
    fn plain_http_credentials_need_an_opt_in() {
        let def = |url: &str, allow: bool| {
            repositories(&[Repository::Full(crate::config::RepositoryDef {
                id: "nexus".into(),
                url: url.into(),
                credentials: Some(Credentials::Bearer("env:NEXUS_TOKEN".into())),
                allow_insecure_credentials: allow,
            })])
        };
        let err = def("http://nexus.local:8081/repo?token=s3cret", false).unwrap_err().to_string();
        assert!(err.contains("repository `nexus`") && err.contains("plain HTTP"), "{err}");
        assert!(err.contains("http://nexus.local:8081;") && !err.contains("s3cret"), "only the origin: {err}");
        assert!(err.contains("https://") && err.contains("allowInsecureCredentials: true"), "{err}");
        assert!(def("HTTP://nexus.local:8081/repo", false).is_err());
        assert!(def("https://nexus.local/repo", false).is_ok());
        let repos = def("http://nexus.local:8081/repo", true).unwrap();
        assert_eq!(insecure_credential_origins(&repos), ["http://nexus.local:8081"]);
    }

    #[test]
    fn coursier_credentials_reach_cs_only_under_the_http_policy() {
        let repo = |url: &str, allow: bool| Repo {
            id: "r".into(),
            url: url.into(),
            credentials: None,
            allow_insecure_credentials: allow,
            ..Default::default()
        };
        let coursier = crate::credentials::CoursierCredentials {
            entries: crate::credentials::parse_properties(
                "a.host=nexus.local\na.username=alice\na.password=pw\na.https-only=false\n\
                 b.host=https://secure.example\nb.username=bob\nb.password=pw\nb.realm=R\n",
            ),
        };
        let parse = |repos: &[Repo]| {
            let text = credential_properties(repos, &|_| None, &coursier, &mut Vec::new());
            crate::credentials::CoursierCredentials { entries: crate::credentials::parse_properties(&text) }
        };
        let closed = parse(&[repo("http://nexus.local:8081/m/", false)]);
        assert!(closed.find("http://nexus.local:8081/m/x.jar").is_none(), "no opt-in: HTTPS only");
        assert_eq!(closed.find("https://nexus.local/m/x.jar").unwrap().user, "alice");
        assert_eq!(closed.for_challenge("https://secure.example/x", Some("R")).unwrap().user, "bob");
        let open = parse(&[repo("http://nexus.local:8081/m/", true)]);
        assert_eq!(open.find("http://nexus.local:8081/m/x.jar").unwrap().user, "alice");
        assert!(open.find("http://nexus.local:8082/m/x.jar").is_none(), "only the opted-in origin");
    }

    #[test]
    fn literal_credentials_are_rejected() {
        let err = repo_with(Credentials::Bearer("hunter2".into())).unwrap_err().to_string();
        assert!(err.contains("env:VARIABLE") && !err.contains("hunter2"), "{err}");
        let err = repo_with(Credentials::Bearer("env:".into())).unwrap_err().to_string();
        assert!(err.contains("without a variable name"), "{err}");
    }

    #[test]
    fn credentials_are_read_only_when_needed() {
        // Unset variables are fine until a request needs them.
        let var = "SCPM_TEST_SURELY_UNSET_TOKEN";
        let repos = repo_with(Credentials::Bearer(format!("env:{var}"))).unwrap();
        assert_eq!(repos[0].configured_user().as_deref(), Some("token"));
        let err = repos[0].auth().unwrap_err().to_string();
        assert!(err.contains(var) && err.contains("not set"), "{err}");
        let repos =
            repo_with(Credentials::Basic { user: format!("env:{var}_U"), password: format!("env:{var}_P") }).unwrap();
        assert_eq!(repos[0].configured_user(), None);
    }

    #[test]
    fn auth_debug_redacts_secrets() {
        let basic = Auth::Basic { user: "alice".into(), password: "hunter2".into() };
        let bearer = Auth::Bearer("tok3n".into());
        let shown = format!("{basic:?} {bearer:?}");
        assert!(shown.contains("alice") && !shown.contains("hunter2") && !shown.contains("tok3n"), "{shown}");
    }

    #[test]
    fn credential_file_never_reaches_argv() {
        let mut cmd = Command::new("cs");
        let repos = vec![Repo {
            id: "r".into(),
            url: "http://127.0.0.1:8080/m/".into(),
            credentials: None,
            allow_insecure_credentials: false,
            ..Default::default()
        }];
        let common = add_common_args(&mut cmd, &repos, Path::new("/c")).unwrap();
        let file = common._credential_file.as_ref().unwrap();
        assert!(!fs::read_to_string(file.path()).unwrap().contains("scpm0"));
        let env = |name: &str| cmd.get_envs().find(|(k, _)| *k == name).and_then(|(_, v)| v);
        let value = env("COURSIER_CREDENTIALS").unwrap();
        assert_eq!(value, credentials_env_value(file.path()).as_str());
        if cfg!(unix) {
            assert_eq!(value, file.path());
        }
        assert!(!cmd.get_args().any(|a| a == "--credential-file" || a == "--credentials"));
        // The Scala CLI config (whose credentials skip scpm's policy) points at nothing.
        let scala_cli = Path::new(env("SCALA_CLI_CONFIG").unwrap());
        assert!(scala_cli.starts_with(common._sealed_dir.as_ref().unwrap().path()));
        assert!(!scala_cli.exists());
    }

    #[test]
    fn cs_runs_sealed() {
        let mut cmd = Command::new("cs");
        let repos = vec![Repo {
            id: "r".into(),
            url: "https://repo.example/m/".into(),
            credentials: None,
            allow_insecure_credentials: false,
            ..Default::default()
        }];
        let common = add_common_args(&mut cmd, &repos, Path::new("/c")).unwrap();
        let sealed = common._sealed_dir.as_ref().unwrap().path();
        let envs: BTreeMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect();
        // A `COURSIER_MIRRORS` naming a missing file disables every mirror file.
        let mirrors = PathBuf::from(envs["COURSIER_MIRRORS"].as_deref().unwrap());
        assert!(mirrors.starts_with(sealed) && !mirrors.exists());
        assert_eq!(envs["COURSIER_MAVEN_SETTINGS"].as_deref(), Some("false"));
        for var in UNSEALED_ENV {
            assert_eq!(envs.get(*var), Some(&None), "{var} is removed");
        }
        assert!(envs["SCALA_CLI_CONFIG"].is_some() && envs["COURSIER_CREDENTIALS"].is_some());
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.iter().any(|a| a == "--no-default"));
        assert_eq!(args.iter().filter(|a| *a == "--repository").count(), 1);
    }

    #[test]
    fn windows_credential_paths_become_valid_file_urls() {
        assert_eq!(
            credentials_env_value(Path::new(r"C:\Users\Ann Lee\AppData\Local\Temp\scpm-credentials-x.properties")),
            "file:///C:/Users/Ann%20Lee/AppData/Local/Temp/scpm-credentials-x.properties"
        );
        assert_eq!(credentials_env_value(Path::new(r"C:\T\é#%.p")), "file:///C:/T/%C3%A9%23%25.p");
        assert_eq!(credentials_env_value(Path::new("/tmp/a b")), "/tmp/a b");
    }

    #[test]
    fn property_escaping() {
        assert_eq!(escape_property("a=b:c\\d#"), "a\\=b\\:c\\\\d\\#");
        let parsed = crate::credentials::parse_properties(&format!(
            "x.host=h\nx.username=u\nx.password={}\n",
            escape_property("p=a:s#s\\")
        ));
        assert_eq!(parsed[0].password, "p=a:s#s\\");
        for value in ["\tpw", "\u{c}pw", " pw", "p\tw"] {
            let parsed = crate::credentials::parse_properties(&format!(
                "x.host=h\nx.username=u\nx.password={}\n",
                escape_property(value)
            ));
            assert_eq!(parsed[0].password, value, "{value:?}");
        }
    }

    #[test]
    fn artifact_types_follow_the_roots() {
        let req = ResolveRequest {
            dependencies: vec![
                "org.slf4j:slf4j-api:2.0.17,classifier=sources".into(),
                "com.google.protobuf:protoc:3.25.1,classifier=linux-x86_64,type=exe".into(),
                "g:bom:1,type=pom".into(),
            ],
            intransitive: vec!["g:a:1,classifier=javadoc".into()],
            ..Default::default()
        };
        assert_eq!(artifact_types(&req), ["jar", "bundle", "test-jar", "src", "exe", "doc"]);
        assert_eq!(dependency_parts("g:a:1,exclude=x%y,classifier=c,type=t"), ("g", "a", Some("c"), Some("t")));
    }

    #[test]
    fn reverse_trees_become_chains() {
        let tree = "\u{1b}[0m└─ org.typelevel:cats-kernel_3:2.13.0\n\
                    \x20  └─ org.typelevel:cats-core_3:2.13.0\n\
                    \x20     ├─ io.circe:circe-core_3:0.14.14\n\
                    \x20     └─ org.typelevel:cats-effect-kernel_3:3.5.7 \u{1b}[31morg.typelevel:cats-core_3:2.9.0 -> 2.13.0\u{1b}[0m\n\
                    \x20        ├─ org.typelevel:cats-effect-std_3:3.5.7\n\
                    \x20        │  └─ org.typelevel:cats-effect_3:3.5.7\n\
                    \x20        └─ org.typelevel:cats-effect_3:3.5.7\n";
        let roots = ["org.typelevel:cats-core_3".to_string()];
        let chains = reverse_tree_chains(tree, &roots);
        assert_eq!(
            chains,
            vec![
                vec!["org.typelevel:cats-core_3:2.13.0", "org.typelevel:cats-kernel_3:2.13.0"],
                vec![
                    "io.circe:circe-core_3:0.14.14",
                    "org.typelevel:cats-core_3:2.13.0",
                    "org.typelevel:cats-kernel_3:2.13.0"
                ],
                vec![
                    "org.typelevel:cats-effect_3:3.5.7",
                    "org.typelevel:cats-effect-std_3:3.5.7",
                    "org.typelevel:cats-effect-kernel_3:3.5.7",
                    "org.typelevel:cats-core_3:2.13.0",
                    "org.typelevel:cats-kernel_3:2.13.0"
                ],
                vec![
                    "org.typelevel:cats-effect_3:3.5.7",
                    "org.typelevel:cats-effect-kernel_3:3.5.7",
                    "org.typelevel:cats-core_3:2.13.0",
                    "org.typelevel:cats-kernel_3:2.13.0"
                ],
            ]
        );
        assert_eq!(reverse_tree_chains("└─ g:a:1\n", &[]), vec![vec!["g:a:1"]]);
        assert!(reverse_tree_chains("", &[]).is_empty());
    }
}
