use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The target platform of a module variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Jvm,
    Js,
    Native,
}

impl Platform {
    pub const ALL: [Platform; 3] = [Platform::Jvm, Platform::Js, Platform::Native];

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Jvm => "jvm",
            Platform::Js => "js",
            Platform::Native => "native",
        }
    }

    /// Prefix inserted before the Scala binary version in cross-built artifact names.
    pub fn artifact_prefix(self) -> &'static str {
        match self {
            Platform::Jvm => "",
            Platform::Js => "sjs1_",
            Platform::Native => "native0.5_",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Platform::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| format!("unknown platform `{s}` (expected jvm, js or native)"))
    }
}
