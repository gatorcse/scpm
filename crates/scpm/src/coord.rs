//! Maven coordinates in the Scala convention: `org:name` (Java), `org::name` (Scala binary
//! version suffix), `org:::name` (full Scala version suffix).

use std::fmt;

use serde::Serialize;

use crate::platform::Platform;

/// How a coordinate's artifact name is suffixed for Scala.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CrossKind {
    /// `org:name` — no suffix.
    Java,
    /// `org::name` — `_<platform><scala binary>`, e.g. `_3`, `_sjs1_2.13`.
    Binary,
    /// `org:::name` — `_<full scala version>`, used for compiler plugins. Compiler plugins run
    /// in the JVM compiler, so there is no platform prefix on any platform.
    Full,
}

/// A dependency coordinate without a version, as written in manifests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Coord {
    pub org: String,
    pub name: String,
    pub cross: CrossKind,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid coordinate `{input}`: {reason}")]
pub struct CoordError {
    pub input: String,
    pub reason: &'static str,
}

impl Coord {
    /// Parses `org:name`, `org::name` or `org:::name`.
    pub fn parse(input: &str) -> Result<Coord, CoordError> {
        let err = |reason| CoordError { input: input.to_string(), reason };
        let (org, rest, cross) = if let Some((org, rest)) = input.split_once(":::") {
            (org, rest, CrossKind::Full)
        } else if let Some((org, rest)) = input.split_once("::") {
            (org, rest, CrossKind::Binary)
        } else if let Some((org, rest)) = input.split_once(':') {
            (org, rest, CrossKind::Java)
        } else {
            return Err(err("expected `org:name`, `org::name` or `org:::name`"));
        };
        if org.is_empty() || rest.is_empty() {
            return Err(err("organization and name must be non-empty"));
        }
        if rest.contains(':') {
            return Err(err("too many `:` separators (versions go in the value, not the key)"));
        }
        let valid = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        if !valid(org) || !valid(rest) {
            return Err(err("only letters, digits, `.`, `_` and `-` are allowed"));
        }
        Ok(Coord { org: org.to_string(), name: rest.to_string(), cross })
    }

    /// The published artifact name for a given Scala version and platform.
    pub fn artifact_name(&self, scala_version: &str, platform: Platform) -> String {
        match self.cross {
            CrossKind::Java => self.name.clone(),
            CrossKind::Binary => {
                format!("{}_{}{}", self.name, platform.artifact_prefix(), scala_binary_version(scala_version))
            }
            CrossKind::Full => format!("{}_{}", self.name, scala_version),
        }
    }
}

impl fmt::Display for Coord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sep = match self.cross {
            CrossKind::Java => ":",
            CrossKind::Binary => "::",
            CrossKind::Full => ":::",
        };
        write!(f, "{}{}{}", self.org, sep, self.name)
    }
}

impl Serialize for Coord {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// `3.7.4` → `3`, `2.13.18` → `2.13`, `3.8.0-RC1` → `3`.
pub fn scala_binary_version(full: &str) -> String {
    let mut parts = full.split(['.', '-']);
    match (parts.next(), parts.next()) {
        (Some("3"), _) => "3".to_string(),
        (Some(major), Some(minor)) => format!("{major}.{minor}"),
        _ => full.to_string(),
    }
}

/// A fully qualified `org:name:version` coordinate (BOMs, overrides of specific modules).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct VersionedCoord {
    pub coord: Coord,
    pub version: String,
}

impl VersionedCoord {
    /// Parses `org:name:version`, `org::name:version`, `org:::name:version`.
    pub fn parse(input: &str) -> Result<VersionedCoord, CoordError> {
        let Some((coord, version)) = input.rsplit_once(':') else {
            return Err(CoordError { input: input.to_string(), reason: "expected `org:name:version`" });
        };
        if version.is_empty() || coord.ends_with(':') {
            return Err(CoordError { input: input.to_string(), reason: "expected `org:name:version`" });
        }
        Ok(VersionedCoord { coord: Coord::parse(coord)?, version: version.to_string() })
    }
}

impl fmt::Display for VersionedCoord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.coord, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cross_kinds() {
        let java = Coord::parse("com.acme:auth-client").unwrap();
        assert_eq!(java.cross, CrossKind::Java);
        let bin = Coord::parse("org.typelevel::cats-core").unwrap();
        assert_eq!((bin.org.as_str(), bin.name.as_str(), bin.cross), ("org.typelevel", "cats-core", CrossKind::Binary));
        let full = Coord::parse("org.typelevel:::kind-projector").unwrap();
        assert_eq!(full.cross, CrossKind::Full);
        assert_eq!(full.to_string(), "org.typelevel:::kind-projector");
    }

    #[test]
    fn rejects_bad_coordinates() {
        for bad in ["cats", ":x", "a:", "a::b:1.0", "a b:c", "a::"] {
            assert!(Coord::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn artifact_names() {
        let c = Coord::parse("org.typelevel::cats-core").unwrap();
        assert_eq!(c.artifact_name("3.7.4", Platform::Jvm), "cats-core_3");
        assert_eq!(c.artifact_name("2.13.18", Platform::Js), "cats-core_sjs1_2.13");
        assert_eq!(c.artifact_name("3.7.4", Platform::Native), "cats-core_native0.5_3");
        let full = Coord::parse("org.typelevel:::kind-projector").unwrap();
        assert_eq!(full.artifact_name("2.13.18", Platform::Jvm), "kind-projector_2.13.18");
        let plugin = Coord::parse("org.scala-js:::scalajs-compiler").unwrap();
        assert_eq!(plugin.artifact_name("2.13.18", Platform::Js), "scalajs-compiler_2.13.18");
        assert_eq!(plugin.artifact_name("2.13.18", Platform::Native), "scalajs-compiler_2.13.18");
    }

    #[test]
    fn binary_versions() {
        assert_eq!(scala_binary_version("3.7.4"), "3");
        assert_eq!(scala_binary_version("3.8.0-RC1"), "3");
        assert_eq!(scala_binary_version("2.13.18"), "2.13");
        assert_eq!(scala_binary_version("2.12.20"), "2.12");
    }

    #[test]
    fn versioned() {
        let v = VersionedCoord::parse("com.fasterxml.jackson:jackson-bom:2.20.1").unwrap();
        assert_eq!(v.version, "2.20.1");
        assert_eq!(v.coord.name, "jackson-bom");
        assert!(VersionedCoord::parse("com.fasterxml.jackson:jackson-bom").is_err());
    }
}
