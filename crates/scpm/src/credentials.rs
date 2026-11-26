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

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// One credential entry.
#[derive(Clone, PartialEq, Eq)]
pub struct CoursierCredential {
    /// Host name, without a port (coursier matches `URL.getHost`).
    pub host: String,
    pub user: String,
    pub password: String,
    pub realm: Option<String>,
    /// Coursier uses the entry without waiting for a challenge (and puts `user@` in cache paths).
    pub auto: bool,
    /// Only sent over HTTPS.
    pub https_only: bool,
}

impl fmt::Debug for CoursierCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoursierCredential")
            .field("host", &self.host)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("realm", &self.realm)
            .field("auto", &self.auto)
            .field("https_only", &self.https_only)
            .finish()
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
            None => parse_inline(trimmed),
        };
        CoursierCredentials { entries }
    }

    /// The entry coursier would use up front for `url`: an `auto` entry for its host, and an
    /// HTTPS URL unless the entry allows plain HTTP.
    pub fn find(&self, url: &str) -> Option<&CoursierCredential> {
        let (scheme, rest) = url.split_once("://")?;
        let host = hostname(rest.split('/').next().unwrap_or(rest));
        let https = scheme.eq_ignore_ascii_case("https");
        self.entries.iter().find(|c| c.auto && c.host.eq_ignore_ascii_case(host) && (https || !c.https_only))
    }
}

/// `host:port` → `host`; `[::1]:8080` → `::1`; strips any `user@`.
pub fn hostname(authority: &str) -> &str {
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split_once(']').map(|(h, _)| h).unwrap_or(rest);
    }
    authority.split_once(':').map(|(h, _)| h).unwrap_or(authority)
}

/// Coursier's config directory (`COURSIER_CONFIG_DIR`, else the platform default).
fn config_dir() -> Option<PathBuf> {
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

/// Inline entries: `host[(realm)] user:password`, one per line.
pub fn parse_inline(text: &str) -> Vec<CoursierCredential> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (host_part, secret) = line.split_once(char::is_whitespace)?;
            let (user, password) = secret.trim().split_once(':')?;
            let (host, realm) = match host_part.split_once('(') {
                Some((h, r)) => (h, Some(r.trim_end_matches(')').to_string())),
                None => (host_part, None),
            };
            Some(CoursierCredential {
                host: hostname(host).to_string(),
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
            Some(CoursierCredential {
                host: hostname(get("host")?.trim()).to_string(),
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
fn java_properties(text: &str) -> BTreeMap<String, String> {
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

fn unescape_char(c: char, rest: &mut std::iter::Peekable<std::str::Chars<'_>>) -> char {
    match c {
        't' => '\t',
        'n' => '\n',
        'r' => '\r',
        'f' => '\u{c}',
        'u' => {
            let hex: String = (0..4).filter_map(|_| rest.next()).collect();
            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('?')
        }
        other => other,
    }
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
                    acme.realm = Acme Realm\n\
                    local.host 127.0.0.1\n\
                    local.username: bob\n\
                    local.password=pw\n\
                    local.https-only=false\n\
                    manual.host=manual.example\n\
                    manual.username=carol\n\
                    manual.password=x\n\
                    manual.auto=false\n";
        let creds = CoursierCredentials { entries: parse_properties(text) };
        assert_eq!(creds.entries.len(), 3);
        let acme = creds.find("https://repo.acme.example:8443/maven/x.jar").unwrap();
        assert_eq!((acme.user.as_str(), acme.password.as_str()), ("alice", "s3cr=et"));
        assert_eq!(acme.realm.as_deref(), Some("Acme Realm"));
        assert!(creds.find("http://repo.acme.example/maven/x.jar").is_none(), "https-only by default");
        assert_eq!(creds.find("http://127.0.0.1:9999/m/x.jar").unwrap().user, "bob");
        assert!(creds.find("https://manual.example/x.jar").is_none(), "auto=false waits for a challenge");
        assert!(creds.find("https://elsewhere.example/x.jar").is_none());
    }

    #[test]
    fn reads_inline_values_and_file_paths() {
        let inline = CoursierCredentials::from_env_value("a.example alice:pw1\nb.example(Realm) bob:p:w2\n");
        assert_eq!(inline.entries.len(), 2);
        assert_eq!(inline.find("https://a.example/x").unwrap().password, "pw1");
        let b = inline.find("https://b.example/x").unwrap();
        assert_eq!((b.password.as_str(), b.realm.as_deref()), ("p:w2", Some("Realm")));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.properties");
        std::fs::write(&file, "x.host=c.example\nx.username=carol\nx.password=pw3\n").unwrap();
        for value in [file.display().to_string(), format!("file:{}", file.display())] {
            let from_file = CoursierCredentials::from_env_value(&value);
            assert_eq!(from_file.find("https://c.example/x").unwrap().user, "carol", "{value}");
        }
    }

    #[test]
    fn hostnames() {
        assert_eq!(hostname("h.example:8443"), "h.example");
        assert_eq!(hostname("alice@h.example"), "h.example");
        assert_eq!(hostname("[::1]:8080"), "::1");
        assert_eq!(hostname("h.example"), "h.example");
    }

    #[test]
    fn debug_redacts_passwords() {
        let c = &parse_inline("h.example alice:hunter2")[0];
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
