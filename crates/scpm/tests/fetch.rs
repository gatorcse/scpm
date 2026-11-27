//! The Rust lock-restore fetcher against a local HTTP server (no network, no `cs`).

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use scpm::coursier::{Auth, Repo};
use scpm::credentials::CoursierCredentials;
use scpm::fetch::{FetchItem, FetchOptions, fetch_all, sha256_file};
use scpm::install::fetch_items;
use scpm::lockfile::{ArtifactLock, Lockfile};
use support::{HELLO_JAR_PATH, Reply, Server, hello_repo_files};

const JAR: &[u8] = b"PK\x03\x04 not really a jar, but bytes with a known sha256";

fn sha256(bytes: &[u8]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    fs::write(&p, bytes).unwrap();
    sha256_file(&p).unwrap()
}

fn item(url: String, dest: &Path, body: &[u8]) -> FetchItem {
    FetchItem {
        key: "com.example:hello:1.0".into(),
        url,
        dest: dest.to_path_buf(),
        sha256: sha256(body),
        size: body.len() as u64,
        auth: Ok(None),
        challenge: Vec::new(),
        verify_cached: false,
        fallbacks: Vec::new(),
    }
}

#[test]
fn fallbacks_are_asked_only_after_a_not_found_each_with_its_own_credentials() {
    let basic = |user: &str, password: &str| Ok(Some(Auth::Basic { user: user.into(), password: password.into() }));
    let source = |server: &Server, cache: &Path, auth: Result<Option<Auth>, String>| FetchItem {
        auth,
        ..item(server.url(HELLO_JAR_PATH), &cache.join(format!("{}/hello-1.0.jar", server.port)), JAR)
    };

    // The hint answers 404 for its own token; the fallback must get its own token, not the hint's.
    let hint = Server::maven(Default::default(), Some(("a", "token-a")));
    let other = Server::maven(hello_repo_files(JAR), Some(("b", "token-b")));
    let cache = tempfile::tempdir().unwrap();
    let mut first = source(&hint, cache.path(), basic("a", "token-a"));
    first.fallbacks = vec![source(&other, cache.path(), basic("b", "token-b"))];
    assert_eq!(fetch_all(vec![first], FetchOptions::default()).unwrap().downloaded, 1);
    assert_eq!(hint.count(HELLO_JAR_PATH), 1);
    let sent: Vec<Option<String>> = other.requests.lock().unwrap().iter().map(|r| r.authorization.clone()).collect();
    assert_eq!(sent.len(), 1);
    assert!(sent.iter().all(|a| a.as_deref() == Some("Basic Yjp0b2tlbi1i")), "token B only: {sent:?}");

    // A 401 (or 403) from the hint is an answer, not a miss: no fallback is asked.
    let locked = Server::maven(hello_repo_files(JAR), Some(("alice", "s3cret")));
    let open = Server::maven(hello_repo_files(JAR), None);
    let cache = tempfile::tempdir().unwrap();
    let mut first = source(&locked, cache.path(), Ok(None));
    first.fallbacks = vec![source(&open, cache.path(), Ok(None))];
    let err = fetch_all(vec![first], FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("HTTP 401"), "{err}");
    assert_eq!(open.requests.lock().unwrap().len(), 0);

    // A hint that can't be asked (an unset credential variable) is skipped like a 404; its
    // problem is the error only when no source has the file.
    let cache = tempfile::tempdir().unwrap();
    let mut first = source(&locked, cache.path(), Err("repository `a` needs `UNSET`, which is not set".into()));
    first.fallbacks = vec![source(&open, cache.path(), Ok(None))];
    let report = fetch_all(vec![first], FetchOptions::default()).unwrap();
    assert_eq!(report.downloaded, 1);
    // ... but it is a warning, so a missing token doesn't go unnoticed.
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].contains("was not asked") && report.warnings[0].contains("`UNSET`"),
        "{:?}",
        report.warnings
    );
    let empty = Server::maven(Default::default(), None);
    let mut first = source(&locked, cache.path(), Err("repository `a` needs `UNSET`, which is not set".into()));
    first.fallbacks = vec![source(&empty, cache.path(), Ok(None))];
    first.dest = cache.path().join("elsewhere/hello-1.0.jar");
    let err = fetch_all(vec![first], FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("`UNSET`"), "{err}");

    // The hint answers 404 and the only other source can't be asked: the error says why.
    let cache = tempfile::tempdir().unwrap();
    let mut first = source(&empty, cache.path(), Ok(None));
    first.fallbacks =
        vec![source(&open, cache.path(), Err("repository `acme` needs `ACME_TOKEN`, which is not set".into()))];
    let err = fetch_all(vec![first], FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("HTTP 404"), "{err}");
    assert!(err.contains("note: ") && err.contains("was not asked") && err.contains("`ACME_TOKEN`"), "{err}");
}

#[test]
fn oversized_bodies_are_rejected_before_they_are_written() {
    let big = vec![b'x'; 4 << 20];
    let server = Server::start(move |_, _| Reply::Ok(big.clone()));
    let cache = tempfile::tempdir().unwrap();
    let dest = cache.path().join("hello-1.0.jar");
    let err =
        fetch_all(vec![item(server.url(HELLO_JAR_PATH), &dest, JAR)], FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("but the lock says"), "{err}");
    assert!(!dest.exists());
    assert_eq!(fs::read_dir(cache.path()).unwrap().count(), 0, "no partial file is left behind");
}

#[test]
fn realm_scoped_credentials_wait_for_a_matching_challenge() {
    // The test server challenges with `Basic realm="test"`.
    let server = Server::maven(hello_repo_files(JAR), Some(("alice", "s3cret")));
    let repos = vec![Repo {
        id: "local".into(),
        url: server.url("/maven/"),
        credentials: None,
        allow_insecure_credentials: true,
        ..Default::default()
    }];
    let cache = tempfile::tempdir().unwrap();

    let other = CoursierCredentials::from_env_value(&format!("http://127.0.0.1:{}(other) alice:s3cret", server.port));
    let items = fetch_items(&hello_lock(), &repos, &other, cache.path(), &mut Vec::new()).unwrap();
    assert!(matches!(items[0].auth, Ok(None)), "realm-scoped credentials are not sent up front");
    let err = fetch_all(items, FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("HTTP 401"), "{err}");
    assert!(server.requests.lock().unwrap().iter().all(|r| r.authorization.is_none()), "wrong realm: never sent");

    let creds = CoursierCredentials::from_env_value(&format!("http://127.0.0.1:{}(test) alice:s3cret", server.port));
    let items = fetch_items(&hello_lock(), &repos, &creds, cache.path(), &mut Vec::new()).unwrap();
    // Not matched up front, so coursier's cache path has no `user@`.
    assert!(!items[0].dest.to_string_lossy().contains("alice"));
    assert_eq!(fetch_all(items, FetchOptions::default()).unwrap().downloaded, 1);
}

#[test]
fn files_cached_under_another_user_are_hash_checked() {
    let cache = tempfile::tempdir().unwrap();
    let dest = cache.path().join("hello-1.0.jar");
    let tampered = vec![b'!'; JAR.len()];
    fs::write(&dest, &tampered).unwrap();
    let mut it = item("http://127.0.0.1:9/never".into(), &dest, JAR);
    it.auth = Err("repository `acme`: environment variable `ACME_USER` (user) is not set".into());
    it.verify_cached = true;
    let err = fetch_all(vec![it.clone()], FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("ACME_USER"), "a same-size file with other bytes is not trusted: {err}");
    fs::write(&dest, JAR).unwrap();
    assert_eq!(fetch_all(vec![it], FetchOptions::default()).unwrap().present, 1);
}

#[test]
fn truncated_bodies_are_retried() {
    let server =
        Server::start(|_, seen| if seen == 0 { Reply::Truncated(JAR[..10].to_vec()) } else { Reply::Ok(JAR.to_vec()) });
    let cache = tempfile::tempdir().unwrap();
    let dest = cache.path().join("hello-1.0.jar");
    let report = fetch_all(vec![item(server.url(HELLO_JAR_PATH), &dest, JAR)], FetchOptions::default()).unwrap();
    assert_eq!(report.downloaded, 1);
    assert_eq!(fs::read(&dest).unwrap(), JAR);
    assert_eq!(server.count(HELLO_JAR_PATH), 2, "the truncated body was retried");
}

#[test]
fn retry_after_is_honoured() {
    let server = Server::start(|_, seen| {
        if seen == 0 { Reply::Status(503, vec![("Retry-After".into(), "2".into())]) } else { Reply::Ok(JAR.to_vec()) }
    });
    let cache = tempfile::tempdir().unwrap();
    let dest = cache.path().join("hello-1.0.jar");
    let started = Instant::now();
    fetch_all(vec![item(server.url(HELLO_JAR_PATH), &dest, JAR)], FetchOptions::default()).unwrap();
    // The default first backoff is 500ms; Retry-After asked for 2s.
    assert!(started.elapsed() >= Duration::from_secs(2), "{:?}", started.elapsed());
    assert_eq!(server.count(HELLO_JAR_PATH), 2);
}

#[test]
fn transport_errors_collapse_per_host_with_a_hint() {
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cache = tempfile::tempdir().unwrap();
    let items: Vec<FetchItem> = (0..3)
        .map(|i| {
            let mut it =
                item(format!("http://127.0.0.1:{port}/m/a{i}.jar"), &cache.path().join(format!("a{i}.jar")), JAR);
            it.key = format!("g:a{i}:1.0");
            it
        })
        .collect();
    let err = fetch_all(items, FetchOptions::default()).unwrap_err().to_string();
    assert_eq!(err.lines().count(), 2, "one line for the host plus the hint: {err}");
    assert!(err.contains(&format!("127.0.0.1:{port}: 3 downloads failed")), "{err}");
    assert!(err.to_lowercase().contains("connection refused"), "the source chain is included: {err}");
    assert!(err.contains("are you offline or behind a proxy?"), "{err}");
}

#[test]
fn http_errors_are_not_mistaken_for_being_offline() {
    let server = Server::maven(hello_repo_files(JAR), None);
    let cache = tempfile::tempdir().unwrap();
    let missing = item(server.url("/maven/missing.jar"), &cache.path().join("missing.jar"), JAR);
    let err = fetch_all(vec![missing], FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("HTTP 404") && !err.contains("offline"), "{err}");
}

#[test]
fn verify_rehashes_and_repairs_cached_files() {
    let server = Server::maven(hello_repo_files(JAR), None);
    let cache = tempfile::tempdir().unwrap();
    let dest = cache.path().join("hello-1.0.jar");
    let mut corrupt = JAR.to_vec();
    corrupt[0] ^= 0xff;
    fs::write(&dest, &corrupt).unwrap();
    let it = || item(server.url(HELLO_JAR_PATH), &dest, JAR);

    // Without --verify, a file of the right size is trusted.
    let report = fetch_all(vec![it()], FetchOptions::default()).unwrap();
    assert_eq!((report.present, report.downloaded), (1, 0));

    let report = fetch_all(vec![it()], FetchOptions { verify: true, ..Default::default() }).unwrap();
    assert_eq!((report.verified, report.repaired, report.downloaded), (1, 1, 1));
    assert_eq!(fs::read(&dest).unwrap(), JAR);

    let report = fetch_all(vec![it()], FetchOptions { verify: true, ..Default::default() }).unwrap();
    assert_eq!((report.verified, report.repaired, report.present), (1, 0, 1));
}

#[test]
fn credential_errors_only_matter_when_downloading() {
    let cache = tempfile::tempdir().unwrap();
    let dest = cache.path().join("hello-1.0.jar");
    let mut it = item("http://127.0.0.1:9/never".into(), &dest, JAR);
    it.auth = Err("repository `acme`: environment variable `ACME_TOKEN` (token) is not set".into());
    let err = fetch_all(vec![it.clone()], FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("ACME_TOKEN"), "{err}");

    fs::write(&dest, JAR).unwrap();
    assert_eq!(fetch_all(vec![it], FetchOptions::default()).unwrap().present, 1);
}

fn hello_lock() -> Lockfile {
    let mut lock = Lockfile { lockfile_version: 1, ..Default::default() };
    lock.boot.insert("test".into(), vec!["com.example:hello:1.0".into()]);
    lock.artifacts.insert(
        "com.example:hello:1.0".into(),
        ArtifactLock { repo: "local".into(), sha256: sha256(JAR), size: JAR.len() as u64 },
    );
    lock
}

#[test]
fn coursier_credentials_authenticate_and_name_the_cache_directory() {
    let server = Server::maven(hello_repo_files(JAR), Some(("alice", "s3cret")));
    let repos = vec![Repo {
        id: "local".into(),
        url: server.url("/maven/"),
        credentials: None,
        allow_insecure_credentials: true,
        ..Default::default()
    }];
    let dir = tempfile::tempdir().unwrap();
    let props = dir.path().join("credentials.properties");
    fs::write(&props, "local.host=127.0.0.1\nlocal.username=alice\nlocal.password=s3cret\nlocal.https-only=false\n")
        .unwrap();
    let creds = CoursierCredentials::from_env_value(&format!("file:{}", props.display()));
    let cache = tempfile::tempdir().unwrap();

    // Without credentials the server refuses.
    let anonymous =
        fetch_items(&hello_lock(), &repos, &CoursierCredentials::default(), cache.path(), &mut Vec::new()).unwrap();
    let err = fetch_all(anonymous, FetchOptions::default()).unwrap_err().to_string();
    assert!(err.contains("HTTP 401") && err.contains("credentials"), "{err}");

    let items = fetch_items(&hello_lock(), &repos, &creds, cache.path(), &mut Vec::new()).unwrap();
    assert!(matches!(&items[0].auth, Ok(Some(Auth::Basic { user, .. })) if user == "alice"));
    // coursier's FileCache puts the user in the path: `http/alice@127.0.0.1:<port>/…`.
    let expected = cache
        .path()
        .join(format!("http/alice%40127.0.0.1%3A{}/maven/com/example/hello/1.0/hello-1.0.jar", server.port));
    assert_eq!(items[0].dest, expected);
    let report = fetch_all(items, FetchOptions::default()).unwrap();
    assert_eq!(report.downloaded, 1);
    assert_eq!(fs::read(&expected).unwrap(), JAR);
    let auth = server.requests.lock().unwrap().last().unwrap().authorization.clone();
    assert_eq!(auth.as_deref(), Some(format!("Basic {}", support::base64(b"alice:s3cret")).as_str()));
}

#[test]
fn coursier_credentials_need_the_repository_opt_in_over_http() {
    let server = Server::maven(hello_repo_files(JAR), Some(("alice", "s3cret")));
    let repo = |allow: bool| {
        vec![Repo {
            id: "local".into(),
            url: server.url("/maven/"),
            credentials: None,
            allow_insecure_credentials: allow,
            ..Default::default()
        }]
    };
    let cache = tempfile::tempdir().unwrap();
    // Both forms coursier would send over plain HTTP: `https-only=false`, and an `http://` host.
    for creds in [
        "x.host=127.0.0.1\nx.username=alice\nx.password=s3cret\nx.https-only=false\n".to_string(),
        format!("x.host=http://127.0.0.1:{}\nx.username=alice\nx.password=s3cret\n", server.port),
    ] {
        let creds = CoursierCredentials { entries: scpm::credentials::parse_properties(&creds) };
        let items = fetch_items(&hello_lock(), &repo(false), &creds, cache.path(), &mut Vec::new()).unwrap();
        assert!(matches!(items[0].auth, Ok(None)) && items[0].challenge.is_empty(), "{creds:?}");
        let items = fetch_items(&hello_lock(), &repo(true), &creds, cache.path(), &mut Vec::new()).unwrap();
        assert!(matches!(&items[0].auth, Ok(Some(Auth::Basic { user, .. })) if user == "alice"), "{creds:?}");
    }
}

#[test]
fn https_only_coursier_credentials_are_not_sent_over_http() {
    let creds = CoursierCredentials::from_env_value("127.0.0.1 alice:s3cret");
    let repos = vec![Repo {
        id: "local".into(),
        url: "http://127.0.0.1:1/maven/".into(),
        credentials: None,
        allow_insecure_credentials: false,
        ..Default::default()
    }];
    let cache = tempfile::tempdir().unwrap();
    let items = fetch_items(&hello_lock(), &repos, &creds, cache.path(), &mut Vec::new()).unwrap();
    assert!(matches!(items[0].auth, Ok(None)));
    let attempted: BTreeMap<_, _> = BTreeMap::from([("dest", items[0].dest.clone())]);
    assert!(!attempted["dest"].to_string_lossy().contains("alice"));
}
