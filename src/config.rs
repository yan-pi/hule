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
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    RustTarget,
    NixStoreGc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    InvalidToml,
    UnsupportedSchemaVersion,
    InvalidKeepPath,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    default_profile: CleanupProfile,
    profiles: BTreeMap<CleanupProfile, RawProfile>,
    #[serde(default)]
    additional_keep_paths: Vec<PathBuf>,
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

        Ok(Self {
            default_profile: raw.default_profile,
            profiles,
            additional_keep_paths,
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
