//! `scpm config explain`: where each repository's machine settings come from (design §5.9).
//! Secrets are never shown, only where they would be read from.

use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use crate::coursier::{self, CredentialsFrom, Repo, RepoCredentials, Secret};
use crate::credentials::CoursierCredentials;
use crate::foreign::{self, Locations};
use crate::install::{self, InstallError};
use crate::machine::{Machine, ProxySettings};
use crate::workspace::Workspace;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Explanation {
    pub user_config: UserConfigInfo,
    pub proxy: ProxyInfo,
    pub repositories: Vec<RepoExplanation>,
    /// Settings of other tools that would have applied to these repositories.
    pub ignored: Vec<IgnoredSetting>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserConfigInfo {
    pub path: Option<String>,
    pub exists: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyInfo {
    pub settings: Option<ProxySettings>,
    /// Why the proxy can't be used (downloads fail until it is fixed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoExplanation {
    pub id: String,
    pub url: String,
    /// Where downloads go: the mirror's URL, else `url`.
    pub effective_url: String,
    pub mirror: Option<MirrorInfo>,
    pub credentials: Option<CredentialsInfo>,
    /// The proxy requests to `effectiveUrl` go through.
    pub proxy: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorInfo {
    pub url: String,
    /// The `mirrorOf` entry that matched.
    pub mirror_of: String,
    pub source: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsInfo {
    /// `bearer` or `basic`.
    pub kind: &'static str,
    /// `scpm-workspace.yaml`, `user config`, `COURSIER_CREDENTIALS` or `credentials.properties`.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<SecretInfo>,
    /// The token or password.
    pub secret: SecretInfo,
    /// Coursier entries scoped to a realm are sent only after a challenge naming it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
}

/// Where a value is read from. Only user names are ever shown.
#[derive(Debug, Serialize)]
#[serde(tag = "from", rename_all = "camelCase")]
pub enum SecretInfo {
    Env {
        name: String,
        set: bool,
    },
    File {
        path: String,
        exists: bool,
    },
    Literal {
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IgnoredSetting {
    pub file: String,
    pub setting: String,
    pub repositories: Vec<String>,
}

/// Explains the workspace's repositories under the current machine settings.
pub fn explain(ws: &Workspace) -> Result<Explanation, InstallError> {
    let (repos, machine) = install::repositories(ws)?;
    let declared = coursier::repositories(&ws.repositories())?;
    let coursier_source = match std::env::var("COURSIER_CREDENTIALS") {
        Ok(v) if !v.trim().is_empty() => "COURSIER_CREDENTIALS",
        _ => "credentials.properties",
    };
    let coursier_creds = CoursierCredentials::load().restricted(&coursier::insecure_credential_origins(&repos));
    let locations = Locations::from_env();
    Ok(build(
        &repos,
        &declared,
        machine,
        &coursier_creds,
        coursier_source,
        foreign::applying(foreign::detect(&locations), &repos),
        &|var| std::env::var(var).ok(),
        locations.home.as_deref(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn build(
    repos: &[Repo],
    declared: &[Repo],
    machine: &Machine,
    coursier_creds: &CoursierCredentials,
    coursier_source: &str,
    ignored: Vec<(foreign::ForeignSetting, Vec<String>)>,
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> Explanation {
    let shown = |path: &Path| match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    let secret = |s: &Secret, show_literal: bool| match s {
        Secret::Env(name) => SecretInfo::Env { name: name.clone(), set: env(name).is_some_and(|v| !v.is_empty()) },
        Secret::File(path) => SecretInfo::File { path: shown(path), exists: path.is_file() },
        Secret::Literal(value) => SecretInfo::Literal { value: show_literal.then(|| value.clone()) },
    };
    let proxy = machine.proxy.as_ref().ok().and_then(Option::as_ref);
    let repositories = repos
        .iter()
        .map(|repo| {
            let mut notes = Vec::new();
            let credentials = match &repo.credentials {
                Some(c) => {
                    let source = repo.credentials_from.describe().to_string();
                    Some(match c {
                        RepoCredentials::Bearer { token } => CredentialsInfo {
                            kind: "bearer",
                            source,
                            user: None,
                            secret: secret(token, false),
                            realm: None,
                        },
                        RepoCredentials::Basic { user, password } => CredentialsInfo {
                            kind: "basic",
                            source,
                            user: Some(secret(user, true)),
                            secret: secret(password, false),
                            realm: None,
                        },
                    })
                }
                None => {
                    let up_front = coursier_creds.find(repo.fetch_url());
                    let found =
                        up_front.or_else(|| coursier_creds.challenge_candidates(repo.fetch_url()).into_iter().next());
                    found.map(|c| CredentialsInfo {
                        kind: "basic",
                        source: coursier_source.to_string(),
                        user: Some(SecretInfo::Literal { value: Some(c.user.clone()) }),
                        secret: SecretInfo::Literal { value: None },
                        realm: if up_front.is_some() { None } else { c.realm.clone() },
                    })
                }
            };
            if repo.mirror.is_some()
                && declared.iter().any(|d| d.id == repo.id && d.credentials.is_some())
                && repo.credentials_from == CredentialsFrom::Workspace
            {
                notes.push(format!(
                    "the credentials in {} are not sent to the mirror, which is another host",
                    crate::config::WORKSPACE_FILE
                ));
            }
            if let Some(note) = machine.skipped_credential(repo, repos) {
                notes.push(note);
            }
            RepoExplanation {
                id: repo.id.clone(),
                url: repo.url.clone(),
                effective_url: repo.fetch_url().to_string(),
                mirror: repo.mirror.as_ref().map(|m| MirrorInfo {
                    url: m.url.clone(),
                    mirror_of: m.matched.clone(),
                    source: "user config",
                }),
                credentials,
                proxy: proxy.and_then(|p| p.for_url(repo.fetch_url())).map(|s| s.url()),
                notes,
            }
        })
        .collect();
    Explanation {
        user_config: UserConfigInfo { path: machine.path.as_deref().map(shown), exists: machine.exists },
        proxy: ProxyInfo { settings: proxy.cloned(), error: machine.proxy.as_ref().err().cloned() },
        repositories,
        ignored: ignored
            .into_iter()
            .map(|(f, ids)| IgnoredSetting { file: f.file, setting: f.setting, repositories: ids })
            .collect(),
        warnings: machine.warnings.clone(),
    }
}

fn describe_secret(what: &str, s: &SecretInfo) -> String {
    match s {
        SecretInfo::Env { name, set: true } => format!("{what} from env {name}"),
        SecretInfo::Env { name, set: false } => format!("{what} from env {name} (not set)"),
        SecretInfo::File { path, exists: true } => format!("{what} from file {path}"),
        SecretInfo::File { path, exists: false } => format!("{what} from file {path} (missing)"),
        SecretInfo::Literal { value: Some(v) } => format!("{what} {v}"),
        SecretInfo::Literal { value: None } => format!("{what} (literal)"),
    }
}

/// The text report.
pub fn render(e: &Explanation) -> String {
    let width = e.repositories.iter().map(|r| r.id.len()).max().unwrap_or(0).max(7);
    let pad = " ".repeat(width + 2);
    let mut out = String::new();
    for r in &e.repositories {
        let _ = writeln!(out, "{:width$}  {}", r.id, r.url);
        if let Some(m) = &r.mirror {
            let _ = writeln!(out, "{pad}via mirror {}  ({}, mirrorOf {})", m.url, m.source, m.mirror_of);
        }
        match &r.credentials {
            Some(c) => {
                let what = match (&c.user, c.kind) {
                    (Some(user), _) => {
                        let password = describe_secret("password", &c.secret);
                        format!("{}, {password}", describe_secret("user", user))
                    }
                    (None, _) => describe_secret("bearer token", &c.secret),
                };
                let realm =
                    c.realm.as_ref().map(|r| format!(", after a challenge for realm \"{r}\"")).unwrap_or_default();
                let _ = writeln!(out, "{pad}credentials: {what}  ({}{realm})", c.source);
            }
            None => {
                let _ = writeln!(out, "{pad}credentials: none");
            }
        }
        let proxy = match (&r.proxy, &e.proxy.settings, &e.proxy.error) {
            (_, _, Some(err)) => format!("error: {err}"),
            (Some(p), Some(s), _) => format!("{p}  ({})", s.source),
            (None, Some(s), _) => {
                let host =
                    r.effective_url.split_once("://").map(|(_, rest)| rest.split(['/', ':']).next().unwrap_or(rest));
                match host {
                    Some(h) if s.bypasses(h) => format!("none ({h} matches noProxy)"),
                    _ => "none".to_string(),
                }
            }
            (_, None, _) => "none".to_string(),
        };
        let _ = writeln!(out, "{pad}proxy: {proxy}");
        for note in &r.notes {
            let _ = writeln!(out, "{pad}note: {note}");
        }
    }
    if let Some(s) = &e.proxy.settings
        && !s.ignored_no_proxy.is_empty()
    {
        let _ = writeln!(
            out,
            "{:width$}  noProxy entries not applied (address ranges): {}",
            "proxy",
            s.ignored_no_proxy.join(", ")
        );
    }
    let config = match (&e.user_config.path, e.user_config.exists) {
        (Some(p), true) => p.clone(),
        (Some(p), false) => format!("{p} (not found)"),
        (None, _) => "none (no config directory)".to_string(),
    };
    let _ = writeln!(out, "{:width$}  {config}", "config");
    for i in &e.ignored {
        let _ = writeln!(
            out,
            "{:width$}  {}: {} (would apply to {})",
            "ignored",
            i.file,
            i.setting,
            i.repositories.join(", ")
        );
    }
    for w in &e.warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_sources_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(
            &config,
            "credentials:\n  - {url: 'https://nexus.example.com/repository/', user: alice, password: 'file:~/.secrets/nexus'}\n\
             mirrors:\n  - {url: 'https://nexus.example.com/repository/maven-public', mirrorOf: [central]}\n\
             proxy:\n  https: 'http://proxy.example.com:3128'\n  noProxy: [nexus.example.com, 10.0.0.0/8]\n",
        )
        .unwrap();
        let machine = Machine::from_file(&config).unwrap();
        let declared = vec![
            Repo { id: "central".into(), url: coursier::CENTRAL_URL.into(), ..Default::default() },
            Repo {
                id: "acme".into(),
                url: "https://acme.jfrog.io/artifactory/maven/".into(),
                credentials: Some(RepoCredentials::Bearer { token: Secret::Env("ACME_TOKEN".into()) }),
                ..Default::default()
            },
            Repo { id: "other".into(), url: "https://other.example/m/".into(), ..Default::default() },
        ];
        let repos = machine.apply(declared.clone());
        let coursier_creds = CoursierCredentials::from_env_value("other.example carol:hunter2");
        let e = build(
            &repos,
            &declared,
            &machine,
            &coursier_creds,
            "credentials.properties",
            Vec::new(),
            &|_| None,
            dirs::home_dir().as_deref(),
        );
        let text = render(&e);
        let json = serde_json::to_string(&e).unwrap();
        for shown in [&text, &json] {
            assert!(!shown.contains("hunter2"), "{shown}");
        }
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "central  https://repo1.maven.org/maven2/");
        assert_eq!(
            lines[1],
            "         via mirror https://nexus.example.com/repository/maven-public/  (user config, mirrorOf central)"
        );
        assert!(
            lines[2].starts_with(
                "         credentials: user alice, password from file ~/.secrets/nexus (missing)  (user config)"
            ),
            "{text}"
        );
        assert_eq!(lines[3], "         proxy: none (nexus.example.com matches noProxy)");
        assert_eq!(lines[5], "         credentials: bearer token from env ACME_TOKEN (not set)  (scpm-workspace.yaml)");
        assert_eq!(lines[6], "         proxy: http://proxy.example.com:3128  (user config)");
        assert_eq!(lines[8], "         credentials: user carol, password (literal)  (credentials.properties)");
        assert!(text.contains("noProxy entries not applied (address ranges): 10.0.0.0/8"), "{text}");
        assert!(text.contains("config   ") && text.contains("config.yaml"), "{text}");
        assert_eq!(e.repositories[0].effective_url, "https://nexus.example.com/repository/maven-public/");
        assert_eq!(e.repositories[1].proxy.as_deref(), Some("http://proxy.example.com:3128"));
    }
}
