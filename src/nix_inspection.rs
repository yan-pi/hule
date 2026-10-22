use crate::{CleanupProfile, config::NixProfileKind};
use serde::Serialize;
use std::path::PathBuf;

/// Information not established by a configuration-only or fixture inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionStatus {
    Unchecked,
}

/// Read-only inspection output is never accepted as an action plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionAuthorization {
    NotAuthorized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NixStoreInspection {
    pub format: &'static str,
    pub schema_version: u8,
    pub profile: CleanupProfile,
    pub selected_objects: Vec<PathBuf>,
    pub requisites: Vec<PathBuf>,
    pub host_scope: InspectionStatus,
    pub version_scope: InspectionStatus,
    pub store_scope: InspectionStatus,
    pub reclaimed_bytes: InspectionStatus,
    pub authorization: InspectionAuthorization,
    pub limitation: &'static str,
}

impl NixStoreInspection {
    pub fn to_human(&self) -> String {
        let lines = [
            "hule Nix store inspection (no deletion; not authorization)".to_string(),
            format!("cleanup profile: {}", profile_name(self.profile)),
            "selected objects:".to_string(),
            paths_text(&self.selected_objects),
            "requisites closure:".to_string(),
            paths_text(&self.requisites),
            "host/version/store scope: unchecked".to_string(),
            "reclaimed bytes: unchecked".to_string(),
            format!("note: {}", self.limitation),
        ];
        lines.join("\n") + "\n"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationRole {
    Current,
    Historical,
    SelectedHistorical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NixGenerationEntry {
    pub id: u32,
    pub role: GenerationRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NixGenerationInspection {
    pub format: &'static str,
    pub schema_version: u8,
    pub cleanup_profile: CleanupProfile,
    pub profile_name: String,
    pub kind: NixProfileKind,
    pub current_generation: u32,
    pub generations: Vec<NixGenerationEntry>,
    pub selected_ids: Vec<u32>,
    pub host_scope: InspectionStatus,
    pub version_scope: InspectionStatus,
    pub store_scope: InspectionStatus,
    pub authorization: InspectionAuthorization,
    pub history_removes_profile_links_only: bool,
    pub store_gc_performed: bool,
    pub limitation: &'static str,
}

impl NixGenerationInspection {
    pub fn to_human(&self) -> String {
        let generations = if self.generations.is_empty() {
            "  (none)".to_string()
        } else {
            self.generations
                .iter()
                .map(|generation| {
                    format!(
                        "  {} [{}]",
                        generation.id,
                        generation_role_name(generation.role)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let selected = if self.selected_ids.is_empty() {
            "(none)".to_string()
        } else {
            self.selected_ids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let lines = [
            "hule Nix generation inspection (no deletion; not authorization)".to_string(),
            format!("cleanup profile: {}", profile_name(self.cleanup_profile)),
            format!(
                "configured Nix profile: {} [{}]",
                self.profile_name,
                profile_kind_name(self.kind)
            ),
            format!("current generation: {}", self.current_generation),
            "generations:".to_string(),
            generations,
            format!("selected historical IDs: {selected}"),
            "history removal affects profile links only; store GC performed: no".to_string(),
            "host/version/store scope: unchecked".to_string(),
            format!("note: {}", self.limitation),
        ];
        lines.join("\n") + "\n"
    }
}

fn paths_text(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        "  (none)".to_string()
    } else {
        paths
            .iter()
            .map(|path| format!("  {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn profile_name(profile: CleanupProfile) -> &'static str {
    match profile {
        CleanupProfile::Low => "low",
        CleanupProfile::Normal => "normal",
        CleanupProfile::Aggressive => "aggressive",
    }
}

fn profile_kind_name(kind: NixProfileKind) -> &'static str {
    match kind {
        NixProfileKind::User => "user",
        NixProfileKind::HomeManager => "home_manager",
    }
}

fn generation_role_name(role: GenerationRole) -> &'static str {
    match role {
        GenerationRole::Current => "current",
        GenerationRole::Historical => "historical",
        GenerationRole::SelectedHistorical => "selected historical",
    }
}
