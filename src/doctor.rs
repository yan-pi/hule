use crate::{
    CleanupProfile,
    config::{Category, Config},
};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Available,
    Unchecked,
    Unsupported,
    Unsafe,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorProfile {
    pub name: CleanupProfile,
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCapability {
    pub name: &'static str,
    pub status: DoctorStatus,
    pub configured: Option<bool>,
    pub summary: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub format: &'static str,
    pub schema_version: u8,
    pub configuration: DoctorStatus,
    pub default_profile: Option<CleanupProfile>,
    pub profiles: Option<Vec<DoctorProfile>>,
    pub nix_profile_names: Option<Vec<String>>,
    pub keep_path_count: Option<usize>,
    pub capabilities: Vec<DoctorCapability>,
}

impl DoctorReport {
    pub fn from_config(config: &Config) -> Self {
        let profiles = [
            CleanupProfile::Low,
            CleanupProfile::Normal,
            CleanupProfile::Aggressive,
        ]
        .into_iter()
        .map(|name| DoctorProfile {
            name,
            categories: config.categories(name).to_vec(),
        })
        .collect::<Vec<_>>();
        let enabled = |category| {
            profiles
                .iter()
                .any(|profile| profile.categories.contains(&category))
        };
        let generation_enabled =
            enabled(Category::NixUserGenerations) || enabled(Category::NixHomeManagerGenerations);
        let rust_target_enabled = enabled(Category::RustTarget);
        let nix_store_enabled = enabled(Category::NixStoreGc);
        let docker_enabled = enabled(Category::DockerPurge);
        Self {
            format: "hule-doctor-report",
            schema_version: 1,
            configuration: DoctorStatus::Available,
            default_profile: Some(config.default_profile()),
            profiles: Some(profiles),
            nix_profile_names: Some(config.nix_profiles().keys().cloned().collect()),
            keep_path_count: Some(config.additional_keep_paths().len()),
            capabilities: capabilities(
                Some(rust_target_enabled),
                Some(nix_store_enabled),
                Some(generation_enabled),
                Some(docker_enabled),
            ),
        }
    }

    pub fn invalid_configuration() -> Self {
        Self {
            format: "hule-doctor-report",
            schema_version: 1,
            configuration: DoctorStatus::Unsafe,
            default_profile: None,
            profiles: None,
            nix_profile_names: None,
            keep_path_count: None,
            capabilities: capabilities(None, None, None, None),
        }
    }

    pub fn capability(&self, name: &str) -> Option<&DoctorCapability> {
        self.capabilities
            .iter()
            .find(|capability| capability.name == name)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn to_human(&self) -> String {
        let config_summary = match self.configuration {
            DoctorStatus::Available => {
                "configuration schema is valid; provider and host state were not checked"
            }
            DoctorStatus::Unsafe => {
                "configuration could not be read or did not pass schema validation; no paths or contents are shown"
            }
            DoctorStatus::Unchecked | DoctorStatus::Unsupported => {
                "configuration status is unknown"
            }
        };
        let mut output = format!(
            "hule doctor (read-only; not authorization)\nconfiguration: {} — {config_summary}\n",
            status_name(self.configuration)
        );
        if let Some(default_profile) = self.default_profile {
            output.push_str(&format!(
                "default profile: {}\n",
                profile_name(default_profile)
            ));
        }
        if let Some(keep_path_count) = self.keep_path_count {
            output.push_str(&format!(
                "configured keep paths: {keep_path_count} (paths are not shown)\n"
            ));
        }
        if let Some(nix_profile_names) = &self.nix_profile_names {
            output.push_str(&format!(
                "configured Nix profile names: {}\n",
                if nix_profile_names.is_empty() {
                    "(none)".to_string()
                } else {
                    nix_profile_names.join(", ")
                }
            ));
        }
        if let Some(profiles) = &self.profiles {
            for profile in profiles {
                let categories = profile
                    .categories
                    .iter()
                    .map(|category| category_name(*category))
                    .collect::<Vec<_>>();
                output.push_str(&format!(
                    "profile {} categories: {}\n",
                    profile_name(profile.name),
                    if categories.is_empty() {
                        "(none)".to_string()
                    } else {
                        categories.join(", ")
                    }
                ));
            }
        }
        output.push_str("capabilities (configuration does not prove runtime availability):\n");
        for capability in &self.capabilities {
            output.push_str(&format!(
                "- {} [{}; configured: {}]: {}\n",
                capability.name,
                status_name(capability.status),
                configured_name(capability.configured),
                capability.summary
            ));
        }
        output
    }
}

fn capabilities(
    rust_target_configured: Option<bool>,
    nix_store_configured: Option<bool>,
    nix_generations_configured: Option<bool>,
    docker_configured: Option<bool>,
) -> Vec<DoctorCapability> {
    vec![
        DoctorCapability {
            name: "rust_target_preview",
            status: DoctorStatus::Available,
            configured: rust_target_configured,
            summary: "metadata and retention preview only; no Rust cleanup executor exists",
        },
        DoctorCapability {
            name: "rust_target_cleanup",
            status: DoctorStatus::Unsupported,
            configured: Some(false),
            summary: "cleanup is unsupported; Cargo clean and directory removal are not used",
        },
        DoctorCapability {
            name: "nix_store_selected_objects",
            status: DoctorStatus::Unchecked,
            configured: nix_store_configured,
            summary: "M1 uses an explicit saved selection; this report does not check Nix compatibility, and Nix host/store scope is not verified",
        },
        DoctorCapability {
            name: "nix_generations",
            status: DoctorStatus::Unchecked,
            configured: nix_generations_configured,
            summary: "M2 is supported only on Nix 2.34.6 and restricted current-user profile layouts; neither is checked here",
        },
        DoctorCapability {
            name: "docker_system_prune",
            status: DoctorStatus::Unchecked,
            configured: docker_configured,
            summary: "broad dynamic prune is mock-only with native acceptance pending; there are no resource-level keep exclusions",
        },
        DoctorCapability {
            name: "orbstack_reset",
            status: DoctorStatus::Unsupported,
            configured: Some(false),
            summary: "OrbStack machine deletion and reset are unsupported",
        },
        DoctorCapability {
            name: "macos_cache_cleanup",
            status: DoctorStatus::Unsupported,
            configured: Some(false),
            summary: "no macOS or application-cache cleanup provider exists",
        },
        DoctorCapability {
            name: "browser_cache_cleanup",
            status: DoctorStatus::Unsupported,
            configured: Some(false),
            summary: "no browser cleanup provider exists",
        },
        DoctorCapability {
            name: "shared_developer_cache_cleanup",
            status: DoctorStatus::Unsupported,
            configured: Some(false),
            summary: "no shared developer-cache cleanup provider exists",
        },
    ]
}

fn configured_name(configured: Option<bool>) -> &'static str {
    match configured {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn status_name(status: DoctorStatus) -> &'static str {
    match status {
        DoctorStatus::Available => "available",
        DoctorStatus::Unchecked => "unchecked",
        DoctorStatus::Unsupported => "unsupported",
        DoctorStatus::Unsafe => "unsafe",
    }
}

fn profile_name(profile: CleanupProfile) -> &'static str {
    match profile {
        CleanupProfile::Low => "low",
        CleanupProfile::Normal => "normal",
        CleanupProfile::Aggressive => "aggressive",
    }
}

fn category_name(category: Category) -> &'static str {
    match category {
        Category::RustTarget => "rust_target",
        Category::NixStoreGc => "nix_store_gc",
        Category::NixUserGenerations => "nix_user_generations",
        Category::NixHomeManagerGenerations => "nix_home_manager_generations",
        Category::DockerPurge => "docker_purge",
    }
}
