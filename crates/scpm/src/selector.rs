//! Variant selectors (design §4.4): the keys of `variants` and the part after `@` in filters.
//!
//! A selector is `<platform>`, `<scala>` or `<platform>-<scala>`. `<platform>` is `jvm`, `js`
//! or `native`; `<scala>` is a binary version (`3`, `2.13`) or a full version (`3.9.0`,
//! `3.8.0-RC1`; a full version may itself contain `-`). A selector matches a
//! variant if every given part matches; a Scala part never matches a Java-only variant.
//!
//! Filters may use `*` globs in either part (`jvm-*`, `*-2.13`).

use globset::{GlobBuilder, GlobMatcher};

use crate::platform::Platform;
use crate::workspace::ScalaVersion;

#[derive(Debug, Clone)]
pub struct VariantSelector {
    platform: Option<PlatformPart>,
    scala: Option<ScalaPart>,
}

#[derive(Debug, Clone)]
enum PlatformPart {
    Exact(Platform),
    Glob(GlobMatcher),
}

#[derive(Debug, Clone)]
enum ScalaPart {
    /// `3`, `2.13`: every version with that binary version.
    Binary(String),
    /// `3.9.0`, `3.8.0-RC1`: exactly that version.
    Full(String),
    /// Matches the binary or the full version.
    Glob(GlobMatcher),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SelectorError {
    message: String,
    help: Option<String>,
}

impl SelectorError {
    fn new(message: impl Into<String>) -> Self {
        SelectorError { message: message.into(), help: None }
    }

    /// A hint on how to fix the selector, if there is one.
    pub fn help(&self) -> Option<&str> {
        self.help.as_deref()
    }
}

impl VariantSelector {
    /// Parses a selector without globs, as used for `variants` keys.
    pub fn parse(s: &str) -> Result<VariantSelector, SelectorError> {
        parse(s, false)
    }

    /// Parses a selector that may contain `*`-style globs, as used in filters.
    pub fn parse_glob(s: &str) -> Result<VariantSelector, SelectorError> {
        parse(s, true)
    }

    pub fn matches(&self, platform: Platform, scala: Option<&ScalaVersion>) -> bool {
        let platform_ok = match &self.platform {
            None => true,
            Some(PlatformPart::Exact(p)) => *p == platform,
            Some(PlatformPart::Glob(g)) => g.is_match(platform.as_str()),
        };
        let scala_ok = match (&self.scala, scala) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(ScalaPart::Binary(b)), Some(s)) => s.binary == *b,
            (Some(ScalaPart::Full(f)), Some(s)) => s.full == *f,
            (Some(ScalaPart::Glob(g)), Some(s)) => g.is_match(&s.binary) || g.is_match(&s.full),
        };
        platform_ok && scala_ok
    }
}

fn parse(s: &str, globs: bool) -> Result<VariantSelector, SelectorError> {
    if s.is_empty() {
        return Err(SelectorError::new("empty selector"));
    }
    if is_platform_part(s, globs) {
        return Ok(VariantSelector { platform: Some(platform_part(s)?), scala: None });
    }
    if let Some((head, rest)) = s.split_once('-')
        && !rest.is_empty()
        && is_platform_part(head, globs)
    {
        return Ok(VariantSelector { platform: Some(platform_part(head)?), scala: Some(scala_part(rest, globs)?) });
    }
    Ok(VariantSelector { platform: None, scala: Some(scala_part(s, globs)?) })
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '[', ']', '{', '}'])
}

/// A known platform, or (in filters) a glob that can only be meant for one: it has glob
/// characters and none of the digits, dots or dashes of a Scala version.
fn is_platform_part(s: &str, globs: bool) -> bool {
    s.parse::<Platform>().is_ok()
        || (globs && has_glob(s) && !s.contains(|c: char| c.is_ascii_digit() || c == '.' || c == '-'))
}

fn platform_part(s: &str) -> Result<PlatformPart, SelectorError> {
    match s.parse::<Platform>() {
        Ok(p) => Ok(PlatformPart::Exact(p)),
        Err(_) => glob(s).map(PlatformPart::Glob),
    }
}

fn scala_part(s: &str, globs: bool) -> Result<ScalaPart, SelectorError> {
    if globs && has_glob(s) {
        return glob(s).map(ScalaPart::Glob);
    }
    let numeric = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit());
    let parts: Vec<&str> = s.split('.').collect();
    match parts.as_slice() {
        // Only Scala 3 has a one-part binary version; `2` would silently match nothing.
        [major] if numeric(major) && *major != "3" => Err(SelectorError {
            message: format!("`{s}` is not a Scala binary version"),
            help: Some("use a binary version such as 2.12 or 2.13".into()),
        }),
        [major] if numeric(major) => Ok(ScalaPart::Binary(s.to_string())),
        [major, minor] if numeric(major) && numeric(minor) => Ok(ScalaPart::Binary(s.to_string())),
        _ if full_version_parts(s).is_some() => Ok(ScalaPart::Full(s.to_string())),
        _ => Err(SelectorError::new(format!(
            "`{s}` is neither a platform (jvm, js, native) nor a Scala version (`3`, `2.13`, `3.9.0`); \
             selectors are `<platform>`, `<scala>` or `<platform>-<scala>`"
        ))),
    }
}

fn glob(s: &str) -> Result<GlobMatcher, SelectorError> {
    GlobBuilder::new(s).build().map(|g| g.compile_matcher()).map_err(|e| SelectorError::new(e.to_string()))
}

/// Splits a full version `major.minor.patch[-qualifier]` into its numeric parts and qualifier.
pub fn full_version_parts(s: &str) -> Option<(&str, &str, &str, Option<&str>)> {
    let (numbers, qualifier) = match s.split_once('-') {
        Some((n, q)) => (n, Some(q)),
        None => (s, None),
    };
    let numeric = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit());
    let mut it = numbers.split('.');
    let (major, minor, patch) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || ![major, minor, patch].into_iter().all(numeric) {
        return None;
    }
    if let Some(q) = qualifier
        && (q.is_empty() || !q.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
    {
        return None;
    }
    Some((major, minor, patch, qualifier))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(full: &str, binary: &str) -> ScalaVersion {
        ScalaVersion { full: full.into(), binary: binary.into() }
    }

    fn matches(selector: &str, platform: Platform, scala: Option<&ScalaVersion>) -> bool {
        VariantSelector::parse_glob(selector).unwrap().matches(platform, scala)
    }

    #[test]
    fn grammar() {
        let s3 = sv("3.9.0", "3");
        let s213 = sv("2.13.18", "2.13");
        let rc = sv("3.8.0-RC1", "3");
        use Platform::*;
        assert!(matches("jvm", Jvm, Some(&s3)));
        assert!(matches("jvm", Jvm, None));
        assert!(!matches("js", Jvm, Some(&s3)));
        assert!(matches("3", Jvm, Some(&s3)));
        assert!(!matches("3", Jvm, None), "a Scala part never matches a Java-only variant");
        assert!(matches("2.13", Js, Some(&s213)));
        assert!(!matches("2.13", Jvm, Some(&s3)));
        assert!(matches("3.9.0", Jvm, Some(&s3)));
        assert!(!matches("3.9.1", Jvm, Some(&s3)));
        assert!(matches("jvm-3", Jvm, Some(&s3)));
        assert!(!matches("js-3", Jvm, Some(&s3)));
        assert!(matches("3.8.0-RC1", Jvm, Some(&rc)));
        assert!(matches("jvm-3.8.0-RC1", Jvm, Some(&rc)));
        assert!(matches("jvm-*", Jvm, Some(&s3)));
        assert!(!matches("jvm-*", Jvm, None));
        assert!(matches("*-2.13", Jvm, Some(&s213)));
        assert!(matches("*", Jvm, None));
        assert!(matches("2.*", Jvm, Some(&s213)));
    }

    #[test]
    fn rejects_unknown_parts() {
        for bad in ["foo", "jvm-foo", "jvm-", "-3", "3.x", "2.13.", "jvm-3-", "3.9.0-", "*", "2", "jvm-2", "4"] {
            assert!(VariantSelector::parse(bad).is_err(), "{bad}");
        }
        assert!(VariantSelector::parse_glob("jvm-foo").is_err());
        assert!(VariantSelector::parse("native-2.13").is_ok());
        let err = VariantSelector::parse_glob("2").unwrap_err();
        assert_eq!(err.help(), Some("use a binary version such as 2.12 or 2.13"));
    }

    #[test]
    fn splits_full_versions() {
        assert_eq!(full_version_parts("3.8.0-RC1"), Some(("3", "8", "0", Some("RC1"))));
        assert_eq!(full_version_parts("2.13.18"), Some(("2", "13", "18", None)));
        assert_eq!(full_version_parts("3.9"), None);
        assert_eq!(full_version_parts("3.9.0.1"), None);
    }
}
