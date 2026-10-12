use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    path::{Component, Path, PathBuf},
};

use crate::{CleanupProfile, ProjectMode};
use serde::Deserialize;

const SUPPORTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub struct Config {
    default_profile: CleanupProfile,
    projects: BTreeMap<String, RegisteredProject>,
    additional_keep_paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct RegisteredProject {
    path: PathBuf,
    mode: ProjectMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    InvalidToml,
    UnsupportedSchemaVersion,
    InvalidProjectName,
    InvalidProjectPath,
    OverlappingProjectRoots,
    InvalidKeepPath,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    default_profile: CleanupProfile,
    #[serde(default)]
    projects: BTreeMap<String, RawProject>,
    #[serde(default)]
    additional_keep_paths: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    path: PathBuf,
    mode: ProjectMode,
}

impl Config {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(input).map_err(|_| ConfigError::InvalidToml)?;

        if raw.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion);
        }

        let mut projects: BTreeMap<String, RegisteredProject> = BTreeMap::new();

        for (name, project) in raw.projects {
            if !valid_project_name(&name) {
                return Err(ConfigError::InvalidProjectName);
            }

            if !valid_absolute_path(&project.path)
                || !project
                    .path
                    .components()
                    .any(|component| matches!(component, Component::Normal(_)))
            {
                return Err(ConfigError::InvalidProjectPath);
            }

            let path = normalize_path(&project.path);

            if projects.values().any(|existing| {
                path.starts_with(&existing.path) || existing.path.starts_with(&path)
            }) {
                return Err(ConfigError::OverlappingProjectRoots);
            }

            projects.insert(
                name,
                RegisteredProject {
                    path,
                    mode: project.mode,
                },
            );
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
            projects,
            additional_keep_paths,
        })
    }

    pub fn default_profile(&self) -> CleanupProfile {
        self.default_profile
    }

    pub fn project(&self, name: &str) -> Option<&RegisteredProject> {
        self.projects.get(name)
    }

    pub fn projects(&self) -> &BTreeMap<String, RegisteredProject> {
        &self.projects
    }

    pub fn additional_keep_paths(&self) -> &[PathBuf] {
        &self.additional_keep_paths
    }
    pub fn overlaps_keep_path(&self, candidate: &Path) -> bool {
        self.additional_keep_paths
            .iter()
            .any(|keep_path| candidate.starts_with(keep_path) || keep_path.starts_with(candidate))
    }
}

impl RegisteredProject {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn mode(&self) -> ProjectMode {
        self.mode
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidToml => "configuration has invalid syntax, fields or enum values",
            Self::UnsupportedSchemaVersion => "unsupported configuration schema version",
            Self::InvalidProjectName => "invalid project name",
            Self::InvalidProjectPath => "invalid project path",
            Self::OverlappingProjectRoots => "registered project roots overlap",
            Self::InvalidKeepPath => "invalid additional keep path",
        };

        formatter.write_str(message)
    }
}

impl Error for ConfigError {}

fn valid_project_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

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
