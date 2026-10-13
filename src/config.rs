use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Component, Path, PathBuf},
};

use crate::CleanupProfile;
use serde::Deserialize;

const SUPPORTED_SCHEMA_VERSION: u32 = 2;

#[derive(Debug)]
pub struct Config {
    default_profile: CleanupProfile,
    profiles: BTreeMap<CleanupProfile, Vec<Category>>,
    additional_keep_paths: Vec<PathBuf>,
    nix_profiles: BTreeMap<String, NixProfileConfig>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    RustTarget,
    NixStoreGc,
    NixUserGenerations,
    NixHomeManagerGenerations,
    DockerPurge,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NixProfileConfig {
    User {
        path: PathBuf,
    },
    HomeManager {
        path: PathBuf,
        current_home_roots: Vec<PathBuf>,
    },
}

impl NixProfileConfig {
    pub fn path(&self) -> &Path {
        match self {
            Self::User { path } | Self::HomeManager { path, .. } => path,
        }
    }

    pub fn kind(&self) -> NixProfileKind {
        match self {
            Self::User { .. } => NixProfileKind::User,
            Self::HomeManager { .. } => NixProfileKind::HomeManager,
        }
    }

    pub fn current_home_roots(&self) -> &[PathBuf] {
        match self {
            Self::User { .. } => &[],
            Self::HomeManager {
                current_home_roots, ..
            } => current_home_roots,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NixProfileKind {
    User,
    HomeManager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    InvalidToml,
    UnsupportedSchemaVersion,
    InvalidKeepPath,
    InvalidNixProfileName,
    InvalidNixProfilePath,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    default_profile: CleanupProfile,
    profiles: BTreeMap<CleanupProfile, RawProfile>,
    #[serde(default)]
    additional_keep_paths: Vec<PathBuf>,
    #[serde(default)]
    nix_profiles: BTreeMap<String, NixProfileConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    categories: Vec<Category>,
}

impl Config {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(input).map_err(|_| ConfigError::InvalidToml)?;

        if raw.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion);
        }

        if raw.profiles.values().any(|settings| {
            let unique_categories = settings.categories.iter().collect::<BTreeSet<_>>();
            unique_categories.len() != settings.categories.len()
        }) || raw.profiles.iter().any(|(profile, settings)| {
            *profile != CleanupProfile::Aggressive
                && settings.categories.contains(&Category::DockerPurge)
        }) {
            return Err(ConfigError::InvalidToml);
        }

        let profiles = raw
            .profiles
            .into_iter()
            .map(|(profile, settings)| (profile, settings.categories))
            .collect::<BTreeMap<_, _>>();
        if profiles.len() != 3 {
            return Err(ConfigError::InvalidToml);
        }

        let additional_keep_paths = raw
            .additional_keep_paths
            .into_iter()
            .map(|path| {
                if !valid_absolute_path(&path) {
                    return Err(ConfigError::InvalidKeepPath);
                }
                Ok(normalize_path(&path))
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;
        for (name, profile) in &raw.nix_profiles {
            if !valid_nix_profile_name(name) {
                return Err(ConfigError::InvalidNixProfileName);
            }
            if !valid_nix_profile_path(profile.path(), profile.kind())
                || (profile.kind() == NixProfileKind::HomeManager
                    && (profile.current_home_roots().is_empty()
                        || profile
                            .current_home_roots()
                            .iter()
                            .any(|root| !valid_absolute_path(root))
                        || profile
                            .current_home_roots()
                            .iter()
                            .collect::<BTreeSet<_>>()
                            .len()
                            != profile.current_home_roots().len()))
            {
                return Err(ConfigError::InvalidNixProfilePath);
            }
        }

        Ok(Self {
            default_profile: raw.default_profile,
            profiles,
            additional_keep_paths,
            nix_profiles: raw.nix_profiles,
        })
    }

    pub fn default_profile(&self) -> CleanupProfile {
        self.default_profile
    }

    pub fn categories(&self, profile: CleanupProfile) -> &[Category] {
        self.profiles.get(&profile).map_or(&[], Vec::as_slice)
    }

    pub fn additional_keep_paths(&self) -> &[PathBuf] {
        &self.additional_keep_paths
    }

    pub fn nix_profiles(&self) -> &BTreeMap<String, NixProfileConfig> {
        &self.nix_profiles
    }
    pub fn overlaps_keep_path(&self, candidate: &Path) -> bool {
        self.additional_keep_paths
            .iter()
            .any(|keep_path| candidate.starts_with(keep_path) || keep_path.starts_with(candidate))
    }

    /// Conservatively compare a validated candidate with keep paths, following aliases only
    /// while resolving configured keep paths. An unresolvable keep path protects the candidate.
    pub fn overlaps_resolved_keep_path(&self, candidate: &Path) -> bool {
        self.additional_keep_paths.iter().any(|keep_path| {
            if candidate.starts_with(keep_path) || keep_path.starts_with(candidate) {
                return true;
            }
            let Some(resolved_keep) = resolve_keep_path(keep_path) else {
                return true;
            };
            candidate.starts_with(&resolved_keep) || resolved_keep.starts_with(candidate)
        })
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidToml => "configuration has invalid syntax, fields or enum values",
            Self::UnsupportedSchemaVersion => "unsupported configuration schema version",
            Self::InvalidKeepPath => "invalid additional keep path",
            Self::InvalidNixProfileName => "invalid Nix profile name",
            Self::InvalidNixProfilePath => "invalid Nix profile path",
        };

        formatter.write_str(message)
    }
}

impl Error for ConfigError {}

fn valid_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        && !path
            .to_string_lossy()
            .chars()
            .any(|character| character.is_control() || matches!(character, '*' | '?' | '[' | ']'))
}

fn valid_nix_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn valid_nix_profile_path(path: &Path, kind: NixProfileKind) -> bool {
    if !valid_absolute_path(path) {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let expected_leaf = match kind {
        NixProfileKind::User => "profile",
        NixProfileKind::HomeManager => "home-manager",
    };
    path.file_name().is_some_and(|leaf| leaf == expected_leaf)
        && parent.file_name().is_some_and(|leaf| leaf == "profiles")
        && parent
            .parent()
            .is_some_and(|parent| parent.file_name().is_some_and(|leaf| leaf == "nix"))
}

fn normalize_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn resolve_keep_path(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if !missing.is_empty() && !metadata.is_dir() {
                    return None;
                }
                let mut resolved = fs::canonicalize(ancestor).ok()?;
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name()?.to_os_string());
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
}
