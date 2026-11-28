//! Native mappings (design §5.4, "Native mappings"): which classifier jar each system gets for a
//! module that publishes its natives as classifiers. Only the resolver sidecar applies them.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::semver::MavenVersion;

/// A version range `[from, to)`; an absent bound is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub from: Option<&'static str>,
    pub to: Option<&'static str>,
}

impl Range {
    pub const ALL: Range = Range { from: None, to: None };

    pub fn contains(&self, version: &str) -> bool {
        let v = MavenVersion::parse(version);
        self.from.is_none_or(|f| v >= MavenVersion::parse(f)) && self.to.is_none_or(|t| v < MavenVersion::parse(t))
    }

    /// As a Maven interval, for the sidecar: `[4.1.0,)`.
    pub fn interval(&self) -> Option<String> {
        (*self != Range::ALL).then(|| format!("[{},{})", self.from.unwrap_or(""), self.to.unwrap_or("")))
    }
}

/// A built-in mapping: for `module` at a version in `range`, each listed system's classifier.
#[derive(Debug, Clone, Copy)]
pub struct Mapping {
    pub module: &'static str,
    pub range: Range,
    pub natives: &'static [(&'static str, &'static str)],
}

/// One change to the table: the table version that made it, the module, and its touched range
/// (the union of the entry's range before and after the change; a deleted entry's last range).
#[derive(Debug, Clone, Copy)]
pub struct Change {
    pub version: u32,
    pub module: &'static str,
    pub touched: Range,
}

/// A mapping table with its version and change log, so a binary can list every range whose
/// mapping changed between two versions without shipping old tables.
#[derive(Debug, Clone, Copy)]
pub struct Table {
    pub version: u32,
    pub mappings: &'static [Mapping],
    pub changes: &'static [Change],
}

const LWJGL: &[(&str, &str)] = &[
    ("linux-aarch64", "natives-linux-arm64"),
    ("linux-x86_64", "natives-linux"),
    ("macos-aarch64", "natives-macos-arm64"),
    ("macos-x86_64", "natives-macos"),
    ("windows-aarch64", "natives-windows-arm64"),
    ("windows-x86_64", "natives-windows"),
];
/// LWJGL publishes `natives-macos-arm64` and the other arm64 classifiers from 3.3.0.
const LWJGL_RANGE: Range = Range { from: Some("3.3.0"), to: None };

// Netty, as published on Maven Central: epoll's x86_64 jar exists from 4.1.0; an aarch64 one
// from 4.1.50, named `linux-aarch64` until 4.1.54 and `linux-aarch_64` from 4.1.55. kqueue
// starts at 4.1.11 (`osx-x86_64`), with `osx-aarch_64` from 4.1.68. io_uring left the
// incubator in 4.2.0. Bounds carry `.Final`, as Netty's versions do.
const EPOLL: &str = "io.netty:netty-transport-native-epoll";
const KQUEUE: &str = "io.netty:netty-transport-native-kqueue";
const IO_URING: &str = "io.netty:netty-transport-native-io_uring";
const FROM_4_1_0: Range = Range { from: Some("4.1.0.Final"), to: None };
const AARCH64_4_1_50: Range = Range { from: Some("4.1.50.Final"), to: Some("4.1.55.Final") };
const FROM_4_1_55: Range = Range { from: Some("4.1.55.Final"), to: None };
const FROM_4_1_11: Range = Range { from: Some("4.1.11.Final"), to: None };
const FROM_4_1_68: Range = Range { from: Some("4.1.68.Final"), to: None };
const FROM_4_2_0: Range = Range { from: Some("4.2.0.Final"), to: None };

/// scpm's built-in table. Start small: the transports and libraries people hit first. A module
/// may have several rows, each for a version range and the systems it covers there; a system no
/// row covers at a version gets no native jar.
pub const BUILT_IN: Table = Table {
    version: 1,
    mappings: &[
        Mapping { module: EPOLL, range: FROM_4_1_0, natives: &[("linux-x86_64", "linux-x86_64")] },
        Mapping { module: EPOLL, range: AARCH64_4_1_50, natives: &[("linux-aarch64", "linux-aarch64")] },
        Mapping { module: EPOLL, range: FROM_4_1_55, natives: &[("linux-aarch64", "linux-aarch_64")] },
        Mapping { module: KQUEUE, range: FROM_4_1_11, natives: &[("macos-x86_64", "osx-x86_64")] },
        Mapping { module: KQUEUE, range: FROM_4_1_68, natives: &[("macos-aarch64", "osx-aarch_64")] },
        Mapping {
            module: IO_URING,
            range: FROM_4_2_0,
            natives: &[("linux-aarch64", "linux-aarch_64"), ("linux-x86_64", "linux-x86_64")],
        },
        Mapping { module: "org.lwjgl:lwjgl", range: LWJGL_RANGE, natives: LWJGL },
        Mapping { module: "org.lwjgl:lwjgl-glfw", range: LWJGL_RANGE, natives: LWJGL },
        Mapping { module: "org.lwjgl:lwjgl-jemalloc", range: LWJGL_RANGE, natives: LWJGL },
        Mapping { module: "org.lwjgl:lwjgl-openal", range: LWJGL_RANGE, natives: LWJGL },
        Mapping { module: "org.lwjgl:lwjgl-opengl", range: LWJGL_RANGE, natives: LWJGL },
        Mapping { module: "org.lwjgl:lwjgl-stb", range: LWJGL_RANGE, natives: LWJGL },
    ],
    changes: &[
        Change { version: 1, module: EPOLL, touched: FROM_4_1_0 },
        Change { version: 1, module: KQUEUE, touched: FROM_4_1_11 },
        Change { version: 1, module: IO_URING, touched: FROM_4_2_0 },
        Change { version: 1, module: "org.lwjgl:lwjgl", touched: LWJGL_RANGE },
        Change { version: 1, module: "org.lwjgl:lwjgl-glfw", touched: LWJGL_RANGE },
        Change { version: 1, module: "org.lwjgl:lwjgl-jemalloc", touched: LWJGL_RANGE },
        Change { version: 1, module: "org.lwjgl:lwjgl-openal", touched: LWJGL_RANGE },
        Change { version: 1, module: "org.lwjgl:lwjgl-opengl", touched: LWJGL_RANGE },
        Change { version: 1, module: "org.lwjgl:lwjgl-stb", touched: LWJGL_RANGE },
    ],
};

/// A mapping as the sidecar receives it (`natives` in a `resolve` request).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct NativeSpec {
    pub module: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions: Option<String>,
    pub natives: BTreeMap<String, String>,
}

/// The mappings for one JVM request: `overrides` (a direct dependency's `natives:`, then
/// `lock.nativeClassifiers`, the first for a module wins) replace the table's entry for their
/// module, at every version; `{}` turns a mapping off.
pub fn specs(table: &Table, overrides: &[(String, BTreeMap<String, String>)]) -> Vec<NativeSpec> {
    let mut out: Vec<NativeSpec> = Vec::new();
    for (module, natives) in overrides {
        if !out.iter().any(|s| &s.module == module) {
            out.push(NativeSpec { module: module.clone(), versions: None, natives: natives.clone() });
        }
    }
    for m in table.mappings {
        if overrides.iter().any(|(module, _)| module == m.module) {
            continue;
        }
        out.push(NativeSpec {
            module: m.module.to_string(),
            versions: m.range.interval(),
            natives: m.natives.iter().map(|(s, c)| (s.to_string(), c.to_string())).collect(),
        });
    }
    out
}

/// The classifier of `module`'s native jar for `system` at `version`, if a row maps one. The
/// sidecar applies the same rule to the specs it gets (the first row that maps the system).
pub fn classifier<'t>(table: &'t Table, module: &str, version: &str, system: &str) -> Option<&'t str> {
    table
        .mappings
        .iter()
        .filter(|m| m.module == module && m.range.contains(version))
        .find_map(|m| m.natives.iter().find(|(s, _)| *s == system).map(|(_, c)| *c))
}

/// Whether a lock key (`g:a:v[:classifier][@type]`) is a module at a version inside a touched
/// range of a change in `(from, to]` (design §5.4, the M > N rule).
pub fn touched(table: &Table, from: u32, to: u32, key: &str) -> bool {
    let mut parts = key.split('@').next().unwrap_or(key).split(':');
    let (Some(g), Some(a), Some(v)) = (parts.next(), parts.next(), parts.next()) else { return false };
    let module = format!("{g}:{a}");
    table.changes.iter().any(|c| c.version > from && c.version <= to && c.module == module && c.touched.contains(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_and_intervals() {
        assert!(FROM_4_1_0.contains("4.1.115.Final") && FROM_4_1_0.contains("4.2.0.Final"));
        assert!(!FROM_4_2_0.contains("4.1.115.Final"));
        assert_eq!(FROM_4_1_0.interval().as_deref(), Some("[4.1.0.Final,)"));
        assert_eq!(AARCH64_4_1_50.interval().as_deref(), Some("[4.1.50.Final,4.1.55.Final)"));
        assert_eq!(Range::ALL.interval(), None);
        let narrow = Range { from: Some("2"), to: Some("3") };
        assert!(narrow.contains("2.5") && !narrow.contains("3.0") && !narrow.contains("1.9"));
    }

    /// Netty's classifiers as published on Maven Central, per version.
    #[test]
    fn netty_classifiers_follow_what_central_publishes() {
        let c = |module, version, system| classifier(&BUILT_IN, module, version, system);
        assert_eq!(c(EPOLL, "4.1.30.Final", "linux-x86_64"), Some("linux-x86_64"));
        assert_eq!(c(EPOLL, "4.1.30.Final", "linux-aarch64"), None, "no aarch64 jar before 4.1.50");
        assert_eq!(c(EPOLL, "4.1.52.Final", "linux-aarch64"), Some("linux-aarch64"));
        assert_eq!(c(EPOLL, "4.1.55.Final", "linux-aarch64"), Some("linux-aarch_64"));
        assert_eq!(c(EPOLL, "4.1.115.Final", "linux-aarch64"), Some("linux-aarch_64"));
        assert_eq!(c(EPOLL, "4.1.115.Final", "macos-aarch64"), None);
        assert_eq!(c(KQUEUE, "4.1.10.Final", "macos-x86_64"), None, "kqueue starts at 4.1.11");
        assert_eq!(c(KQUEUE, "4.1.20.Final", "macos-x86_64"), Some("osx-x86_64"));
        assert_eq!(c(KQUEUE, "4.1.60.Final", "macos-aarch64"), None, "osx-aarch_64 from 4.1.68");
        assert_eq!(c(KQUEUE, "4.1.68.Final", "macos-aarch64"), Some("osx-aarch_64"));
        assert_eq!(c(IO_URING, "4.2.1.Final", "linux-aarch64"), Some("linux-aarch_64"));
        assert_eq!(c("org.lwjgl:lwjgl", "3.3.6", "windows-aarch64"), Some("natives-windows-arm64"));
        assert_eq!(c("org.lwjgl:lwjgl", "3.2.3", "linux-x86_64"), None);
    }

    #[test]
    fn the_table_is_consistent() {
        for m in BUILT_IN.mappings {
            assert!(m.natives.iter().all(|(s, _)| crate::system::known(s).is_some()), "{}", m.module);
            assert!(
                BUILT_IN.changes.iter().any(|c| c.module == m.module && c.version <= BUILT_IN.version),
                "{} has no change log",
                m.module
            );
        }
        assert!(BUILT_IN.changes.iter().all(|c| c.version <= BUILT_IN.version));
    }

    #[test]
    fn overrides_replace_the_built_in_entry() {
        let epoll = "io.netty:netty-transport-native-epoll".to_string();
        let mine = BTreeMap::from([("linux-x86_64".to_string(), "linux-x86_64".to_string())]);
        let specs = specs(&BUILT_IN, &[(epoll.clone(), mine.clone()), (epoll.clone(), BTreeMap::new())]);
        let found: Vec<&NativeSpec> = specs.iter().filter(|s| s.module == epoll).collect();
        assert_eq!(found, [&NativeSpec { module: epoll, versions: None, natives: mine }], "first wins, all versions");
        // epoll's three rows became the one override.
        assert_eq!(specs.len(), BUILT_IN.mappings.len() - 2);
    }

    /// Design §5.4's M > N rule, with a table that changed and deleted entries.
    #[test]
    fn touched_ranges_include_narrowed_and_deleted_entries() {
        const T: Table = Table {
            version: 3,
            mappings: &[],
            changes: &[
                Change { version: 1, module: "g:a", touched: Range { from: Some("1"), to: Some("4") } },
                // v2 narrowed g:a from [1,4) to [2,4): the touched range is their union.
                Change { version: 2, module: "g:a", touched: Range { from: Some("1"), to: Some("4") } },
                // v3 deleted g:b (a tombstone with its last range) and changed g:c 4.2 only.
                Change { version: 3, module: "g:b", touched: Range::ALL },
                Change { version: 3, module: "g:c", touched: Range { from: Some("4.2"), to: None } },
            ],
        };
        assert!(touched(&T, 1, 3, "g:a:1.5"), "narrowed away from 1.x: 1.x users re-lock");
        assert!(!touched(&T, 2, 3, "g:a:1.5"), "already at v2");
        assert!(touched(&T, 2, 3, "g:b:9@aar"), "tombstone");
        assert!(!touched(&T, 2, 3, "g:c:4.1.0"), "a 4.2 change doesn't re-lock 4.1 users");
        assert!(touched(&T, 2, 3, "g:c:4.2.1:linux"));
        assert!(!touched(&T, 3, 3, "g:b:9"));
    }
}
