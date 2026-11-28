//! Systems: an OS and CPU architecture pair, such as `linux-x86_64` (design §5.4, "Systems"), and
//! the host system, which is the project JDK's.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A system scpm knows the JVM properties for (design §5.2.1, "Resolution context").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownSystem {
    pub name: &'static str,
    /// `os.name` a JVM on this system reports.
    pub os_name: &'static str,
    /// `os.arch` a JVM on this system reports.
    pub os_arch: &'static str,
    pub path_separator: &'static str,
}

/// Every system `lock.systems` may name, sorted by name.
pub const KNOWN: &[KnownSystem] = &[
    KnownSystem { name: "linux-aarch64", os_name: "Linux", os_arch: "aarch64", path_separator: ":" },
    KnownSystem { name: "linux-x86_64", os_name: "Linux", os_arch: "amd64", path_separator: ":" },
    KnownSystem { name: "macos-aarch64", os_name: "Mac OS X", os_arch: "aarch64", path_separator: ":" },
    KnownSystem { name: "macos-x86_64", os_name: "Mac OS X", os_arch: "x86_64", path_separator: ":" },
    KnownSystem { name: "windows-aarch64", os_name: "Windows 11", os_arch: "aarch64", path_separator: ";" },
    KnownSystem { name: "windows-x86_64", os_name: "Windows 11", os_arch: "amd64", path_separator: ";" },
];

/// The version of scpm's default system set. Until M2 of the resolver sidecar it is version 1,
/// a single system (design §5.4).
pub const DEFAULT_SYSTEMS_VERSION: u32 = 1;

/// scpm's default system set (version [`DEFAULT_SYSTEMS_VERSION`]), sorted.
pub const DEFAULT_SYSTEMS: &[&str] = &["linux-x86_64"];

pub fn known(name: &str) -> Option<&'static KnownSystem> {
    KNOWN.iter().find(|s| s.name == name)
}

/// The known system names, for messages.
pub fn known_names() -> String {
    KNOWN.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
}

/// A sorted, de-duplicated system set.
pub fn normalized(systems: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    let mut list: Vec<String> = systems.into_iter().map(Into::into).collect();
    list.sort();
    list.dedup();
    list
}

/// The system of an OS and architecture as a JDK's `release` file (`OS_NAME`, `OS_ARCH`) or a
/// Rust target (`std::env::consts`) names them: `Darwin`/`macos` and `aarch64` → `macos-aarch64`.
pub fn from_os_arch(os: &str, arch: &str) -> Option<String> {
    let os = match os.trim().to_ascii_lowercase().as_str() {
        "linux" => "linux",
        "darwin" | "mac os x" | "macos" | "osx" => "macos",
        w if w.starts_with("windows") => "windows",
        _ => return None,
    };
    let arch = match arch.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x86_64".to_string(),
        "aarch64" | "arm64" => "aarch64".to_string(),
        "" => return None,
        other => other.to_string(),
    };
    Some(format!("{os}-{arch}"))
}

/// The system a JDK's `release` file names (`OS_NAME`, `OS_ARCH`), if it names one.
pub fn from_release(text: &str) -> Option<String> {
    let value = |key: &str| {
        text.lines().find_map(|l| {
            let v = l.trim().strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
            Some(v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v).to_string())
        })
    };
    from_os_arch(&value("OS_NAME")?, &value("OS_ARCH")?)
}

/// The system of the JDK at `home`, from its `release` file.
pub fn of_jdk(home: &Path) -> Option<String> {
    from_release(&std::fs::read_to_string(home.join("release")).ok()?)
}

/// The OS and architecture scpm itself was built for.
pub fn own() -> String {
    from_os_arch(std::env::consts::OS, std::env::consts::ARCH)
        .unwrap_or_else(|| format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH))
}

/// The host system (design §5.4, "Using it").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub system: String,
    /// The project JDK, when its `release` file named the system.
    pub jdk: Option<PathBuf>,
    /// Set when the project JDK couldn't be read and scpm fell back to its own OS and arch.
    pub note: Option<String>,
}

/// The project JDK: the JDK that runs `run` and `test`. Until scpm provisions `toolchain.jdk`
/// itself (design §6.3), that is the JDK `detect_java` finds, so this is its home.
pub fn project_jdk() -> Option<PathBuf> {
    crate::worker::java_home()
}

/// The JDK the compile worker runs on: the one `detect_java` finds.
pub fn worker_jdk() -> Option<PathBuf> {
    crate::worker::java_home()
}

/// The host system: the project JDK's, read from its `release` file, else (with a note) the OS
/// and architecture scpm runs on. Computed once per process.
pub fn host() -> &'static Host {
    static HOST: OnceLock<Host> = OnceLock::new();
    HOST.get_or_init(|| host_from(project_jdk(), own()))
}

fn host_from(project_jdk: Option<PathBuf>, own: String) -> Host {
    match project_jdk.as_deref().and_then(|home| Some((home, of_jdk(home)?))) {
        Some((home, system)) => Host { system, jdk: Some(home.to_path_buf()), note: None },
        None => {
            let why = match &project_jdk {
                Some(home) => format!("the release file of the project JDK {} names no OS and arch", home.display()),
                None => "no project JDK was found".to_string(),
            };
            Host {
                note: Some(format!("{why}, so the host system is {own}, the one scpm runs on")),
                system: own,
                jdk: None,
            }
        }
    }
}

/// The compile worker's JDK must be the host system's: macros and compiler plugins run inside
/// it and load natives from the compile classpath, which is the host's projection (design §5.4).
pub fn check_worker() -> Result<(), String> {
    let host = host();
    let (Some(project), Some(worker)) = (&host.jdk, worker_jdk()) else { return Ok(()) };
    check_worker_system(project, &host.system, &worker, of_jdk(&worker).as_deref())
}

fn check_worker_system(project: &Path, host: &str, worker: &Path, worker_system: Option<&str>) -> Result<(), String> {
    match worker_system {
        Some(w) if w != host => Err(format!(
            "the compile worker's JDK {} is for {w}, but the project JDK {} is for {host}; macros and compiler plugins \
             would load natives for another system than the one the project runs on\n  help: point SCPM_JAVA (or \
             SCPM_JAVA_HOME) at a JDK for {host}",
            worker.display(),
            project.display()
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_files_name_systems() {
        assert_eq!(from_release("OS_ARCH=\"aarch64\"\nOS_NAME=\"Darwin\"\n").as_deref(), Some("macos-aarch64"));
        assert_eq!(from_release("OS_NAME=\"Linux\"\nOS_ARCH=\"amd64\"\n").as_deref(), Some("linux-x86_64"));
        assert_eq!(from_release("OS_NAME=\"Windows\"\nOS_ARCH=\"x86_64\"\n").as_deref(), Some("windows-x86_64"));
        assert_eq!(from_release("OS_NAME=Linux\nOS_ARCH=arm64\n").as_deref(), Some("linux-aarch64"));
        assert_eq!(from_release("OS_NAME=\"Linux\"\n"), None);
        assert_eq!(from_release("OS_NAME=\"Plan9\"\nOS_ARCH=\"mips\"\n"), None);
        assert_eq!(from_os_arch("macos", "aarch64").as_deref(), Some("macos-aarch64"));
    }

    #[test]
    fn the_known_table_is_sorted_and_holds_the_defaults() {
        let names: Vec<&str> = KNOWN.iter().map(|s| s.name).collect();
        assert_eq!(names, normalized(names.clone()));
        assert!(DEFAULT_SYSTEMS.iter().all(|s| known(s).is_some()));
        assert_eq!(normalized(DEFAULT_SYSTEMS.iter().copied()), DEFAULT_SYSTEMS);
    }

    #[test]
    fn the_host_is_the_project_jdks_system() {
        let jdk = tempfile::tempdir().unwrap();
        std::fs::write(jdk.path().join("release"), "OS_NAME=\"Linux\"\nOS_ARCH=\"x86_64\"\n").unwrap();
        let host = host_from(Some(jdk.path().to_path_buf()), "macos-aarch64".into());
        assert_eq!(host.system, "linux-x86_64", "an x86_64 JDK under Rosetta gets x86_64 natives");
        assert_eq!(host.note, None);
        let fallback = host_from(None, "macos-aarch64".into());
        assert_eq!(fallback.system, "macos-aarch64");
        assert!(fallback.note.unwrap().contains("no project JDK was found"));
    }

    #[test]
    fn a_worker_jdk_for_another_system_is_an_error() {
        let (p, w) = (Path::new("/jdk/x64"), Path::new("/jdk/arm"));
        let err = check_worker_system(p, "macos-x86_64", w, Some("macos-aarch64")).unwrap_err();
        assert!(err.contains("/jdk/arm is for macos-aarch64") && err.contains("/jdk/x64 is for macos-x86_64"), "{err}");
        assert!(err.contains("SCPM_JAVA"), "{err}");
        assert!(check_worker_system(p, "macos-x86_64", w, Some("macos-x86_64")).is_ok());
        assert!(check_worker_system(p, "macos-x86_64", w, None).is_ok(), "unknown: not an error");
    }
}
