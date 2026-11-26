//! Machine-local settings (design §5.9): credentials, mirrors and proxies that decide how the
//! bytes are reached, never what is resolved. They come from scpm's user config
//! (`<config dir>/scpm/config.yaml`, or the file `SCPM_CONFIG` names) and the standard proxy
//! variables, and both download paths (`cs` and the Rust fetcher) get the same resolved values.

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
    /// Problems worth a warning (e.g. a user config with secrets that others can read).
    pub warnings: Vec<String>,
}

impl Machine {
    /// No user config.
    pub fn none() -> Machine {
        Machine { path: None, exists: false, credentials: Vec::new(), mirrors: Vec::new(), warnings: Vec::new() }
    }

    /// Reads the user config at `path`; a missing file means no settings.
    pub fn from_file(path: &Path) -> Result<Machine, Diagnostics> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Machine { path: Some(path.to_path_buf()), ..Machine::none() });
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
        diags.into_result(Machine {
            path: Some(file.to_path_buf()),
            exists: true,
            credentials,
            mirrors,
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
                let prefix =
                    if p.ends_with('/') { p.to_ascii_lowercase() } else { format!("{}/", p.to_ascii_lowercase()) };
                repo.url.to_ascii_lowercase().starts_with(&prefix)
            } else {
                p == repo.id
            }
        };
        let found = self
            .mirrors
            .iter()
            .find_map(|m| m.mirror_of.iter().find(|p| *p != "*" && named(p)).map(|p| (m, p)))
            .or_else(|| self.mirrors.iter().find_map(|m| m.mirror_of.iter().find(|p| *p == "*").map(|p| (m, p))))?;
        (found.0.url != repo.url).then(|| Mirror { url: found.0.url.clone(), matched: found.1.clone() })
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

/// The user config path: `SCPM_CONFIG`, else `<config dir>/scpm/config.yaml`.
pub fn user_config_path() -> Option<PathBuf> {
    match std::env::var_os(USER_CONFIG_ENV).filter(|v| !v.is_empty()) {
        Some(path) => Some(PathBuf::from(path)),
        None => dirs::config_dir().map(|d| d.join("scpm").join("config.yaml")),
    }
}

/// The machine settings for this process, read once.
pub fn current() -> Result<&'static Machine, Diagnostics> {
    static MACHINE: OnceLock<Result<Machine, Diagnostics>> = OnceLock::new();
    MACHINE
        .get_or_init(|| match user_config_path() {
            Some(path) => Machine::from_file(&path),
            None => Ok(Machine::none()),
        })
        .as_ref()
        .map_err(Clone::clone)
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

/// An `http(s)://host[:port][/path]` URL without credentials in it.
fn check_url(url: &str) -> Result<UrlParts, String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!("`{url}` is not a URL; write `https://host/…`"));
    };
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return Err(format!("only http:// and https:// URLs are supported, not `{scheme}://`"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err("the URL must not contain credentials (`user@`); use `user` and `password`".into());
    }
    match UrlParts::parse(url) {
        Some(parts) if !parts.host.is_empty() => Ok(parts),
        _ => Err(format!("`{url}` has no valid host and port")),
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
