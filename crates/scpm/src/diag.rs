//! User-facing diagnostics for configuration errors.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// One problem, pointing at a file (and position, when known).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// Dotted key path inside the file, e.g. `dependencies.org.typelevel::cats-core`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn new(message: impl Into<String>) -> Self {
        Diagnostic { file: None, line: None, column: None, key: None, message: message.into(), help: None }
    }

    pub fn in_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.file = Some(file.into());
        self
    }

    pub fn at_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Builds a diagnostic from a YAML deserialization error, rewriting serde's
    /// "unknown field" messages into "did you mean" help.
    pub fn from_yaml(file: &Path, err: serde_path_to_error::Error<serde_yaml_ng::Error>) -> Self {
        let path = err.path().to_string();
        let inner = err.into_inner();
        let location = inner.location();
        let raw = inner.to_string();
        let message = strip_location(&raw);
        // serde_yaml prefixes nested errors with (part of) the key path, which we report
        // separately.
        let message = match message.split_once(": ") {
            Some((prefix, rest)) if path.starts_with(prefix) => rest.to_string(),
            _ => message,
        };
        let mut diag = Diagnostic::new(message).in_file(file);
        diag.line = location.as_ref().map(|l| l.line());
        diag.column = location.as_ref().map(|l| l.column());
        if path != "." && !path.is_empty() {
            diag.key = Some(path);
        }
        suggest(diag)
    }

    /// Same as [`Diagnostic::from_yaml`] for errors without a source position (merged values).
    pub fn from_value_error(file: &Path, err: serde_path_to_error::Error<serde_yaml_ng::Error>) -> Self {
        let mut diag = Diagnostic::from_yaml(file, err);
        diag.line = None;
        diag.column = None;
        diag
    }
}

fn strip_location(message: &str) -> String {
    match message.rfind(" at line ") {
        Some(i) => message[..i].to_string(),
        None => message.to_string(),
    }
}

/// Turns ``unknown field `x`, expected one of `a`, `b` `` into a short message plus help.
fn suggest(mut diag: Diagnostic) -> Diagnostic {
    let kind = if diag.message.starts_with("unknown field") {
        "key"
    } else if diag.message.starts_with("unknown variant") {
        "value"
    } else {
        return diag;
    };
    let ticked: Vec<&str> = diag.message.split('`').skip(1).step_by(2).collect();
    let Some((unknown, expected)) = ticked.split_first() else { return diag };
    let unknown = unknown.to_string();
    if kind == "key" {
        // The path already ends at the unknown key; point at its parent instead.
        diag.key = diag.key.as_deref().and_then(|k| k.rsplit_once('.')).map(|(parent, _)| parent.to_string());
    }
    diag.help = Some(match closest(&unknown, expected.iter().copied()) {
        Some(e) => format!("did you mean `{e}`?"),
        None => format!("expected one of: {}", expected.join(", ")),
    });
    diag.message = format!("unknown {kind} `{unknown}`");
    diag
}

/// The candidate most similar to `word`, if any is close enough to be a likely typo.
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let max_edits = (word.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .map(|c| (strsim::osa_distance(word, c), strsim::jaro_winkler(word, c), c))
        .filter(|(edits, similarity, _)| *edits <= max_edits || *similarity >= 0.88)
        .min_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)))
        .map(|(_, _, c)| c)
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}", file.display())?;
            if let Some(line) = self.line {
                write!(f, ":{line}")?;
                if let Some(col) = self.column {
                    write!(f, ":{col}")?;
                }
            }
            f.write_str(": ")?;
        }
        if let Some(key) = &self.key {
            write!(f, "{key}: ")?;
        }
        f.write_str(&self.message)?;
        if let Some(help) = &self.help {
            write!(f, "\n  help: {help}")?;
        }
        Ok(())
    }
}

/// A non-empty collection of diagnostics, returned when configuration is invalid.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Diagnostics(pub Vec<Diagnostic>);

impl Diagnostics {
    pub fn push(&mut self, d: Diagnostic) {
        self.0.push(d);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `Ok(value)` if no diagnostics were collected.
    pub fn into_result<T>(self, value: T) -> Result<T, Diagnostics> {
        if self.0.is_empty() { Ok(value) } else { Err(self) }
    }
}

impl From<Diagnostic> for Diagnostics {
    fn from(d: Diagnostic) -> Self {
        Diagnostics(vec![d])
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "error: {d}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostics {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModuleFile;

    fn parse_err(yaml: &str) -> Diagnostic {
        let de = serde_yaml_ng::Deserializer::from_str(yaml);
        let err = serde_path_to_error::deserialize::<_, ModuleFile>(de).unwrap_err();
        Diagnostic::from_yaml(Path::new("scpm.yaml"), err)
    }

    #[test]
    fn did_you_mean_for_unknown_keys() {
        let d = parse_err("name: core\ndependecies:\n  a:b: '1'\n");
        assert_eq!(d.message, "unknown key `dependecies`");
        assert_eq!(d.help.as_deref(), Some("did you mean `dependencies`?"));
        assert_eq!(d.line, Some(2));
    }

    #[test]
    fn nested_key_path_and_float_versions() {
        let d = parse_err("dependencies:\n  org.typelevel::cats-core: 2.10\n");
        assert_eq!(d.key.as_deref(), Some("dependencies.org.typelevel::cats-core"));
        assert!(d.message.contains("must be quoted"), "{}", d.message);
    }

    #[test]
    fn unknown_enum_value() {
        let d = parse_err("platforms: [jmv]\n");
        assert_eq!(d.message, "unknown value `jmv`");
        assert_eq!(d.help.as_deref(), Some("did you mean `jvm`?"));
    }
}
