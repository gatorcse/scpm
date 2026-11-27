//! Machine settings that belong to other tools, which scpm ignores on purpose (design §5.9):
//! Maven `settings.xml`, coursier's mirror files, and the Scala CLI config. `cs` is sealed off
//! from them; this module only finds them, so `scpm config explain` and a one-line notice can
//! say what would have applied. JVM proxy properties can't be detected and aren't looked for.
//!
//! The parsing is deliberately shallow: enough to name the setting and the repositories it
//! would have touched, never to apply it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::coursier::Repo;

/// One ignored setting.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForeignSetting {
    /// The file that holds it (home shortened to `~`), and the variable that named the file.
    pub file: String,
    /// What it sets, e.g. `mirror "corp" for *`.
    pub setting: String,
    #[serde(skip)]
    applies: Applies,
}

/// Which repositories a setting would have touched.
#[derive(Debug, Clone, PartialEq)]
enum Applies {
    All,
    /// A Maven `mirrorOf`: ids, `*`, `external:*`, `external:http:*`, `!id`.
    MirrorOf(String),
    /// Repository ids (a `settings.xml` server).
    Ids(Vec<String>),
    /// URL prefixes (a coursier mirror's `from`).
    UrlPrefixes(Vec<String>),
    /// Hosts a credential is for.
    Hosts(Vec<String>),
}

impl ForeignSetting {
    fn new(file: &str, setting: String, applies: Applies) -> ForeignSetting {
        ForeignSetting { file: file.to_string(), setting, applies }
    }

    /// The ids of the repositories in `repos` this setting would have applied to.
    pub fn repositories(&self, repos: &[Repo]) -> Vec<String> {
        repos.iter().filter(|r| self.applies_to(r)).map(|r| r.id.clone()).collect()
    }

    fn applies_to(&self, repo: &Repo) -> bool {
        match &self.applies {
            Applies::All => true,
            Applies::MirrorOf(spec) => mirror_of_matches(spec, repo),
            Applies::Ids(ids) => ids.contains(&repo.id),
            Applies::UrlPrefixes(prefixes) => prefixes.iter().any(|p| {
                let p = p.trim().to_ascii_lowercase();
                let p = if p.ends_with('/') { p } else { format!("{p}/") };
                repo.url.to_ascii_lowercase().starts_with(&p)
            }),
            Applies::Hosts(hosts) => {
                let host = host_of(repo.fetch_url());
                hosts.iter().any(|h| h.eq_ignore_ascii_case(&host))
            }
        }
    }
}

/// Where the foreign sources live.
#[derive(Debug, Clone, Default)]
pub struct Locations {
    pub home: Option<PathBuf>,
    /// Maven `settings.xml` files `cs` would read (absent when `COURSIER_MAVEN_SETTINGS=false`).
    pub maven_settings: Vec<(String, PathBuf)>,
    /// Coursier mirror files, each with the variable that named it (if any).
    pub coursier_mirrors: Vec<(String, PathBuf)>,
    pub scala_cli_config: Option<(String, PathBuf)>,
}

impl Locations {
    /// The locations `cs` 2.1.27 reads, from the environment.
    pub fn from_env() -> Locations {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = dirs::home_dir();
        let mut maven_settings = Vec::new();
        if !std::env::var("COURSIER_MAVEN_SETTINGS").is_ok_and(|v| v.eq_ignore_ascii_case("false")) {
            if let Some(h) = &home {
                maven_settings.push((String::new(), h.join(".m2/settings.xml")));
            }
            for name in ["CS_MAVEN_HOME", "MAVEN_HOME"] {
                if let Some(dir) = var(name) {
                    maven_settings.push((name.to_string(), dir.join("settings.xml")));
                }
            }
        }
        let mut coursier_mirrors = Vec::new();
        match var("COURSIER_MIRRORS") {
            Some(file) => coursier_mirrors.push(("COURSIER_MIRRORS".to_string(), file)),
            None => {
                if let Some(dir) = crate::credentials::config_dir() {
                    coursier_mirrors.push((String::new(), dir.join("mirror.properties")));
                }
            }
        }
        if let Some(file) = var("COURSIER_EXTRA_MIRRORS") {
            coursier_mirrors.push(("COURSIER_EXTRA_MIRRORS".to_string(), file));
        }
        let scala_cli_config = match var("SCALA_CLI_CONFIG") {
            Some(file) => Some(("SCALA_CLI_CONFIG".to_string(), file)),
            None => scala_cli_default().map(|f| (String::new(), f)),
        };
        Locations { home, maven_settings, coursier_mirrors, scala_cli_config }
    }

    /// `path` for messages: `~/…` under the home directory, plus the variable that named it.
    fn display(&self, var: &str, path: &Path) -> String {
        let shown = match self.home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        };
        if var.is_empty() { shown } else { format!("{shown} ({var})") }
    }
}

/// The Scala CLI config's default location (its `secrets/config.json` in the data directory).
fn scala_cli_default() -> Option<PathBuf> {
    let data = dirs::data_local_dir()?;
    Some(if cfg!(target_os = "macos") {
        data.join("ScalaCli/secrets/config.json")
    } else if cfg!(windows) {
        data.join("ScalaCli/data/secrets/config.json")
    } else {
        data.join("scalacli/secrets/config.json")
    })
}

/// Every foreign setting found at `locations`, whether or not it touches a repository.
pub fn detect(locations: &Locations) -> Vec<ForeignSetting> {
    let mut found = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for (var, path) in &locations.maven_settings {
        if seen.contains(path) {
            continue;
        }
        seen.push(path.clone());
        if let Ok(text) = std::fs::read_to_string(path) {
            found.extend(maven_settings(&locations.display(var, path), &text));
        }
    }
    for (var, path) in &locations.coursier_mirrors {
        if let Ok(text) = std::fs::read_to_string(path) {
            found.extend(coursier_mirrors(&locations.display(var, path), &text));
        }
    }
    if let Some((var, path)) = &locations.scala_cli_config
        && let Ok(text) = std::fs::read_to_string(path)
    {
        found.extend(scala_cli(&locations.display(var, path), &text));
    }
    found
}

/// The foreign settings that touch `repos`, each with the ids it would have applied to.
pub fn applying(found: Vec<ForeignSetting>, repos: &[Repo]) -> Vec<(ForeignSetting, Vec<String>)> {
    found
        .into_iter()
        .filter_map(|f| {
            let ids = f.repositories(repos);
            (!ids.is_empty()).then_some((f, ids))
        })
        .collect()
}

/// The one-line notice for foreign settings touching `repos`, at most once per process.
pub fn take_notice(repos: &[Repo]) -> Option<String> {
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if SHOWN.load(Ordering::Relaxed) {
        return None;
    }
    let notice = notice(&applying(detect(&Locations::from_env()), repos))?;
    (!SHOWN.swap(true, Ordering::Relaxed)).then_some(notice)
}

/// `<file>: <setting> is ignored (…)`, naming the first setting and counting the rest.
pub fn notice(applying: &[(ForeignSetting, Vec<String>)]) -> Option<String> {
    let (first, ids) = applying.first()?;
    let more = match applying.len() - 1 {
        0 => String::new(),
        1 => " (and 1 more setting)".to_string(),
        n => format!(" (and {n} more settings)"),
    };
    Some(format!(
        "{}: {} is ignored for {}{more}; scpm takes credentials, mirrors and proxies only from its user config and the environment (see `scpm config explain`)",
        first.file,
        first.setting,
        ids.join(", ")
    ))
}

/// Mirrors, servers and active proxies of a Maven `settings.xml`.
fn maven_settings(file: &str, text: &str) -> Vec<ForeignSetting> {
    let text = strip_comments(text);
    let mut found = Vec::new();
    let mirrors: Vec<(String, String, String)> = blocks(&text, "mirror")
        .into_iter()
        .map(|m| {
            (
                child(m, "id").unwrap_or_default(),
                child(m, "mirrorOf").unwrap_or_default(),
                child(m, "url").unwrap_or_default(),
            )
        })
        .collect();
    for (id, of, url) in &mirrors {
        found.push(ForeignSetting::new(
            file,
            format!("mirror \"{id}\" for {of} ({})", shown_origin(url)),
            Applies::MirrorOf(of.clone()),
        ));
    }
    for server in blocks(&text, "server") {
        let Some(id) = child(server, "id") else { continue };
        // A server's credentials go to the repository, or the mirror, with its id.
        let applies = match mirrors.iter().find(|(m, _, _)| *m == id) {
            Some((_, of, _)) => Applies::MirrorOf(of.clone()),
            None => Applies::Ids(vec![id.clone()]),
        };
        found.push(ForeignSetting::new(file, format!("server \"{id}\" credentials"), applies));
    }
    for proxy in blocks(&text, "proxy") {
        if child(proxy, "active").is_some_and(|a| a.eq_ignore_ascii_case("false")) {
            continue;
        }
        let id = child(proxy, "id").unwrap_or_default();
        let protocol = child(proxy, "protocol").unwrap_or_else(|| "http".into());
        let host = child(proxy, "host").unwrap_or_default();
        let port = child(proxy, "port").map(|p| format!(":{p}")).unwrap_or_default();
        let shown = shown_origin(&format!("{protocol}://{host}{port}"));
        found.push(ForeignSetting::new(file, format!("proxy \"{id}\" ({shown})"), Applies::All));
    }
    found
}

/// Coursier mirror properties: `<name>.from` (comma-separated URLs) and `<name>.to`.
fn coursier_mirrors(file: &str, text: &str) -> Vec<ForeignSetting> {
    let props = crate::credentials::java_properties(text);
    props
        .iter()
        .filter_map(|(key, from)| {
            let name = key.strip_suffix(".from")?;
            let to = props.get(&format!("{name}.to")).cloned().unwrap_or_default();
            let prefixes: Vec<String> =
                from.split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect();
            let shown: Vec<String> = prefixes.iter().map(|p| shown_origin(p)).collect();
            Some(ForeignSetting::new(
                file,
                format!("mirror \"{name}\" ({} to {})", shown.join(", "), shown_origin(&to)),
                Applies::UrlPrefixes(prefixes),
            ))
        })
        .collect()
}

/// The Scala CLI config's `repositories.credentials` and `repositories.mirrors`.
fn scala_cli(file: &str, text: &str) -> Vec<ForeignSetting> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let repositories = &json["repositories"];
    let mut found = Vec::new();
    // An empty array configures nothing; any other shape counts as applying to every repository.
    let empty = |v: &serde_json::Value| v.as_array().is_some_and(Vec::is_empty) || v.is_null();
    if let Some(credentials) = repositories.get("credentials").filter(|v| !empty(v)) {
        let hosts: Option<Vec<String>> = credentials
            .as_array()
            .map(|entries| entries.iter().filter_map(|e| e["host"].as_str().map(str::to_string)).collect());
        let (setting, applies) = match hosts {
            Some(hosts) if !hosts.is_empty() => {
                (format!("repositories.credentials for {}", hosts.join(", ")), Applies::Hosts(hosts))
            }
            _ => ("repositories.credentials".to_string(), Applies::All),
        };
        found.push(ForeignSetting::new(file, setting, applies));
    }
    if let Some(mirrors) = repositories.get("mirrors").filter(|v| !empty(v)) {
        let froms: Option<Vec<String>> = mirrors.as_array().map(|entries| {
            entries.iter().filter_map(|e| e.as_str()?.split_once('=').map(|(f, _)| f.trim().to_string())).collect()
        });
        let (setting, applies) = match froms {
            Some(froms) if !froms.is_empty() => {
                let shown: Vec<String> = froms.iter().map(|f| shown_origin(f)).collect();
                (format!("repositories.mirrors for {}", shown.join(", ")), Applies::UrlPrefixes(froms))
            }
            _ => ("repositories.mirrors".to_string(), Applies::All),
        };
        found.push(ForeignSetting::new(file, setting, applies));
    }
    found
}

/// Maven's `mirrorOf` matching, by repository id (and URL for `external:`).
fn mirror_of_matches(spec: &str, repo: &Repo) -> bool {
    let mut matched = false;
    for part in spec.split(',').map(str::trim) {
        if let Some(excluded) = part.strip_prefix('!') {
            if excluded == repo.id {
                return false;
            }
        } else if part == "*" || part == repo.id {
            matched = true;
        } else if part == "external:*" {
            matched |= !is_local(&repo.url);
        } else if part == "external:http:*" {
            matched |= !is_local(&repo.url) && crate::coursier::is_plain_http(&repo.url);
        }
    }
    matched
}

fn is_local(url: &str) -> bool {
    let host = host_of(url);
    url.starts_with("file:") || host == "localhost" || host == "127.0.0.1" || host == "[::1]"
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = if authority.starts_with('[') {
        authority.split_inclusive(']').next().unwrap_or(authority)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    host.to_ascii_lowercase()
}

/// Only `scheme://host[:port]` of another tool's URL, for display: its files may hold
/// credentials in a URL (`https://user:token@nexus/…`), so nothing else is shown. The host
/// follows the last `@` before the first `/`, so an `@` in a path (`/m/@scope/`) is no
/// userinfo; but a `user:password` before that `/` (a `:` that starts no port) means the
/// password itself holds a `/`, and the host follows the URL's last `@`.
fn shown_origin(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), url),
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => {
            let password_like = !authority.starts_with('[')
                && authority
                    .split_once(':')
                    .is_some_and(|(_, p)| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()));
            match rest.rsplit_once('@') {
                Some((_, host)) if password_like => host,
                _ => authority,
            }
        }
    };
    let host = host.split(['/', '?', '#']).next().unwrap_or_default();
    // Anything but a plain scheme, host and port (an undecoded reference, say) is not shown.
    let plain_scheme = scheme.trim_end_matches("://").chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    let plain_host = host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'));
    if plain_scheme && plain_host { format!("{scheme}{host}") } else { "(URL not shown)".to_string() }
}

fn strip_comments(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = rest[start..].find("-->").map(|end| &rest[start + end + 3..]).unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// The content of each `<tag>…</tag>` (or `<tag attr…>…</tag>`) element, not nested.
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        // `<mirrors>` is not `<mirror>`.
        if !after.starts_with(['>', ' ', '\t', '\n', '\r']) {
            rest = after;
            continue;
        }
        let Some(body_start) = after.find('>') else { break };
        let body = &after[body_start + 1..];
        let Some(end) = body.find(&close) else { break };
        out.push(&body[..end]);
        rest = &body[end + close.len()..];
    }
    out
}

/// The trimmed, unescaped text of the first `<tag>` child.
fn child(xml: &str, tag: &str) -> Option<String> {
    Some(unescape_xml(blocks(xml, tag).into_iter().next()?).trim().to_string())
}

/// XML text with the predefined entities, character references (`&#64;`, `&#x40;`) and CDATA
/// sections decoded, in one pass; other entities (`settings.xml` declares none) stay as written.
fn unescape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(at) = rest.find(['&', '<']) else { break };
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>").unwrap_or(cdata.len());
            out.push_str(&cdata[..end]);
            rest = cdata.get(end + 3..).unwrap_or("");
            continue;
        }
        let decoded = rest.find(';').filter(|_| rest.starts_with('&')).and_then(|semi| {
            let c = match &rest[1..semi] {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "amp" => Some('&'),
                name => {
                    let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => name.strip_prefix('#').and_then(|dec| dec.parse().ok()),
                    };
                    code.and_then(char::from_u32)
                }
            };
            c.map(|c| (c, semi + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push_str(&rest[..1]);
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(id: &str, url: &str) -> Repo {
        Repo { id: id.into(), url: url.into(), ..Default::default() }
    }

    fn repos() -> Vec<Repo> {
        vec![
            repo("central", "https://repo1.maven.org/maven2/"),
            repo("acme", "https://acme.example/maven/"),
            repo("local", "http://localhost:8081/m/"),
        ]
    }

    #[test]
    fn maven_settings_mirrors_servers_and_proxies() {
        let xml = r#"<settings>
          <!-- <mirror><id>commented</id><mirrorOf>*</mirrorOf><url>x</url></mirror> -->
          <mirrors>
            <mirror><id>corp</id><mirrorOf>external:*,!acme</mirrorOf><url>https://nexus/&amp;m</url></mirror>
          </mirrors>
          <servers>
            <server><id>corp</id><username>alice</username><password>secret</password></server>
            <server><id>acme</id><username>bob</username></server>
            <server><id>unused</id></server>
          </servers>
          <proxies>
            <proxy><id>off</id><active>false</active><host>p</host></proxy>
            <proxy><id>on</id><protocol>http</protocol><host>proxy.example</host><port>3128</port></proxy>
          </proxies>
        </settings>"#;
        let found = maven_settings("~/.m2/settings.xml", xml);
        let shown: Vec<(&str, Vec<String>)> =
            found.iter().map(|f| (f.setting.as_str(), f.repositories(&repos()))).collect();
        assert_eq!(
            shown,
            vec![
                ("mirror \"corp\" for external:*,!acme (https://nexus)", vec!["central".to_string()]),
                ("server \"corp\" credentials", vec!["central".to_string()]),
                ("server \"acme\" credentials", vec!["acme".to_string()]),
                ("server \"unused\" credentials", vec![]),
                ("proxy \"on\" (http://proxy.example:3128)", vec!["central".into(), "acme".into(), "local".into()]),
            ]
        );
        assert!(!format!("{found:?}").contains("secret"), "passwords are never kept");
    }

    #[test]
    fn credentials_in_foreign_urls_are_never_shown() {
        for (url, shown) in [
            ("https://user:t0ken@nexus/m", "https://nexus"),
            ("https://t0ken@nexus:8081/m", "https://nexus:8081"),
            ("https://user:t0#ken@nexus/m", "https://nexus"),
            ("https://user:t0/ken@nexus/m", "https://nexus"),
            ("https://nexus/m/@scope/x", "https://nexus"),
            ("https://nexus:8081/m/@scope/x", "https://nexus:8081"),
            ("https://user:t0?k/en@nexus/m", "https://nexus"),
            ("https://user:@nexus/m?x", "https://nexus"),
            ("user:t0ken@nexus/m", "nexus"),
            ("https://[::1]:8081/m/x", "https://[::1]:8081"),
            ("https://nexus/m", "https://nexus"),
            ("https://user:t0&ken@nexus/m", "https://nexus"),
            ("https://user:t0ken&#64;nexus/m", "(URL not shown)"),
        ] {
            assert_eq!(shown_origin(url), shown, "{url}");
        }
        // Character references and CDATA are decoded before the userinfo is cut off.
        for url in [
            "https://alice:t0ken&#64;nexus/m",
            "https://alice:t0ken&#x40;nexus/m",
            "<![CDATA[https://alice:t0ken@nexus/m]]>",
        ] {
            let xml = format!(
                "<settings><mirrors><mirror><id>corp</id><mirrorOf>*</mirrorOf><url>{url}</url></mirror></mirrors></settings>"
            );
            let found = maven_settings("~/.m2/settings.xml", &xml);
            assert_eq!(found[0].setting, "mirror \"corp\" for * (https://nexus)", "{url}");
        }
        assert_eq!(unescape_xml("a&amp;lt;b &#60; &bogus; &"), "a&lt;b < &bogus; &");
        let xml = "<settings><mirrors><mirror><id>corp</id><mirrorOf>*</mirrorOf>\
                   <url>https://alice:t0ken@nexus/m</url></mirror></mirrors>\
                   <proxies><proxy><id>p</id><host>bob:pw@proxy</host></proxy></proxies></settings>";
        let found = maven_settings("~/.m2/settings.xml", xml);
        assert_eq!(found[0].setting, "mirror \"corp\" for * (https://nexus)");
        assert_eq!(found[1].setting, "proxy \"p\" (http://proxy)");
        let found = coursier_mirrors(
            "m.properties",
            "corp.from=https://a:t0ken@repo1.maven.org/maven2\ncorp.to=https://b:t0ken@nexus/m\n",
        );
        assert_eq!(found[0].setting, "mirror \"corp\" (https://repo1.maven.org to https://nexus)");
        let found = scala_cli(
            "config.json",
            r#"{"repositories":{"mirrors":["https://a:t0ken@acme.example/maven=https://m"]}}"#,
        );
        assert_eq!(found[0].setting, "repositories.mirrors for https://acme.example");
    }

    #[test]
    fn coursier_and_scala_cli_mirrors() {
        let found = coursier_mirrors(
            "mirror.properties",
            "corp.from=https://repo1.maven.org/maven2, https://other.example\ncorp.to=https://nexus/m\n",
        );
        assert_eq!(found[0].repositories(&repos()), ["central"]);
        let found = scala_cli(
            "config.json",
            r#"{"repositories":{"mirrors":["https://acme.example/maven=https://m"],"credentials":[{"host":"acme.example","user":"value:u"}]}}"#,
        );
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| f.repositories(&repos()) == ["acme"]), "{found:?}");
        assert!(scala_cli("config.json", r#"{"java":{"home":"/x"}}"#).is_empty());
        let empty = r#"{"repositories":{"credentials":[],"mirrors":[]}}"#;
        assert!(scala_cli("config.json", empty).is_empty(), "empty arrays configure nothing");
    }

    #[test]
    fn detection_reads_the_files_and_names_them() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        std::fs::create_dir_all(home.join(".m2")).unwrap();
        std::fs::write(
            home.join(".m2/settings.xml"),
            "<settings><mirrors><mirror><id>corp</id><mirrorOf>*</mirrorOf><url>https://n</url></mirror></mirrors></settings>",
        )
        .unwrap();
        let mirrors = home.join("mirrors.properties");
        std::fs::write(&mirrors, "x.from=https://acme.example/maven\nx.to=https://m\n").unwrap();
        let locations = Locations {
            home: Some(home.clone()),
            maven_settings: vec![(String::new(), home.join(".m2/settings.xml"))],
            coursier_mirrors: vec![("COURSIER_MIRRORS".into(), mirrors), (String::new(), home.join("missing"))],
            scala_cli_config: None,
        };
        let found = applying(detect(&locations), &repos()[..2]);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0.file, "~/.m2/settings.xml");
        assert_eq!(found[1].0.file, "~/mirrors.properties (COURSIER_MIRRORS)");
        let n = notice(&found).unwrap();
        assert!(n.starts_with("~/.m2/settings.xml: mirror \"corp\" for * (https://n) is ignored for central, acme (and 1 more setting)"), "{n}");
        assert!(n.contains("scpm config explain"), "{n}");
        assert_eq!(notice(&[]), None);
    }
}
