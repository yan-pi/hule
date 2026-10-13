pub mod action_plan;
pub mod config;
pub mod nix_generations;
pub mod nix_store;
pub mod preset;
pub mod preview;
pub mod report;
pub mod state;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CleanupProfile {
    Low,
    Normal,
    Aggressive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectMode {
    Ephemeral,
    Persistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionDecision {
    KeepProtected,
    KeepPersistent,
    SkipUnregistered,
    CategoryDisabled,
    Eligible,
}

/// Evaluates project-retention rules only.
///
/// Eligibility does not authorize filesystem changes.
pub fn decide_retention(
    profile: CleanupProfile,
    mode: Option<ProjectMode>,
    protected: bool,
) -> RetentionDecision {
    if protected {
        return RetentionDecision::KeepProtected;
    }

    match (profile, mode) {
        (_, None) => RetentionDecision::SkipUnregistered,

        (CleanupProfile::Low | CleanupProfile::Normal, Some(ProjectMode::Persistent)) => {
            RetentionDecision::KeepPersistent
        }

        (CleanupProfile::Aggressive, Some(ProjectMode::Persistent))
        | (_, Some(ProjectMode::Ephemeral)) => RetentionDecision::Eligible,
    }
}
