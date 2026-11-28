//! Raw configuration files exactly as written: `scpm-workspace.yaml` and `scpm.yaml`.
//!
//! These types only describe syntax and are used to validate files with precise error
//! locations. Presets, catalogs and variants are applied in [`crate::workspace`], which merges
//! the raw YAML values and re-deserializes the result.

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::platform::Platform;
use crate::version::Ver;

/// `boms` is a map like `dependencies`; the earlier `[org:name:version]` list form gets a
/// pointer to the new form rather than serde's generic type error.
fn boms<'de, D: Deserializer<'de>>(d: D) -> Result<IndexMap<String, Ver>, D::Error> {
    struct Boms;
    impl<'de> serde::de::Visitor<'de> for Boms {
        type Value = IndexMap<String, Ver>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of BOM coordinates to versions")
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(IndexMap::new())
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            IndexMap::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _: A) -> Result<Self::Value, A::Error> {
            Err(A::Error::custom(
                "`boms` is a map of coordinate to version, like `dependencies` \
                 (e.g. `io.netty:netty-bom: 4.1.115.Final` or `com.fasterxml.jackson:jackson-bom: catalog`)",
            ))
        }
    }
    d.deserialize_any(Boms)
}

pub const WORKSPACE_FILE: &str = "scpm-workspace.yaml";
pub const MODULE_FILE: &str = "scpm.yaml";

/// `scpm-workspace.yaml`: workspace-wide settings.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(title = "scpm workspace")]
pub struct WorkspaceFile {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Pinned scpm version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scpm: Option<Ver>,
    /// Globs of module directories (each containing `scpm.yaml`). `!` excludes.
    #[serde(default)]
    pub modules: Vec<String>,
    /// Default organization (groupId) for modules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default)]
    pub toolchain: Toolchain,
    /// Maven repositories, in resolution order. Defaults to `[central]`.
    #[serde(default)]
    pub repositories: Vec<Repository>,
    /// The default catalog, referenced as `catalog:`.
    #[serde(default)]
    pub catalog: IndexMap<String, Ver>,
    /// Named catalogs, referenced as `catalog:<name>`.
    #[serde(default)]
    pub catalogs: IndexMap<String, IndexMap<String, Ver>>,
    /// BOMs applied to every module: `org:name: <version>`, where the version may be exact, a
    /// range, `catalog` or `catalog:<name>` (looked up by the BOM's coordinate).
    #[serde(default, deserialize_with = "boms")]
    pub boms: IndexMap<String, Ver>,
    /// Forced versions applied workspace-wide.
    #[serde(default)]
    pub overrides: IndexMap<String, Ver>,
    /// Named fragments that modules merge in with `extends:`.
    #[serde(default)]
    pub presets: IndexMap<String, ModuleFile>,
    /// Task defaults inherited by every module.
    #[serde(default)]
    pub tasks: IndexMap<String, TaskDef>,
    #[serde(default)]
    pub pipelines: IndexMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_base: Option<String>,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versioning: Option<Versioning>,
    #[serde(default)]
    pub lock: LockSettings,
    /// IDE project files that a non-frozen `scpm install` keeps up to date (design §10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ide: Option<WorkspaceIde>,
}

/// `ide` in the workspace file: which IDE files a non-frozen `scpm install` regenerates.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceIde {
    /// Regenerate `.metals/mbt.json` (Metals v2) after every non-frozen install. Without it,
    /// the file is regenerated only when it already exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metals: Option<bool>,
    /// Regenerate the IntelliJ project files (`scpm idea`) after every non-frozen install.
    /// Without it, they are regenerated only when scpm generated them before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idea: Option<bool>,
}

/// `ide` in a module file or a preset.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModuleIde {
    /// The variant `scpm idea` turns into this module's IntelliJ module, as a variant selector
    /// (`2.13`, `jvm-3`, …). IntelliJ modules can't share a content root, so a cross-built
    /// module gets one IntelliJ module. Default: the first variant (the first Scala version
    /// listed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockSettings {
    /// The fixed OS/arch/JDK used for POM profile activation when locking, so a lock made on
    /// macOS equals one made on Linux. Default `{os: linux, arch: x86_64, jdk: <project jdk>}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_context: Option<ResolutionContext>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolutionContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jdk: Option<Ver>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Toolchain {
    /// JDK, e.g. `temurin:25`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jdk: Option<String>,
    /// Default Scala version (or `catalog:`/`catalog:<name>`, looked up under the key `scala`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scala: Option<Ver>,
}

/// A repository: a well-known name (`central`) or a full definition.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Repository {
    Named(String),
    Full(RepositoryDef),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryDef {
    pub id: String,
    pub url: String,
    /// Omit to use coursier's credentials (`COURSIER_CREDENTIALS`, `credentials.properties`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Credentials>,
    /// Send credentials to this `http://` repository although plain HTTP exposes them on the
    /// network. Without it, credentials only ever go over HTTPS. They are then sent to exactly
    /// this URL's scheme, host and port.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_insecure_credentials: bool,
    /// Group-id prefixes (`com.acme` covers `com.acme` and `com.acme.*`) served only by this
    /// repository and any other that lists the same longest prefix: those artifacts are
    /// restored from nowhere else, and nothing else is restored from here. Without it, the
    /// repository serves every group no repository claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub groups: Option<Vec<String>>,
}

/// `env:VAR` (bearer token) or `{user: env:U, password: env:P}` (basic auth).
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Credentials {
    Bearer(String),
    Basic { user: String, password: String },
}

impl<'de> Deserialize<'de> for Credentials {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Basic {
            user: String,
            password: String,
        }
        let value = serde_yaml_ng::Value::deserialize(d)?;
        match value {
            serde_yaml_ng::Value::String(s) => Ok(Credentials::Bearer(s)),
            other => {
                let b: Basic = serde_yaml_ng::from_value(other).map_err(D::Error::custom)?;
                Ok(Credentials::Basic { user: b.user, password: b.password })
            }
        }
    }
}

impl<'de> Deserialize<'de> for Repository {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(d)?;
        match value {
            serde_yaml_ng::Value::String(s) => Ok(Repository::Named(s)),
            other => serde_yaml_ng::from_value(other).map(Repository::Full).map_err(D::Error::custom),
        }
    }
}

/// A task definition, pnpm style.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskDef {
    /// Shell command, or `builtin:<name> [args]`. Omitted for built-in tasks like `compile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// `^task` = that task in each upstream module; `task` = same module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<String>>,
    /// Declared outputs make the task cacheable; `[]` means "no files".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
    /// Environment variables that are part of the cache key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency_group: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Policy {
    /// Refuse to lock versions younger than this, e.g. `1d`, `12h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_release_age: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub minimum_release_age_exclude: Vec<ReleaseAgeExclude>,
    /// `advisory-fixed` (default) or `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_bypass: Option<String>,
    /// `error` (default) or `warn`: evictions that violate the evicted library's versionScheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_conflicts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ReleaseAgeExclude {
    Pattern(String),
    Entry {
        coord: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires: Option<String>,
    },
}

impl<'de> Deserialize<'de> for ReleaseAgeExclude {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Entry {
            coord: String,
            #[serde(default)]
            reason: Option<String>,
            #[serde(default)]
            expires: Option<String>,
        }
        let value = serde_yaml_ng::Value::deserialize(d)?;
        match value {
            serde_yaml_ng::Value::String(s) => Ok(ReleaseAgeExclude::Pattern(s)),
            other => {
                let e: Entry = serde_yaml_ng::from_value(other).map_err(D::Error::custom)?;
                Ok(ReleaseAgeExclude::Entry { coord: e.coord, reason: e.reason, expires: e.expires })
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Versioning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixed: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub lanes: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bump: Option<String>,
}

/// `scpm.yaml`: one module. Also the shape of presets and of `variants` overrides.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(title = "scpm module")]
pub struct ModuleFile {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Module name; defaults to the directory name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Ver>,
    /// Presets to merge in, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    /// Defaults to `[jvm]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<Platform>,
    /// Scala version(s). A list cross-builds. `none` marks a Java-only module.
    /// Defaults to `toolchain.scala`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scala: Option<OneOrMany<Ver>>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub dependencies: IndexMap<String, DepSpec>,
    /// Compile-only (Maven `provided`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub provided_dependencies: IndexMap<String, DepSpec>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub runtime_dependencies: IndexMap<String, DepSpec>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub test_dependencies: IndexMap<String, DepSpec>,
    /// Compiler plugins (`org:::name` for full-cross Scala 2 plugins); added as `-Xplugin`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub compiler_plugins: IndexMap<String, DepSpec>,
    /// BOMs: `org:name: <version>` (exact, a range, `catalog` or `catalog:<name>`).
    #[serde(default, deserialize_with = "boms", skip_serializing_if = "IndexMap::is_empty")]
    pub boms: IndexMap<String, Ver>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scalac_options: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub javac_options: Vec<String>,
    /// Java release target, e.g. `17`. Defaults to the project JDK's major version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<Ver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_class: Option<String>,
    /// JVM options for forked `run` and `test` JVMs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jvm_options: Vec<String>,
    /// Environment variables for forked `run` and `test` JVMs.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    /// Source directories relative to the module. Default `src/main/scala`, `src/main/java`
    /// plus version- and platform-specific `src/main/scala-<x>` directories that exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_sources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_resources: Option<Vec<String>>,
    /// Directories under `${outDir}` (`.scpm/out/<variant>/`) written by tasks and compiled
    /// with the main sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_scheme: Option<VersionScheme>,
    /// Overrides for variants matching a selector such as `2.13`, `js` or `js-2.13`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variants: IndexMap<String, ModuleFile>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub tasks: IndexMap<String, TaskDef>,
    /// IDE settings (not allowed in `variants` overrides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ide: Option<ModuleIde>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VersionScheme {
    EarlySemver,
    SemverSpec,
    Pvp,
    Strict,
    Always,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::One(t) => vec![t],
            OneOrMany::Many(v) => v,
        }
    }
}

impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for OneOrMany<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(d)?;
        match value {
            serde_yaml_ng::Value::Sequence(_) => serde_yaml_ng::from_value(value).map(OneOrMany::Many),
            other => serde_yaml_ng::from_value(other).map(OneOrMany::One),
        }
        .map_err(D::Error::custom)
    }
}

/// A dependency's value: a version spec string, or a detailed object.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum DepSpec {
    Version(Ver),
    Detailed(DepDetails),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DepDetails {
    pub version: Ver,
    /// `org:name` patterns; `*` allowed in either part.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    /// Artifact type, default `jar`.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Default `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transitive: Option<bool>,
}

impl DepSpec {
    pub fn version(&self) -> &Ver {
        match self {
            DepSpec::Version(v) => v,
            DepSpec::Detailed(d) => &d.version,
        }
    }
}

impl<'de> Deserialize<'de> for DepSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(d)?;
        match value {
            serde_yaml_ng::Value::Mapping(_) => serde_yaml_ng::from_value(value).map(DepSpec::Detailed),
            other => serde_yaml_ng::from_value(other).map(DepSpec::Version),
        }
        .map_err(D::Error::custom)
    }
}
