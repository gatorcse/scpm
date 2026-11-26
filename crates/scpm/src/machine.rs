//! Machine-local settings (design §5.9): credentials, mirrors and proxies that decide how the
//! bytes are reached, never what is resolved. They come from scpm's user config
//! (`<config dir>/scpm/config.yaml`, or the file `SCPM_CONFIG` names) and the standard proxy
//! variables, and both download paths (`cs` and the Rust fetcher) get the same resolved values.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::coursier::{self, CredentialsFrom, Mirror, Repo, RepoCredentials, Secret};
use crate::diag::{Diagnostic, Diagnostics};

/// Names the user config file, replacing the default location.
pub const USER_CONFIG_ENV: &str = "SCPM_CONFIG";

/// The user config file as written.
#[derive(Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(title = "scpm user config")]
pub struct UserConfigFile {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Credentials, matched against a repository's (or its mirror's) URL by scheme, host and
    /// port; a path narrows the match to URLs under it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<CredentialEntry>,
    /// Mirrors, applied at download time and never recorded in the lock.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<MirrorEntry>,
    /// The proxy for every download. Set, it replaces `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyEntry>,
}

/// `{url, token}` (bearer) or `{url, user, password}` (basic auth). Each value is `env:VAR`,
/// `file:PATH` (`~` is the home directory) or the value itself.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialEntry {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MirrorEntry {
    pub url: String,
    /// Repository ids, URL prefixes, or `*` for every repository.
    pub mirror_of: Vec<String>,
}

#[derive(Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyEntry {
    /// Proxy for `https://` URLs, `http://host:port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https: Option<String>,
    /// Proxy for `http://` URLs, `http://host:port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    /// Hosts reached directly: a name also covers its subdomains (`.example.com` or
    /// `example.com`), `*` covers every host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_proxy: Vec<String>,
}

/// One user config credential, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct UserCredential {
    /// As written.
    pub url: String,
    target: UrlParts,
    pub credentials: RepoCredentials,
}

/// One user config mirror, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct UserMirror {
    /// Ends with `/`.
    pub url: String,
    pub mirror_of: Vec<String>,
}

/// The machine settings in effect.
#[derive(Debug, Clone)]
pub struct Machine {
    /// The user config file (which may not exist).
    pub path: Option<PathBuf>,
    pub exists: bool,
    pub credentials: Vec<UserCredential>,
    pub mirrors: Vec<UserMirror>,
    /// The proxy both download paths use. A problem with one from the environment is only an
    /// error once something must be downloaded with that scheme.
    pub proxy: ProxyChoice,
    /// Problems worth a warning (e.g. a user config with secrets that others can read).
    pub warnings: Vec<String>,
}

impl Machine {
    /// No user config.
    pub fn none() -> Machine {
        Machine {
            path: None,
            exists: false,
            credentials: Vec::new(),
            mirrors: Vec::new(),
            proxy: None,
            warnings: Vec::new(),
        }
    }

    /// Takes the proxy from `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`.
    fn with_env_proxy(mut self) -> Machine {
        self.proxy = proxy_from_env(&|var| std::env::var(var).ok());
        self
    }

    /// Reads the user config at `path`; a missing file means no settings.
    pub fn from_file(path: &Path) -> Result<Machine, Diagnostics> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Machine { path: Some(path.to_path_buf()), ..Machine::none() }.with_env_proxy());
            }
            Err(e) => return Err(Diagnostic::new(format!("cannot read the user config: {e}")).in_file(path).into()),
        };
        let file = parse(path, &text)?;
        let base = path.parent().unwrap_or(Path::new("."));
        let mut machine = Machine::from_config(&file, path, base)?;
        machine.exists = true;
        machine.path = Some(path.to_path_buf());
        if machine.has_literal_secrets()
            && let Some(mode) = group_or_other_readable(path)
        {
            machine.warnings.push(format!(
                "the user config {} holds literal secrets and is readable by others (mode {mode:o}); run `chmod 600` on it, or use `env:` or `file:` values",
                path.display()
            ));
        }
        Ok(machine)
    }

    /// Validates a parsed user config; `file` names it in diagnostics, `base` resolves relative
    /// `file:` paths.
    pub fn from_config(config: &UserConfigFile, file: &Path, base: &Path) -> Result<Machine, Diagnostics> {
        let mut diags = Diagnostics::default();
        let mut err = |key: String, message: String| diags.push(Diagnostic::new(message).in_file(file).at_key(key));
        let mut credentials = Vec::new();
        for (i, c) in config.credentials.iter().enumerate() {
            let key = format!("credentials[{i}]");
            let target = match check_url(&c.url) {
                Ok(t) => t,
                Err(m) => {
                    err(format!("{key}.url"), m);
                    continue;
                }
            };
            let mut secret = |name: &str, value: &str| match parse_secret(value, base) {
                Ok(s) => Some(s),
                Err(m) => {
                    err(format!("{key}.{name}"), m);
                    None
                }
            };
            let spec = match (&c.token, &c.user, &c.password) {
                (Some(token), None, None) => secret("token", token).map(|token| RepoCredentials::Bearer { token }),
                (None, Some(user), Some(password)) => match (secret("user", user), secret("password", password)) {
                    (Some(user), Some(password)) => Some(RepoCredentials::Basic { user, password }),
                    _ => None,
                },
                _ => {
                    err(key.clone(), "give either `token`, or `user` and `password`".to_string());
                    None
                }
            };
            if let Some(credentials_spec) = spec {
                credentials.push(UserCredential { url: c.url.clone(), target, credentials: credentials_spec });
            }
        }
        let mut mirrors = Vec::new();
        for (i, m) in config.mirrors.iter().enumerate() {
            let key = format!("mirrors[{i}]");
            if let Err(message) = check_url(&m.url) {
                err(format!("{key}.url"), message);
                continue;
            }
            if m.mirror_of.is_empty() || m.mirror_of.iter().any(|p| p.trim().is_empty()) {
                err(format!("{key}.mirrorOf"), "list repository ids, URL prefixes, or `*`".to_string());
                continue;
            }
            let url = if m.url.ends_with('/') { m.url.clone() } else { format!("{}/", m.url) };
            mirrors.push(UserMirror { url, mirror_of: m.mirror_of.iter().map(|p| p.trim().to_string()).collect() });
        }
        let proxy = match &config.proxy {
            Some(entry) => match proxy_from_config(entry) {
                Ok(p) => p,
                Err(errors) => {
                    for (key, message) in errors {
                        err(format!("proxy.{key}"), message);
                    }
                    None
                }
            },
            None => proxy_from_env(&|var| std::env::var(var).ok()),
        };
        diags.into_result(Machine {
            path: Some(file.to_path_buf()),
            exists: true,
            credentials,
            mirrors,
            proxy,
            warnings: Vec::new(),
        })
    }

    fn has_literal_secrets(&self) -> bool {
        self.credentials.iter().any(|c| match &c.credentials {
            RepoCredentials::Bearer { token } => matches!(token, Secret::Literal(_)),
            RepoCredentials::Basic { password, .. } => matches!(password, Secret::Literal(_)),
        })
    }

    /// The user config credential for `url`: the entry with the longest matching path.
    pub fn credential_for(&self, url: &str) -> Option<&UserCredential> {
        let target = UrlParts::parse(url)?;
        self.credentials.iter().filter(|c| c.target.covers(&target)).max_by_key(|c| c.target.path.len())
    }

    /// The mirror for `repo`: the first whose `mirrorOf` names its id or a prefix of its URL,
    /// else the first for `*`.
    pub fn mirror_for(&self, repo: &Repo) -> Option<Mirror> {
        let named = |p: &str| {
            if p.contains("://") {
                let prefix = comparable_url(p);
                let prefix = if prefix.ends_with('/') { prefix } else { format!("{prefix}/") };
                comparable_url(&repo.url).starts_with(&prefix)
            } else {
                p == repo.id
            }
        };
        let found = self
            .mirrors
            .iter()
            .find_map(|m| m.mirror_of.iter().find(|p| *p != "*" && named(p)).map(|p| (m, p)))
            .or_else(|| self.mirrors.iter().find_map(|m| m.mirror_of.iter().find(|p| *p == "*").map(|p| (m, p))))?;
        (comparable_url(&found.0.url) != comparable_url(&repo.url))
            .then(|| Mirror { url: found.0.url.clone(), matched: found.1.clone() })
    }

    /// Applies the machine settings to the workspace's repositories (design §5.9):
    /// - a mirror replaces the repository's URL for downloads (never in the lock);
    /// - a mirrored repository's workspace credentials are not sent to the mirror;
    /// - a repository without workspace credentials gets the user config's for its download URL.
    ///   Over plain HTTP that needs the repository's `allowInsecureCredentials`.
    pub fn apply(&self, mut repos: Vec<Repo>) -> Vec<Repo> {
        for repo in &mut repos {
            repo.mirror = self.mirror_for(repo);
            if repo.mirror.is_some() && repo.credentials_from == CredentialsFrom::Workspace {
                repo.credentials = None;
            }
        }
        let insecure = coursier::insecure_credential_origins(&repos);
        for repo in &mut repos {
            if repo.credentials.is_some() {
                continue;
            }
            if let Some(c) = self.user_credential(repo, &insecure) {
                repo.credentials = Some(c.credentials.clone());
                repo.credentials_from = CredentialsFrom::UserConfig { url: c.url.clone() };
            }
        }
        repos
    }

    /// The user config credential that applies to `repo`, if the plain-HTTP policy allows it.
    fn user_credential(&self, repo: &Repo, insecure_origins: &[String]) -> Option<&UserCredential> {
        let c = self.credential_for(repo.fetch_url())?;
        let allowed = !coursier::is_plain_http(repo.fetch_url())
            || repo.origin().is_some_and(|o| insecure_origins.iter().any(|i| i.eq_ignore_ascii_case(o)));
        allowed.then_some(c)
    }

    /// Why a user config credential matching `repo` is not used, if one is skipped.
    pub fn skipped_credential(&self, repo: &Repo, repos: &[Repo]) -> Option<String> {
        if repo.credentials.is_some() && repo.credentials_from == CredentialsFrom::Workspace {
            return None;
        }
        let c = self.credential_for(repo.fetch_url())?;
        let insecure = coursier::insecure_credential_origins(repos);
        if self.user_credential(repo, &insecure).is_some() {
            return None;
        }
        Some(format!(
            "the user config entry for {} is not sent over plain HTTP without the repository's `allowInsecureCredentials`",
            c.url
        ))
    }
}

/// The resolved proxy, if any. Problems with a proxy from the environment are kept per scheme in
/// [`ProxySettings::errors`].
pub type ProxyChoice = Option<ProxySettings>;

/// A proxy server, reached over plain HTTP (`http://host:port`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProxyServer {
    pub host: String,
    pub port: u16,
}

impl ProxyServer {
    pub fn url(&self) -> String {
        if self.host.contains(':') {
            format!("http://[{}]:{}", self.host, self.port)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }
}

/// The proxy settings both download paths get (design §5.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettings {
    /// For `https://` URLs.
    pub https: Option<ProxyServer>,
    /// For `http://` URLs.
    pub http: Option<ProxyServer>,
    /// Hosts reached directly, with their subdomains; `*` is every host. Loopback is always here.
    pub no_proxy: Vec<String>,
    /// `noProxy` entries that the two paths can't apply alike (address ranges, wildcards other
    /// than a leading `*.`), dropped from both.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_no_proxy: Vec<String>,
    /// Why the environment's proxy for a scheme (`https`, `http`) can't be used. Only requests of
    /// that scheme, to hosts `noProxy` doesn't cover, fail.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<String, String>,
    /// `user config`, or the environment variables it was read from.
    pub source: String,
}

/// Loopback never goes through a proxy (Java's default `http.nonProxyHosts`, made explicit so
/// both paths agree).
const LOOPBACK: &[&str] = &["localhost", "127.0.0.1", "::1"];

impl ProxySettings {
    fn new(https: Option<ProxyServer>, http: Option<ProxyServer>, entries: &[String], source: String) -> Self {
        let mut no_proxy: Vec<String> = Vec::new();
        let mut ignored_no_proxy = Vec::new();
        for e in entries.iter().map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty()) {
            // `*.corp.com` means what `.corp.com` does, to both paths: the domain and its
            // subdomains (reqwest-style matching knows no other wildcard).
            let e = match e.strip_prefix("*.") {
                Some(rest) => format!(".{rest}"),
                None => e,
            };
            let e = match e.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
                Some(v6) => v6.to_string(),
                None => e,
            };
            let domain = e.trim_start_matches('.');
            let plain = e == "*"
                || (!domain.is_empty()
                    && domain.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':')));
            if !plain {
                ignored_no_proxy.push(e);
            } else if !no_proxy.contains(&e) {
                no_proxy.push(e);
            }
        }
        for l in LOOPBACK {
            if !no_proxy.iter().any(|e| e == l) {
                no_proxy.push(l.to_string());
            }
        }
        ProxySettings { https, http, no_proxy, ignored_no_proxy, errors: BTreeMap::new(), source }
    }

    /// Whether `host` is reached directly.
    pub fn bypasses(&self, host: &str) -> bool {
        let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
        self.no_proxy.iter().any(|e| {
            let domain = e.trim_start_matches('.');
            e == "*" || host == domain || host.ends_with(&format!(".{domain}"))
        })
    }

    /// How a request to `url` goes out: through a proxy, directly (`None`), or not at all
    /// because the environment's proxy for its scheme is unusable. Both download paths and
    /// `scpm config explain` decide with this.
    pub fn route(&self, url: &str) -> Result<Option<&ProxyServer>, String> {
        let Some(parts) = UrlParts::parse(url) else { return Ok(None) };
        if !matches!(parts.scheme.as_str(), "https" | "http") || self.bypasses(&parts.host) {
            return Ok(None);
        }
        if let Some(e) = self.errors.get(&parts.scheme) {
            return Err(e.clone());
        }
        Ok(if parts.scheme == "https" { self.https.as_ref() } else { self.http.as_ref() })
    }

    /// The proxy a request to `url` goes through, if any (direct when its scheme's proxy is
    /// unusable; callers check [`Self::route`] first).
    pub fn for_url(&self, url: &str) -> Option<&ProxyServer> {
        self.route(url).ok().flatten()
    }

    /// The settings as `cs` JVM options. `http.nonProxyHosts` covers both schemes; a
    /// `noProxy` domain becomes `domain|*.domain`.
    pub fn jvm_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for (scheme, server) in [("https", &self.https), ("http", &self.http)] {
            if let Some(s) = server {
                args.push(format!("-J-D{scheme}.proxyHost={}", s.host));
                args.push(format!("-J-D{scheme}.proxyPort={}", s.port));
            }
        }
        if !args.is_empty() {
            args.push(format!("-J-Dhttp.nonProxyHosts={}", self.non_proxy_hosts()));
        }
        args
    }

    /// `noProxy` in the JVM's `http.nonProxyHosts` form.
    fn non_proxy_hosts(&self) -> String {
        let mut hosts: Vec<String> = Vec::new();
        for e in &self.no_proxy {
            let domain = e.trim_start_matches('.');
            if e == "*" || domain.parse::<std::net::IpAddr>().is_ok() {
                hosts.push(domain.to_string());
                if domain.contains(':') {
                    hosts.push(format!("[{domain}]"));
                }
            } else {
                hosts.push(domain.to_string());
                hosts.push(format!("*.{domain}"));
            }
        }
        hosts.join("|")
    }
}

/// `http://host[:port]` (or `host[:port]`); the port defaults to 80. Proxy authentication is not
/// supported (design §5.9), so a URL with a user is an error. No error repeats the value, which
/// may hold a password that didn't parse as one (`http://alice:pa#ss@proxy:3128`).
fn parse_proxy_url(value: &str) -> Result<ProxyServer, String> {
    let value = value.trim();
    let (scheme, rest) = value.split_once("://").unwrap_or(("http", value));
    // Anywhere, not just before the first `/`, `?` or `#`: a password may contain those.
    if rest.contains('@') {
        return Err("proxy authentication is not supported yet; remove the user and password from the proxy URL".into());
    }
    if !scheme.eq_ignore_ascii_case("http") {
        let safe =
            scheme.len() <= 16 && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        let what = if safe { format!("`{scheme}://` proxies are") } else { "proxies other than `http://` are".into() };
        return Err(format!(
            "{what} not supported; both download paths reach the proxy over plain HTTP (`http://host:port`)"
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let valid_host =
        |h: &str| !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'));
    let parts = UrlParts::parse(&format!("http://{authority}"))
        .filter(|p| valid_host(&p.host))
        .ok_or("the value is not a proxy URL; write `http://host:port`")?;
    Ok(ProxyServer { host: parts.host, port: parts.port.unwrap_or(80) })
}

/// The user config's `proxy`: errors are `(key, message)`. No server means no proxy, whatever
/// the environment says.
fn proxy_from_config(entry: &ProxyEntry) -> Result<Option<ProxySettings>, Vec<(String, String)>> {
    let mut errors = Vec::new();
    let mut server = |key: &str, value: &Option<String>| match value.as_deref().map(parse_proxy_url) {
        Some(Ok(s)) => Some(s),
        Some(Err(m)) => {
            errors.push((key.to_string(), m));
            None
        }
        None => None,
    };
    let (https, http) = (server("https", &entry.https), server("http", &entry.http));
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok((https.is_some() || http.is_some())
        .then(|| ProxySettings::new(https, http, &entry.no_proxy, "user config".into())))
}

/// `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` (upper case first, then lower case). A bad value
/// only makes its own scheme unusable.
fn proxy_from_env(env: &dyn Fn(&str) -> Option<String>) -> ProxyChoice {
    let mut used = Vec::new();
    let mut read = |name: &str| {
        [name.to_string(), name.to_ascii_lowercase()].into_iter().find_map(|var| {
            let value = env(&var).filter(|v| !v.trim().is_empty())?;
            used.push(var.clone());
            Some((var, value))
        })
    };
    let (https, http, no_proxy) = (read("HTTPS_PROXY"), read("HTTP_PROXY"), read("NO_PROXY"));
    if https.is_none() && http.is_none() {
        return None;
    }
    let source = format!("environment ({})", used.join(", "));
    let mut errors = BTreeMap::new();
    let mut parse = |value: Option<(String, String)>, scheme: &str| match value {
        Some((var, v)) => match parse_proxy_url(&v) {
            Ok(s) => Some(s),
            Err(m) => {
                errors.insert(scheme.to_string(), format!("{var}: {m}"));
                None
            }
        },
        None => None,
    };
    let https = parse(https, "https");
    let http = parse(http, "http");
    let entries: Vec<String> = no_proxy.map(|(_, v)| v).unwrap_or_default().split(',').map(str::to_string).collect();
    let mut settings = ProxySettings::new(https, http, &entries, source);
    settings.errors = errors;
    Some(settings)
}

/// The user config path: `SCPM_CONFIG`, else `<config dir>/scpm/config.yaml`.
pub fn user_config_path() -> Option<PathBuf> {
    named_config_path().or_else(|| dirs::config_dir().map(|d| d.join("scpm").join("config.yaml")))
}

/// The file `SCPM_CONFIG` names, if set.
fn named_config_path() -> Option<PathBuf> {
    std::env::var_os(USER_CONFIG_ENV).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The machine settings for this process, read once.
pub fn current() -> Result<&'static Machine, Diagnostics> {
    static MACHINE: OnceLock<Result<Machine, Diagnostics>> = OnceLock::new();
    MACHINE.get_or_init(|| load(named_config_path(), user_config_path())).as_ref().map_err(Clone::clone)
}

/// Reads the user config at `path`. `named` is the file `SCPM_CONFIG` names: a typo there would
/// otherwise silently mean no settings.
fn load(named: Option<PathBuf>, path: Option<PathBuf>) -> Result<Machine, Diagnostics> {
    let Some(path) = path else { return Ok(Machine::none().with_env_proxy()) };
    let mut machine = Machine::from_file(&path)?;
    if !machine.exists && named.is_some_and(|n| n == path) {
        machine
            .warnings
            .push(format!("{USER_CONFIG_ENV} names {}, which does not exist; no user config is used", path.display()));
    }
    Ok(machine)
}

fn parse(path: &Path, text: &str) -> Result<UserConfigFile, Diagnostics> {
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).map_err(|e| Diagnostic::from_yaml_syntax(path, e))?;
    if raw.is_null() {
        return Ok(UserConfigFile::default());
    }
    let de = serde_yaml_ng::Deserializer::from_str(text);
    serde_path_to_error::deserialize(de).map_err(|e| Diagnostic::from_yaml(path, e).into())
}

/// `env:VAR`, `file:PATH` or a literal value.
fn parse_secret(value: &str, base: &Path) -> Result<Secret, String> {
    if let Some(var) = value.strip_prefix("env:") {
        let var = var.trim();
        if var.is_empty() {
            return Err("`env:` needs a variable name".into());
        }
        return Ok(Secret::Env(var.to_string()));
    }
    if let Some(path) = value.strip_prefix("file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err("`file:` needs a path".into());
        }
        let expanded = match path.strip_prefix("~/").or_else(|| (path == "~").then_some("")) {
            Some(rest) => dirs::home_dir().ok_or("cannot expand `~`: no home directory")?.join(rest),
            None => PathBuf::from(path),
        };
        return Ok(Secret::File(if expanded.is_absolute() { expanded } else { base.join(expanded) }));
    }
    Ok(Secret::Literal(value.to_string()))
}

/// A URL's scheme (lower-cased), host (lower-cased), effective port and path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlParts {
    scheme: String,
    host: String,
    port: Option<u16>,
    /// Empty, or starting and ending with `/`.
    path: String,
}

impl UrlParts {
    fn parse(url: &str) -> Option<UrlParts> {
        let (scheme, rest) = url.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
        let (host, port) = match authority.strip_prefix('[') {
            Some(v6) => {
                let (host, after) = v6.split_once(']')?;
                (host, after.strip_prefix(':'))
            }
            None => match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            },
        };
        let port = match port {
            Some(p) => Some(p.parse().ok()?),
            None => match scheme.as_str() {
                "https" => Some(443),
                "http" => Some(80),
                _ => None,
            },
        };
        let path = tail.split(['?', '#']).next().unwrap_or_default();
        let path = if path.is_empty() || path == "/" {
            String::new()
        } else if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        Some(UrlParts { scheme, host: host.to_ascii_lowercase(), port, path })
    }

    /// Same scheme, host and port, and `other`'s path is under this one's.
    fn covers(&self, other: &UrlParts) -> bool {
        let other_path = if other.path.is_empty() { "/" } else { other.path.as_str() };
        self.scheme == other.scheme
            && self.host == other.host
            && self.port == other.port
            && (self.path.is_empty() || other_path.starts_with(&self.path))
    }
}

/// `url` lower-cased and without a default port (`:443` for `https`, `:80` for `http`), so
/// `mirrorOf` URL prefixes match however the port is written.
fn comparable_url(url: &str) -> String {
    let url = url.to_ascii_lowercase();
    let Some((scheme, rest)) = url.split_once("://") else { return url };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let default = match scheme {
        "https" => ":443",
        "http" => ":80",
        _ => return url.clone(),
    };
    let authority = authority.strip_suffix(default).unwrap_or(authority);
    format!("{scheme}://{authority}{tail}")
}

/// An `http(s)://host[:port][/path]` URL without credentials in it. No error repeats the URL:
/// a password that doesn't parse as one (`https://alice:pa#ss@host`) would be shown.
fn check_url(url: &str) -> Result<UrlParts, String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err("not a URL; write `https://host/…`".into());
    };
    // Anywhere, not just before the first `/`, `?` or `#`: a password may contain those.
    if rest.contains('@') {
        return Err(
            "the URL must not contain credentials or any other `@`; use `user` and `password`, or `token`".into()
        );
    }
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return Err("only http:// and https:// URLs are supported".into());
    }
    match UrlParts::parse(url) {
        Some(parts) if !parts.host.is_empty() => Ok(parts),
        _ => Err("the URL has no valid host and port".into()),
    }
}

/// The file's permission bits when group or others may read it (Unix only).
#[cfg(unix)]
fn group_or_other_readable(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & 0o044 != 0).then_some(mode)
}

#[cfg(not(unix))]
fn group_or_other_readable(_path: &Path) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(yaml: &str) -> Result<Machine, Diagnostics> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();
        Machine::from_file(&path)
    }

    #[test]
    fn missing_or_empty_files_mean_no_settings() {
        let dir = tempfile::tempdir().unwrap();
        let m = Machine::from_file(&dir.path().join("nope.yaml")).unwrap();
        assert!(!m.exists && m.credentials.is_empty() && m.mirrors.is_empty());
        let m = machine("# nothing yet\n").unwrap();
        assert!(m.exists && m.credentials.is_empty());
    }

    #[test]
    fn a_missing_file_named_by_scpm_config_is_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("confg.yaml");
        let m = load(Some(missing.clone()), Some(missing.clone())).unwrap();
        assert!(!m.exists);
        assert!(m.warnings.iter().any(|w| w.contains("SCPM_CONFIG") && w.contains("confg.yaml")), "{:?}", m.warnings);
        // The default location may well not exist: no warning.
        assert!(load(None, Some(missing)).unwrap().warnings.is_empty());
        let present = dir.path().join("config.yaml");
        std::fs::write(&present, "").unwrap();
        assert!(load(Some(present.clone()), Some(present)).unwrap().warnings.is_empty());
    }

    #[test]
    fn unknown_keys_and_bad_entries_are_errors() {
        let err = machine("mirror: []\n").unwrap_err().to_string();
        assert!(err.contains("mirror") && err.contains("mirrors"), "{err}");
        let err = machine("credentials:\n  - {url: 'https://h', tokn: 'env:X'}\n").unwrap_err().to_string();
        assert!(err.contains("tokn"), "{err}");
        let err = machine("credentials:\n  - {url: 'https://h', token: 'env:X', user: u, password: p}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("credentials[0]") && err.contains("either `token`"), "{err}");
        let err = machine("credentials:\n  - {url: 'https://a:b@h', token: 'env:X'}\n").unwrap_err().to_string();
        assert!(err.contains("must not contain credentials"), "{err}");
        let err = machine("credentials:\n  - {url: 'https://h', token: 'env:'}\n").unwrap_err().to_string();
        assert!(err.contains("credentials[0].token") && err.contains("variable name"), "{err}");
        let err = machine("mirrors:\n  - {url: 'ftp://m', mirrorOf: ['*']}\n").unwrap_err().to_string();
        assert!(err.contains("mirrors[0].url"), "{err}");
        let err = machine("mirrors:\n  - {url: 'https://m', mirrorOf: []}\n").unwrap_err().to_string();
        assert!(err.contains("mirrors[0].mirrorOf"), "{err}");
    }

    #[test]
    fn credentials_match_by_origin_and_path() {
        let m = machine(
            "credentials:\n\
             \x20 - {url: 'https://acme.example', token: 'env:ACME'}\n\
             \x20 - {url: 'https://acme.example/private', user: alice, password: 'file:~/.secrets/acme'}\n\
             \x20 - {url: 'http://nexus.local:8081/repo/', user: bob, password: hunter2}\n",
        )
        .unwrap();
        let find = |url: &str| m.credential_for(url).map(|c| c.url.as_str());
        assert_eq!(find("https://acme.example/maven/"), Some("https://acme.example"));
        assert_eq!(find("https://ACME.example:443/maven/"), Some("https://acme.example"));
        assert_eq!(find("https://acme.example/private/maven/"), Some("https://acme.example/private"));
        assert_eq!(find("https://acme.example/privateer/"), Some("https://acme.example"), "path segments");
        assert_eq!(find("https://acme.example:8443/maven/"), None, "another port");
        assert_eq!(find("http://acme.example/maven/"), None, "another scheme");
        assert_eq!(find("http://nexus.local:8081/repo/m/"), Some("http://nexus.local:8081/repo/"));
        assert_eq!(find("http://nexus.local:8081/other/"), None);
        let RepoCredentials::Basic { password: Secret::File(path), .. } = &m.credentials[1].credentials else {
            panic!("{:?}", m.credentials[1]);
        };
        assert!(path.is_absolute() && path.ends_with(".secrets/acme"), "{}", path.display());
    }

    #[test]
    fn user_credentials_fill_in_and_respect_the_http_policy() {
        let m = machine(
            "credentials:\n\
             \x20 - {url: 'https://acme.example', token: 'env:ACME'}\n\
             \x20 - {url: 'http://nexus.local:8081', user: bob, password: 'env:NEXUS'}\n",
        )
        .unwrap();
        let repo = |id: &str, url: &str, allow: bool| Repo {
            id: id.into(),
            url: url.into(),
            allow_insecure_credentials: allow,
            ..Default::default()
        };
        let declared = Repo {
            credentials: Some(RepoCredentials::Bearer { token: Secret::Env("DECLARED".into()) }),
            ..repo("declared", "https://acme.example/declared/", false)
        };
        let repos = m.apply(vec![
            repo("acme", "https://acme.example/maven/", false),
            declared.clone(),
            repo("closed", "http://nexus.local:8081/m/", false),
        ]);
        assert_eq!(repos[0].credentials, Some(RepoCredentials::Bearer { token: Secret::Env("ACME".into()) }));
        assert_eq!(repos[0].credentials_from, CredentialsFrom::UserConfig { url: "https://acme.example".into() });
        assert_eq!(repos[1], declared, "workspace credentials win");
        assert_eq!(repos[2].credentials, None, "plain HTTP without the opt-in");
        assert!(m.skipped_credential(&repos[2], &repos).unwrap().contains("allowInsecureCredentials"));
        let open = m.apply(vec![repo("open", "http://nexus.local:8081/m/", true)]);
        assert!(matches!(open[0].credentials, Some(RepoCredentials::Basic { .. })));

        // Missing variables are reported with where they were configured.
        let err = repos[0].auth().unwrap_err().to_string();
        assert!(err.contains("`ACME`") && err.contains("user config entry for https://acme.example"), "{err}");
    }

    #[test]
    fn mirrors_match_ids_then_url_prefixes_then_everything() {
        let m = machine(
            "mirrors:\n\
             \x20 - {url: 'https://all.example/m', mirrorOf: ['*']}\n\
             \x20 - {url: 'https://central.example/m/', mirrorOf: [central]}\n\
             \x20 - {url: 'https://acme-mirror.example/m', mirrorOf: ['https://ACME.example/maven']}\n\
             \x20 - {url: 'https://self.example/m', mirrorOf: [self]}\n\
             credentials:\n\
             \x20 - {url: 'https://acme.example', token: 'env:ACME'}\n\
             \x20 - {url: 'https://central.example', user: alice, password: 'env:CENTRAL'}\n",
        )
        .unwrap();
        let repo = |id: &str, url: &str| Repo { id: id.into(), url: url.into(), ..Default::default() };
        let declared = Repo {
            credentials: Some(RepoCredentials::Bearer { token: Secret::Env("DECLARED".into()) }),
            ..repo("declared", "https://declared.example/m/")
        };
        let repos = m.apply(vec![
            repo("central", "https://repo1.maven.org/maven2/"),
            repo("acme", "https://acme.example/maven/releases/"),
            repo("other", "https://other.example/m/"),
            repo("self", "https://self.example/m/"),
            declared,
        ]);
        let mirror = |i: usize| repos[i].mirror.as_ref().map(|m| (m.url.as_str(), m.matched.as_str()));
        assert_eq!(mirror(0), Some(("https://central.example/m/", "central")), "an id beats `*`");
        assert_eq!(mirror(1), Some(("https://acme-mirror.example/m/", "https://ACME.example/maven")));
        assert_eq!(mirror(2), Some(("https://all.example/m/", "*")));
        assert_eq!(mirror(3), None, "a mirror of itself is no mirror");
        assert_eq!(repos[0].fetch_url(), "https://central.example/m/");
        assert_eq!(repos[0].url, "https://repo1.maven.org/maven2/", "the lock still sees the repository");
        // Credentials are matched against the mirror's URL, not the repository's.
        assert!(matches!(&repos[0].credentials, Some(RepoCredentials::Basic { .. })));
        assert_eq!(repos[1].credentials, None, "acme's entry is not sent to its mirror");
        // Workspace credentials never go to a mirror (another host).
        assert_eq!(repos[4].mirror.as_ref().unwrap().url, "https://all.example/m/");
        assert_eq!(repos[4].credentials, None);
    }

    #[test]
    fn mirror_url_prefixes_ignore_default_ports() {
        let m = machine(
            "mirrors:\n\
             \x20 - {url: 'https://m1.example/m', mirrorOf: ['https://acme.example:443/maven']}\n\
             \x20 - {url: 'https://m2.example/m', mirrorOf: ['http://plain.example/m']}\n\
             \x20 - {url: 'https://m3.example/m', mirrorOf: ['https://other.example/m']}\n",
        )
        .unwrap();
        let repo = |id: &str, url: &str| Repo { id: id.into(), url: url.into(), ..Default::default() };
        let mirror = |r: Repo| m.mirror_for(&r).map(|m| m.url);
        assert_eq!(mirror(repo("a", "https://acme.example/maven/x/")).as_deref(), Some("https://m1.example/m/"));
        assert_eq!(mirror(repo("p", "http://plain.example:80/m/")).as_deref(), Some("https://m2.example/m/"));
        assert_eq!(mirror(repo("o", "https://other.example:443/m/")).as_deref(), Some("https://m3.example/m/"));
        assert_eq!(mirror(repo("x", "https://acme.example:8443/maven/")), None, "another port");
        assert_eq!(mirror(repo("y", "http://acme.example:443/maven/")), None, "443 is no default for http");
        assert_eq!(comparable_url("HTTPS://H.example:443"), "https://h.example");
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn proxies_come_from_the_environment() {
        assert_eq!(proxy_from_env(&env(&[])), None);
        assert_eq!(proxy_from_env(&env(&[("NO_PROXY", "x"), ("HTTPS_PROXY", " ")])), None, "blank is unset");
        let p = proxy_from_env(&env(&[
            ("HTTPS_PROXY", "http://proxy.example:3128"),
            ("https_proxy", "http://ignored.example:1"),
            ("http_proxy", "proxy.example"),
            ("no_proxy", " .corp.example, 10.0.0.0/8,internal ,"),
        ]))
        .unwrap();
        assert_eq!(p.https, Some(ProxyServer { host: "proxy.example".into(), port: 3128 }));
        assert_eq!(p.http, Some(ProxyServer { host: "proxy.example".into(), port: 80 }));
        assert_eq!(p.no_proxy, [".corp.example", "internal", "localhost", "127.0.0.1", "::1"]);
        assert_eq!(p.ignored_no_proxy, ["10.0.0.0/8"]);
        assert_eq!(p.source, "environment (HTTPS_PROXY, http_proxy, no_proxy)");
        assert!(p.errors.is_empty());

        let p = proxy_from_env(&env(&[("HTTPS_PROXY", "http://alice:s3cret@proxy.example:3128")])).unwrap();
        let err = p.route("https://repo.example/m/").unwrap_err();
        assert!(err.contains("HTTPS_PROXY") && err.contains("authentication is not supported"), "{err}");
        assert!(!err.contains("s3cret"), "{err}");
        let p = proxy_from_env(&env(&[("http_proxy", "socks5://proxy.example:1080")])).unwrap();
        let err = p.route("http://repo.example/m/").unwrap_err();
        assert!(err.starts_with("http_proxy: ") && err.contains("`socks5://` proxies are not supported"), "{err}");
    }

    #[test]
    fn a_bad_environment_proxy_only_blocks_its_own_scheme() {
        let p = proxy_from_env(&env(&[
            ("HTTPS_PROXY", "http://proxy.example:3128"),
            ("HTTP_PROXY", "socks5://proxy.example:1080"),
            ("NO_PROXY", "plain.example"),
        ]))
        .unwrap();
        assert_eq!(
            p.route("https://repo.example/m/").unwrap().map(ProxyServer::url).as_deref(),
            Some("http://proxy.example:3128")
        );
        assert!(p.route("http://repo.example/m/").unwrap_err().contains("HTTP_PROXY"));
        assert_eq!(p.route("http://plain.example/m/"), Ok(None), "noProxy hosts need no proxy");
        assert_eq!(p.jvm_args()[..2], ["-J-Dhttps.proxyHost=proxy.example", "-J-Dhttps.proxyPort=3128"]);
        assert!(!p.jvm_args().iter().any(|a| a.contains("http.proxyHost")));
        // Both unusable: still nothing fails until a URL of either scheme is asked for.
        let p = proxy_from_env(&env(&[("HTTPS_PROXY", "socks5://p:1"), ("HTTP_PROXY", "socks5://p:1")])).unwrap();
        assert_eq!(p.errors.len(), 2);
        assert!(p.jvm_args().is_empty());
    }

    #[test]
    fn proxy_and_url_errors_never_repeat_the_value() {
        // A `#`, `?` or `/` in a password ends the authority early, so the `@` comes "later".
        for (value, secret) in [
            ("http://alice:pa#ss@proxy:3128", "pa#ss"),
            ("http://alice:pa?ss@proxy:3128", "pa?ss"),
            ("alice:pa/ss@proxy:3128", "pa/ss"),
        ] {
            let err = parse_proxy_url(value).unwrap_err();
            assert!(err.contains("authentication is not supported") && !err.contains(secret), "{value}: {err}");
            assert!(!err.contains("alice"), "{value}: {err}");
        }
        for value in ["http://pro xy:3128", "http://proxy:31x8", "http://:3128"] {
            let err = parse_proxy_url(value).unwrap_err();
            assert!(!err.contains("proxy:") && !err.contains("3128"), "{value}: {err}");
        }
        let err = parse_proxy_url("hunter2://proxy:1").unwrap_err();
        assert!(err.contains("`hunter2://` proxies"), "a plain scheme may be named: {err}");
        let err = parse_proxy_url("se cret://proxy:1").unwrap_err();
        assert!(!err.contains("cret"), "{err}");
        for (url, secret) in [
            ("https://alice:pa#ss@nexus.example/m/", "pa#ss"),
            ("https://alice:pa?ss@nexus.example/m/", "pa?ss"),
            ("https://tok/en@nexus.example/m/", "tok/en"),
            ("nexus.example:s3cret", "s3cret"),
            ("https://nexus.example:s3cret/m/", "s3cret"),
        ] {
            let err = check_url(url).unwrap_err();
            assert!(!err.contains(secret) && !err.contains("nexus"), "{url}: {err}");
        }
        let err = machine("credentials:\n  - {url: 'https://alice:pa#ss@h.example', token: 'env:X'}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("credentials[0].url") && !err.contains("pa#ss"), "{err}");
        let err =
            machine("mirrors:\n  - {url: 'https://u:pa#ss@m.example', mirrorOf: ['*']}\n").unwrap_err().to_string();
        assert!(err.contains("mirrors[0].url") && !err.contains("pa#ss"), "{err}");
    }

    #[test]
    fn the_user_config_proxy_replaces_the_environment() {
        let m = machine("proxy:\n  https: 'http://proxy.example:3128'\n  noProxy: [.example.com]\n").unwrap();
        let p = m.proxy.unwrap();
        assert_eq!(p.source, "user config");
        assert_eq!(p.http, None);
        // Set but without a server: no proxy at all, whatever the environment says.
        assert_eq!(machine("proxy: {noProxy: [x]}\n").unwrap().proxy, None);
        // In the user config, a bad proxy is a configuration error.
        let err = machine("proxy:\n  https: 'http://u:p@proxy.example:3128'\n").unwrap_err().to_string();
        assert!(err.contains("proxy.https") && err.contains("authentication"), "{err}");
    }

    /// The JVM's `http.nonProxyHosts` matching: `|`-separated, case-insensitive, `*` only at the
    /// start or the end (`sun.net.spi.DefaultProxySelector`). `host` as `URI.getHost` gives it.
    fn jvm_bypasses(non_proxy_hosts: &str, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        non_proxy_hosts.split('|').map(|d| d.trim().to_ascii_lowercase()).filter(|d| !d.is_empty()).any(|d| {
            if let Some(suffix) = d.strip_prefix('*') {
                host.ends_with(suffix)
            } else if let Some(prefix) = d.strip_suffix('*') {
                host.starts_with(prefix)
            } else {
                host == d
            }
        })
    }

    #[test]
    fn both_paths_bypass_the_same_real_hosts() {
        let entries: Vec<String> =
            ["*.corp.example", ".dot.example", "Internal", "10.1.2.3", "[fd00::1]", "corp.*", "a|b", "10.0.0.0/8", "*"]
                .map(String::from)
                .to_vec();
        let hosts = [
            "corp.example",
            "a.corp.example",
            "a.b.corp.example",
            "notcorp.example",
            "corp.example.evil",
            "corp.com",
            "dot.example",
            "x.dot.example",
            "internal",
            "a.internal",
            "internal.com",
            "10.1.2.3",
            "10.1.2.30",
            "10.0.0.1",
            "localhost",
            "[::1]",
            "[fd00::1]",
            "[fd00::2]",
            "127.0.0.1",
            "a",
            "b",
            "repo1.maven.org",
        ];
        let server = Some(ProxyServer { host: "proxy.example".into(), port: 3128 });
        let p = ProxySettings::new(server.clone(), None, &entries[..entries.len() - 1], "test".into());
        assert_eq!(
            p.no_proxy,
            [".corp.example", ".dot.example", "internal", "10.1.2.3", "fd00::1", "localhost", "127.0.0.1", "::1"]
        );
        assert_eq!(p.ignored_no_proxy, ["corp.*", "a|b", "10.0.0.0/8"]);
        let jvm = p.non_proxy_hosts();
        for host in hosts {
            let rust = p.for_url(&format!("https://{host}/m/")).is_none();
            assert_eq!(rust, p.bypasses(host), "{host}");
            assert_eq!(rust, jvm_bypasses(&jvm, host), "{host}: the fetcher and cs disagree ({jvm})");
        }
        for (host, direct) in
            [("a.corp.example", true), ("corp.example", true), ("notcorp.example", false), ("corp.com", false)]
        {
            assert_eq!(p.bypasses(host), direct, "{host}");
        }
        // `*` is every host, on both paths.
        let all = ProxySettings::new(server, None, &entries, "test".into());
        for host in hosts {
            assert!(all.for_url(&format!("https://{host}/")).is_none() && jvm_bypasses(&all.non_proxy_hosts(), host));
        }
    }

    #[test]
    fn no_proxy_matching_and_jvm_options_agree() {
        let p = ProxySettings::new(
            Some(ProxyServer { host: "proxy.example".into(), port: 3128 }),
            None,
            &[".corp.example".into(), "internal".into(), "10.1.2.3".into()],
            "test".into(),
        );
        assert_eq!(
            p.for_url("https://repo1.maven.org/maven2/").map(ProxyServer::url).as_deref(),
            Some("http://proxy.example:3128")
        );
        assert!(p.for_url("http://repo1.maven.org/maven2/").is_none(), "no http proxy");
        for direct in
            ["https://corp.example/m/", "https://a.corp.example/m/", "https://internal/m/", "https://127.0.0.1:8443/"]
        {
            assert!(p.for_url(direct).is_none(), "{direct}");
        }
        assert!(p.for_url("https://notcorp.example/m/").is_some());
        assert_eq!(
            p.jvm_args(),
            [
                "-J-Dhttps.proxyHost=proxy.example",
                "-J-Dhttps.proxyPort=3128",
                "-J-Dhttp.nonProxyHosts=corp.example|*.corp.example|internal|*.internal|10.1.2.3|localhost|*.localhost|127.0.0.1|::1|[::1]",
            ]
        );
    }

    #[test]
    fn file_secrets_drop_one_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pw"), "s3cret\n").unwrap();
        let config: UserConfigFile =
            serde_yaml_ng::from_str("credentials:\n  - {url: 'https://h.example', user: alice, password: 'file:pw'}\n")
                .unwrap();
        let m = Machine::from_config(&config, &dir.path().join("config.yaml"), dir.path()).unwrap();
        let repos = m.apply(vec![Repo { id: "h".into(), url: "https://h.example/m/".into(), ..Default::default() }]);
        let auth = repos[0].auth().unwrap().unwrap();
        assert_eq!(auth, coursier::Auth::Basic { user: "alice".into(), password: "s3cret".into() });
        assert_eq!(repos[0].configured_user().as_deref(), Some("alice"));
    }

    #[cfg(unix)]
    #[test]
    fn literal_secrets_in_a_readable_file_are_warned_about() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "credentials:\n  - {url: 'https://h', user: alice, password: hunter2}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let m = Machine::from_file(&path).unwrap();
        assert!(m.warnings.iter().any(|w| w.contains("chmod 600")), "{:?}", m.warnings);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Machine::from_file(&path).unwrap().warnings.is_empty());
        // `env:` and `file:` values are not secrets.
        std::fs::write(&path, "credentials:\n  - {url: 'https://h', user: alice, password: 'env:PW'}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Machine::from_file(&path).unwrap().warnings.is_empty());
    }
}
