//! Version specifications as written in manifests and catalogs.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// A version string from YAML. Unquoted integers (`release: 17`) are read as strings, but
/// unquoted floats are rejected: YAML reads `2.10` as the float `2.1`, which silently changes
/// the version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Ver(pub String);

impl JsonSchema for Ver {
    fn schema_name() -> Cow<'static, str> {
        "Ver".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A version. Quote versions that contain a dot (`'2.10'`): YAML reads unquoted ones as floats.",
            "type": ["string", "integer"]
        })
    }
}

impl Ver {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Ver {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Ver;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a version string")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Ver, E> {
                if v.trim().is_empty() {
                    return Err(E::custom("version must not be empty"));
                }
                Ok(Ver(v.trim().to_string()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Ver, E> {
                Err(E::custom(format!("versions must be quoted strings: YAML reads this unquoted number as `{v}`")))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Ver, E> {
                Ok(Ver(v.to_string()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Ver, E> {
                Ok(Ver(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

/// A parsed dependency version specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VersionSpec {
    /// An exact version, e.g. `2.13.0`.
    Exact { version: String },
    /// An npm-style or Maven range, e.g. `^0.14.10`, `~1.2.3`, `>=1.2 <2`, `[1.0,2.0)`.
    /// Resolved to an exact version at lock time.
    Range { range: String },
    /// `catalog` (default catalog; also `catalog:default`) or `catalog:<name>`.
    Catalog { catalog: Option<String> },
    /// `workspace:*`, `workspace:^`, `workspace:~` or `workspace:<version>`: another module.
    Workspace { range: String },
}

impl VersionSpec {
    pub fn parse(s: &str) -> VersionSpec {
        let s = s.trim();
        // `catalog` (bare, because a trailing `:` is not valid in a YAML value), `catalog:` and
        // `catalog:default` all mean the default catalog.
        if s == "catalog" {
            return VersionSpec::Catalog { catalog: None };
        }
        if let Some(rest) = s.strip_prefix("catalog:") {
            let name = rest.trim();
            let named = !name.is_empty() && name != "default";
            return VersionSpec::Catalog { catalog: named.then(|| name.to_string()) };
        }
        if let Some(rest) = s.strip_prefix("workspace:") {
            let range = if rest.trim().is_empty() { "*" } else { rest.trim() };
            return VersionSpec::Workspace { range: range.to_string() };
        }
        let is_range = s.starts_with(['^', '~', '>', '<', '=', '[', '('])
            || s.contains(' ')
            || s.contains('*')
            || s.contains(".x")
            // Ivy dynamic revisions: `1.2.+`, `1.2+`, `+`.
            || s.ends_with('+');
        if is_range {
            VersionSpec::Range { range: s.to_string() }
        } else {
            VersionSpec::Exact { version: s.to_string() }
        }
    }
}

impl fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionSpec::Exact { version } => f.write_str(version),
            VersionSpec::Range { range } => f.write_str(range),
            VersionSpec::Catalog { catalog: None } => f.write_str("catalog"),
            VersionSpec::Catalog { catalog: Some(c) } => write!(f, "catalog:{c}"),
            VersionSpec::Workspace { range } => write!(f, "workspace:{range}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs() {
        assert_eq!(VersionSpec::parse("2.13.0"), VersionSpec::Exact { version: "2.13.0".into() });
        assert_eq!(VersionSpec::parse("33.4.0-jre"), VersionSpec::Exact { version: "33.4.0-jre".into() });
        assert_eq!(VersionSpec::parse("^0.14.10"), VersionSpec::Range { range: "^0.14.10".into() });
        assert_eq!(VersionSpec::parse("[1.0,2.0)"), VersionSpec::Range { range: "[1.0,2.0)".into() });
        assert_eq!(VersionSpec::parse(">=1.2 <2"), VersionSpec::Range { range: ">=1.2 <2".into() });
        assert_eq!(VersionSpec::parse("1.2.+"), VersionSpec::Range { range: "1.2.+".into() });
        assert_eq!(VersionSpec::parse("1.0.0+build.5"), VersionSpec::Exact { version: "1.0.0+build.5".into() });
        assert_eq!(VersionSpec::parse("catalog"), VersionSpec::Catalog { catalog: None });
        assert_eq!(VersionSpec::parse("catalog:"), VersionSpec::Catalog { catalog: None });
        assert_eq!(VersionSpec::parse("catalog:default"), VersionSpec::Catalog { catalog: None });
        assert_eq!(VersionSpec::parse("catalog:spark"), VersionSpec::Catalog { catalog: Some("spark".into()) });
        assert_eq!(VersionSpec::parse("workspace:*"), VersionSpec::Workspace { range: "*".into() });
        assert_eq!(VersionSpec::parse("workspace:"), VersionSpec::Workspace { range: "*".into() });
    }

    #[test]
    fn rejects_unquoted_floats() {
        let err = serde_yaml_ng::from_str::<Ver>("2.10").unwrap_err().to_string();
        assert!(err.contains("must be quoted"), "{err}");
        assert_eq!(serde_yaml_ng::from_str::<Ver>("'2.10'").unwrap(), Ver("2.10".into()));
        assert_eq!(serde_yaml_ng::from_str::<Ver>("3.7.4").unwrap(), Ver("3.7.4".into()));
    }

    #[test]
    fn integers_are_versions() {
        assert_eq!(serde_yaml_ng::from_str::<Ver>("17").unwrap(), Ver("17".into()));
        assert_eq!(serde_yaml_ng::from_str::<Ver>("'17'").unwrap(), Ver("17".into()));
    }
}
