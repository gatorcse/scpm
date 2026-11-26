//! Coursier's own credential sources, read the way `cs` reads them (design §5.7), so the Rust
//! fetcher authenticates (and names cache directories) exactly like coursier for repositories
//! without configured `credentials`.
//!
//! - `COURSIER_CREDENTIALS`: a path (`/…` or `file:…`) to a properties file, or inline entries,
//!   one per line: `host[(realm)] user:password`;
//! - otherwise `<coursier config dir>/credentials.properties` and every
//!   `<coursier config dir>/credentials/*.properties`.
//!
//! Properties files hold `<prefix>.host`, `.username`, `.password`, and optionally `.realm`,
//! `.auto` (default `true`) and `.https-only` (default `true`).
//!
//! A `host` is `[scheme://]host[:port]`, matched as coursier 2.1.27's `DirectCredentials` does:
//! a bare host matches HTTPS URLs on any port (plain HTTP too if `https-only` is false); a port
//! restricts it to that port; a scheme restricts it to that scheme and port (the scheme's default
//! port if none), and `https-only` is then ignored.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// One credential entry.
#[derive(Clone, PartialEq, Eq)]
pub struct CoursierCredential {
    /// The scheme the entry is restricted to, if its host names one (lower-cased).
    pub scheme: Option<String>,
    /// Host name (IPv6 literals without brackets).
    pub host: String,
    /// The port the entry is restricted to, if its host names one.
    pub port: Option<u16>,
    pub user: String,
    pub password: String,
    /// Only sent once the server challenges with this realm.
    pub realm: Option<String>,
    /// Coursier matches the entry by host (and puts `user@` in cache paths when it applies up
    /// front). Coursier never uses `auto=false` entries for a repository URL without a user.
    pub auto: bool,
    /// Only sent over HTTPS (ignored when the host names a scheme).
    pub https_only: bool,
}

impl fmt::Debug for CoursierCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoursierCredential")
            .field("scheme", &self.scheme)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("realm", &self.realm)
            .field("auto", &self.auto)
            .field("https_only", &self.https_only)
            .finish()
    }
}

impl CoursierCredential {
    /// Whether the entry may be sent to `url`, judging from its scheme, host and port (realm
    /// and `auto` aside).
    pub fn applies_to(&self, url: &str) -> bool {
        UrlTarget::parse(url).is_some_and(|t| self.matches(&t))
    }

    /// Whether the entry may be sent to `target`, judging from its scheme, host and port.
    fn matches(&self, target: &UrlTarget) -> bool {
        let scheme_ok = match &self.scheme {
            Some(s) => *s == target.scheme,
            None => target.scheme == "https" || (target.scheme == "http" && !self.https_only),
        };
        let expected_port = self.port.or_else(|| self.scheme.as_deref().and_then(default_port));
        scheme_ok
            && self.host.eq_ignore_ascii_case(&target.host)
            && expected_port.is_none_or(|p| target.port == Some(p))
    }
}

/// All entries from coursier's credential sources.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoursierCredentials {
    pub entries: Vec<CoursierCredential>,
}

impl CoursierCredentials {
    /// Reads coursier's credential sources from the environment and its config directory.
    /// Unreadable or missing files are skipped, as coursier treats them as optional.
    pub fn load() -> CoursierCredentials {
        match std::env::var("COURSIER_CREDENTIALS") {
            Ok(value) if !value.trim().is_empty() => CoursierCredentials::from_env_value(&value),
            _ => {
                let mut entries = Vec::new();
                if let Some(dir) = config_dir() {
                    for file in default_files(&dir) {
                        entries.extend(read_file(&file));
                    }
                }
                CoursierCredentials { entries }
            }
        }
    }

    /// Interprets a `COURSIER_CREDENTIALS` value: a file path or inline entries.
    pub fn from_env_value(value: &str) -> CoursierCredentials {
        let trimmed = value.trim();
        let path = if let Some(rest) = trimmed.strip_prefix("file://") {
            Some(PathBuf::from(rest))
        } else if let Some(rest) = trimmed.strip_prefix("file:") {
            Some(PathBuf::from(rest))
        } else if trimmed.starts_with('/') {
            Some(PathBuf::from(trimmed))
        } else {
            None
        };
        let entries = match path {
            Some(p) => read_file(&p),
            // Not trimmed: inline passwords keep their trailing spaces.
            None => parse_inline(value),
        };
        CoursierCredentials { entries }
    }

    /// The entry coursier uses up front for `url` (`FileCache` picks `autoMatches(url, None)`):
    /// the first `auto` entry without a realm whose scheme, host and port match the URL.
    /// Realm-scoped entries wait for a challenge naming their realm ([`Self::for_challenge`]).
    pub fn find(&self, url: &str) -> Option<&CoursierCredential> {
        let target = UrlTarget::parse(url)?;
        self.entries.iter().find(|c| c.auto && c.realm.is_none() && c.matches(&target))
    }

    /// The entry coursier retries with after `url` answered a Basic challenge for `realm`: the
    /// first `auto` entry for the URL whose realm is unset or equal to the server's.
    pub fn for_challenge(&self, url: &str, realm: Option<&str>) -> Option<&CoursierCredential> {
        let target = UrlTarget::parse(url)?;
        self.entries
            .iter()
            .find(|c| c.auto && c.realm.as_deref().is_none_or(|r| Some(r) == realm) && c.matches(&target))
    }

    /// Every `auto` entry that may be sent to `url` after a challenge, in matching order: the
    /// first whose realm is unset or equal to the challenge's is the one [`Self::for_challenge`]
    /// picks.
    pub fn challenge_candidates(&self, url: &str) -> Vec<&CoursierCredential> {
        let Some(target) = UrlTarget::parse(url) else { return Vec::new() };
        self.entries.iter().filter(|c| c.auto && c.matches(&target)).collect()
    }

    /// The entries restricted to scpm's policy: credentials go over HTTPS, and over plain HTTP
    /// only to `insecure_origins` (`http://host[:port]` of repositories that set
    /// `allowInsecureCredentials`), scoped to exactly that origin. An entry that coursier would
    /// send over plain HTTP to a whole host (`https-only=false`) keeps only its HTTPS use plus
    /// one copy per opted-in origin it matches.
    pub fn restricted(&self, insecure_origins: &[String]) -> CoursierCredentials {
        let targets: Vec<UrlTarget> =
            insecure_origins.iter().filter_map(|o| UrlTarget::parse(&format!("{o}/"))).collect();
        let mut entries = Vec::new();
        for c in &self.entries {
            if c.scheme.as_deref().is_none_or(|s| s == "https") {
                entries.push(CoursierCredential { https_only: true, ..c.clone() });
            }
            let plain_http = c.scheme.as_deref() == Some("http") || (c.scheme.is_none() && !c.https_only);
            if plain_http {
                for t in targets.iter().filter(|t| t.scheme == "http" && c.matches(t)) {
                    entries.push(CoursierCredential {
                        scheme: Some("http".into()),
                        host: t.host.clone(),
                        port: t.port,
                        ..c.clone()
                    });
                }
            }
        }
        CoursierCredentials { entries }
    }
}

impl CoursierCredential {
    /// The entry's `host` in coursier's `[scheme://]host[:port]` form.
    pub fn host_spec(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let scheme = self.scheme.as_ref().map(|s| format!("{s}://")).unwrap_or_default();
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!("{scheme}{host}{port}")
    }
}

/// A request URL's scheme (lower-cased), host and effective port.
struct UrlTarget {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl UrlTarget {
    fn parse(url: &str) -> Option<UrlTarget> {
        let (scheme, rest) = url.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
        let (host, port) = split_host_port(authority)?;
        let port = port.or_else(|| default_port(&scheme));
        Some(UrlTarget { scheme, host: host.to_string(), port })
    }
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    }
}

/// `host[:port]` or `[v6][:port]` → the host (without brackets) and port; `None` for a bad port.
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(p) => Some(p.parse().ok()?),
        None => None,
    };
    Some((host, port))
}

/// Splits a credential `host` (`[scheme://]host[:port][/]`) into its scheme, host and port, like
/// coursier's `DirectCredentials.parseHost`. Values of another shape are kept whole as a host.
pub fn parse_host(value: &str) -> (Option<String>, String, Option<u16>) {
    let (scheme, rest) = match value.split_once("://") {
        Some((s, rest))
            if s.starts_with(|c: char| c.is_ascii_alphabetic())
                && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) =>
        {
            (Some(s.to_ascii_lowercase()), rest)
        }
        _ => (None, value),
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    match split_host_port(authority) {
        Some((host, port)) if !authority.contains('/') => (scheme, host.to_string(), port),
        _ => (None, value.to_string(), None),
    }
}

/// Coursier's config directory (`COURSIER_CONFIG_DIR`, else the platform default).
pub(crate) fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("COURSIER_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let base = dirs::config_dir()?;
    Some(if cfg!(target_os = "macos") {
        base.join("Coursier")
    } else if cfg!(windows) {
        base.join("Coursier").join("config")
    } else {
        base.join("coursier")
    })
}

fn default_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![dir.join("credentials.properties")];
    if let Ok(entries) = std::fs::read_dir(dir.join("credentials")) {
        let mut extra: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "properties"))
            .collect();
        extra.sort();
        files.extend(extra);
    }
    files
}

fn read_file(path: &Path) -> Vec<CoursierCredential> {
    std::fs::read_to_string(path).map(|text| parse_properties(&text)).unwrap_or_default()
}

/// Inline entries: `host[(realm)] user:password`, one per line. Leading spaces and empty lines
/// are ignored; a realm may contain spaces, and the password is everything after the first `:`,
/// trailing spaces included. Inline entries are HTTPS-only unless their host names a scheme.
pub fn parse_inline(text: &str) -> Vec<CoursierCredential> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            if line.is_empty() {
                return None;
            }
            let host_end = line.find(|c: char| c.is_whitespace() || c == '(')?;
            let (host, rest) = line.split_at(host_end);
            let (realm, rest) = match rest.strip_prefix('(') {
                Some(r) => {
                    let (realm, after) = r.split_once(')')?;
                    (Some(realm.to_string()), after)
                }
                None => (None, rest),
            };
            let (user, password) = rest.trim_start().split_once(':')?;
            let (scheme, host, port) = parse_host(host);
            Some(CoursierCredential {
                scheme,
                host,
                port,
                user: user.to_string(),
                password: password.to_string(),
                realm,
                auto: true,
                https_only: true,
            })
        })
        .collect()
}

/// Entries from a coursier credentials properties file.
pub fn parse_properties(text: &str) -> Vec<CoursierCredential> {
    let props = java_properties(text);
    let prefixes: Vec<&str> = props.keys().filter_map(|k| k.strip_suffix(".username")).collect();
    prefixes
        .into_iter()
        .filter_map(|prefix| {
            let get = |name: &str| props.get(&format!("{prefix}.{name}")).cloned();
            let flag = |name: &str| get(name).map(|v| v.trim().eq_ignore_ascii_case("true")).unwrap_or(true);
            let (scheme, host, port) = parse_host(get("host")?.trim());
            Some(CoursierCredential {
                scheme,
                host,
                port,
                user: get("username")?,
                password: get("password").unwrap_or_default(),
                realm: get("realm"),
                auto: flag("auto"),
                https_only: flag("https-only"),
            })
        })
        .collect()
}

/// A small `java.util.Properties` reader: `key=value`, `key: value` or `key value`, `#`/`!`
/// comments, backslash line continuations and escapes.
pub(crate) fn java_properties(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut logical = String::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        if logical.is_empty() && (line.is_empty() || line.starts_with('#') || line.starts_with('!')) {
            continue;
        }
        let trailing_backslashes = line.chars().rev().take_while(|c| *c == '\\').count();
        if trailing_backslashes % 2 == 1 {
            logical.push_str(&line[..line.len() - 1]);
            continue;
        }
        logical.push_str(line);
        let entry = std::mem::take(&mut logical);
        let mut key = String::new();
        let mut chars = entry.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        key.push(unescape_char(n, &mut chars));
                    }
                }
                '=' | ':' => break,
                c if c.is_whitespace() => {
                    while chars.peek().is_some_and(|c| c.is_whitespace()) {
                        chars.next();
                    }
                    if chars.peek().is_some_and(|c| *c == '=' || *c == ':') {
                        chars.next();
                    }
                    break;
                }
                c => key.push(c),
            }
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(n) = chars.next() {
                    value.push(unescape_char(n, &mut chars));
                }
            } else {
                value.push(c);
            }
        }
        out.insert(key, value);
    }
    out
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn unescape_char(c: char, rest: &mut Chars<'_>) -> char {
    match c {
        't' => '\t',
        'n' => '\n',
        'r' => '\r',
        'f' => '\u{c}',
        'u' => match utf16_unit(rest) {
            // A supplementary character is written as two `\uXXXX` escapes (a surrogate pair).
            Some(high @ 0xD800..=0xDBFF) => {
                let mut ahead = rest.clone();
                let low =
                    if ahead.next() == Some('\\') && ahead.next() == Some('u') { utf16_unit(&mut ahead) } else { None };
                match low {
                    Some(low @ 0xDC00..=0xDFFF) => {
                        *rest = ahead;
                        char::decode_utf16([high, low]).next().and_then(Result::ok).unwrap_or('?')
                    }
                    _ => '?',
                }
            }
            Some(unit) => char::from_u32(u32::from(unit)).unwrap_or('?'),
            None => '?',
        },
        other => other,
    }
}

/// Four hex digits of a `\u` escape.
fn utf16_unit(rest: &mut Chars<'_>) -> Option<u16> {
    let hex: String = (0..4).filter_map(|_| rest.next()).collect();
    u16::from_str_radix(&hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_properties_files() {
        let text = "# coursier credentials\n\
                    acme.host=repo.acme.example:8443\n\
                    acme.username=alice\n\
                    acme.password=s3cr\\=et\n\
                    local.host 127.0.0.1\n\
                    local.username: bob\n\
                    local.password=pw\n\
                    local.https-only=false\n\
                    manual.host=manual.example\n\
                    manual.username=carol\n\
                    manual.password=x\n\
                    manual.auto=false\n\
                    realm.host=realm.example\n\
                    realm.username=dave\n\
                    realm.password=y\n\
                    realm.realm = Acme Realm\n";
        let creds = CoursierCredentials { entries: parse_properties(text) };
        assert_eq!(creds.entries.len(), 4);
        let acme = creds.find("https://repo.acme.example:8443/maven/x.jar").unwrap();
        assert_eq!((acme.user.as_str(), acme.password.as_str()), ("alice", "s3cr=et"));
        assert!(creds.find("https://repo.acme.example/maven/x.jar").is_none(), "another port");
        assert!(creds.find("https://repo.acme.example:9443/maven/x.jar").is_none(), "another port");
        assert!(creds.find("http://repo.acme.example:8443/maven/x.jar").is_none(), "https-only by default");
        assert_eq!(creds.find("http://127.0.0.1:9999/m/x.jar").unwrap().user, "bob");
        assert!(creds.find("https://manual.example/x.jar").is_none(), "coursier never matches auto=false");
        assert!(creds.for_challenge("https://manual.example/x.jar", None).is_none());
        assert!(creds.find("https://elsewhere.example/x.jar").is_none());

        // Realm-scoped entries are held back until a challenge names their realm.
        assert!(creds.find("https://realm.example/x.jar").is_none());
        assert!(creds.for_challenge("https://realm.example/x.jar", Some("Other")).is_none());
        let r = creds.for_challenge("https://realm.example/x.jar", Some("Acme Realm")).unwrap();
        assert_eq!(r.user, "dave");
        assert_eq!(creds.for_challenge("http://127.0.0.1:1/x", Some("any")).unwrap().user, "bob");
    }

    #[test]
    fn scheme_qualified_hosts_match_one_origin() {
        let creds = CoursierCredentials {
            entries: parse_properties(
                "n.host=http://nexus.local:8081\nn.username=alice\nn.password=pw\n\
                 s.host=https://secure.example/\ns.username=bob\ns.password=pw\n",
            ),
        };
        assert_eq!(creds.find("http://nexus.local:8081/repo/x.jar").unwrap().user, "alice");
        assert!(creds.find("http://nexus.local:8082/repo/x.jar").is_none());
        assert!(creds.find("https://nexus.local:8081/repo/x.jar").is_none(), "another scheme");
        assert_eq!(creds.find("https://secure.example/x.jar").unwrap().user, "bob");
        assert_eq!(creds.find("https://secure.example:443/x.jar").unwrap().user, "bob");
        assert!(creds.find("https://secure.example:8443/x.jar").is_none(), "the default port only");
        assert!(creds.find("http://secure.example/x.jar").is_none());

        let inline = CoursierCredentials::from_env_value("http://nexus.local:8081 carol:pw");
        assert_eq!(inline.find("http://nexus.local:8081/x").unwrap().user, "carol");
        assert!(inline.find("http://nexus.local/x").is_none());
    }

    #[test]
    fn reads_inline_values_and_file_paths() {
        let inline = CoursierCredentials::from_env_value(
            "\n  a.example alice:pw1\nb.example(The Realm) bob:p:w2\nc.example:8443 carol: pw3  \n",
        );
        assert_eq!(inline.entries.len(), 3);
        assert_eq!(inline.find("https://a.example/x").unwrap().password, "pw1");
        let b = inline.for_challenge("https://b.example/x", Some("The Realm")).unwrap();
        assert_eq!((b.user.as_str(), b.password.as_str(), b.realm.as_deref()), ("bob", "p:w2", Some("The Realm")));
        let c = inline.find("https://c.example:8443/x").unwrap();
        assert_eq!(c.password, " pw3  ", "passwords keep their spaces");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.properties");
        std::fs::write(&file, "x.host=c.example\nx.username=carol\nx.password=pw3\n").unwrap();
        // A bare path is read as a file only if it starts with `/` (not `C:\…` on Windows).
        let bare = file.display().to_string();
        let values = [Some(format!("file:{bare}")), bare.starts_with('/').then_some(bare)];
        for value in values.into_iter().flatten() {
            let from_file = CoursierCredentials::from_env_value(&value);
            assert_eq!(from_file.find("https://c.example/x").unwrap().user, "carol", "{value}");
        }
    }

    #[test]
    fn hosts() {
        assert_eq!(parse_host("h.example:8443"), (None, "h.example".into(), Some(8443)));
        assert_eq!(parse_host("h.example"), (None, "h.example".into(), None));
        assert_eq!(parse_host("[::1]:8080"), (None, "::1".into(), Some(8080)));
        assert_eq!(parse_host("HTTPS://h.example/"), (Some("https".into()), "h.example".into(), None));
        assert_eq!(parse_host("http://h.example:81"), (Some("http".into()), "h.example".into(), Some(81)));
        assert_eq!(parse_host("h.example:port"), (None, "h.example:port".into(), None));
    }

    #[test]
    fn surrogate_pairs_decode_to_one_character() {
        let parsed = parse_properties("x.host=h\nx.username=u\nx.password=a\\ud83d\\ude00b\\u00e9\n");
        assert_eq!(parsed[0].password, "a\u{1F600}b\u{e9}");
    }

    #[test]
    fn debug_redacts_passwords() {
        let c = &parse_inline("h.example alice:hunter2")[0];
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
