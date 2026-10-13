use crate::{
    CleanupProfile, RetentionDecision,
    config::{Category, Config},
    decide_retention,
    state::ProjectRegistry,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    Discovered(PathBuf),
    NoTarget,
    Unsupported,
}

pub struct RustTargetPreview {
    project_name: String,
    outcome: DiscoveryOutcome,
    decision: RetentionDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustTargetSelection {
    Named(String),
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PreviewAuthorization {
    #[serde(rename = "not_authorized")]
    NotAuthorized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DiscoveryOutcomeSnapshot {
    Discovered { path: PathBuf },
    NoTarget,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RustTargetPreviewEntry {
    project_name: String,
    discovery: DiscoveryOutcomeSnapshot,
    decision: RetentionDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RustTargetPreviewPlan {
    schema_version: u8,
    profile: CleanupProfile,
    category: Category,
    authorization: PreviewAuthorization,
    entries: Vec<RustTargetPreviewEntry>,
}

impl RustTargetPreview {
    pub fn project_name(&self) -> &str {
        &self.project_name
    }

    pub fn outcome(&self) -> &DiscoveryOutcome {
        &self.outcome
    }

    pub fn target_path(&self) -> Option<&Path> {
        match &self.outcome {
            DiscoveryOutcome::Discovered(path) => Some(path),
            DiscoveryOutcome::NoTarget | DiscoveryOutcome::Unsupported => None,
        }
    }

    pub fn decision(&self) -> RetentionDecision {
        self.decision
    }
}

impl RustTargetPreviewEntry {
    pub fn project_name(&self) -> &str {
        &self.project_name
    }

    pub fn discovery(&self) -> &DiscoveryOutcomeSnapshot {
        &self.discovery
    }

    pub fn decision(&self) -> RetentionDecision {
        self.decision
    }
}

impl RustTargetPreviewPlan {
    pub fn schema_version(&self) -> u8 {
        self.schema_version
    }

    pub fn profile(&self) -> CleanupProfile {
        self.profile
    }

    pub fn category(&self) -> Category {
        self.category
    }

    pub fn authorization(&self) -> PreviewAuthorization {
        self.authorization
    }

    pub fn entries(&self) -> &[RustTargetPreviewEntry] {
        &self.entries
    }
}

#[derive(Deserialize)]
struct CargoMetadata {
    target_directory: PathBuf,
}

pub fn preview_rust_target(
    config: &Config,
    registry: &ProjectRegistry,
    project_name: &str,
    profile: CleanupProfile,
) -> RustTargetPreview {
    preview_with_runner(config, registry, project_name, profile, run_cargo_metadata)
}

pub fn preview_rust_target_plan(
    config: &Config,
    registry: &ProjectRegistry,
    selection: RustTargetSelection,
    profile: CleanupProfile,
) -> RustTargetPreviewPlan {
    preview_plan_with_runner(config, registry, selection, profile, run_cargo_metadata)
}

fn preview_plan_with_runner(
    config: &Config,
    registry: &ProjectRegistry,
    selection: RustTargetSelection,
    profile: CleanupProfile,
    runner: impl Fn(&Path) -> Result<Vec<u8>, ()>,
) -> RustTargetPreviewPlan {
    let previews = match selection {
        RustTargetSelection::Named(name) => {
            vec![preview_with_runner(
                config, registry, &name, profile, &runner,
            )]
        }
        RustTargetSelection::All => registry
            .list()
            .keys()
            .map(|name| preview_with_runner(config, registry, name, profile, &runner))
            .collect(),
    };
    assemble_plan(profile, previews)
}

fn assemble_plan(
    profile: CleanupProfile,
    previews: Vec<RustTargetPreview>,
) -> RustTargetPreviewPlan {
    RustTargetPreviewPlan {
        schema_version: 1,
        profile,
        category: Category::RustTarget,
        authorization: PreviewAuthorization::NotAuthorized,
        entries: previews.into_iter().map(entry_from_preview).collect(),
    }
}

fn entry_from_preview(preview: RustTargetPreview) -> RustTargetPreviewEntry {
    let discovery = match preview.outcome {
        DiscoveryOutcome::Discovered(path) => DiscoveryOutcomeSnapshot::Discovered { path },
        DiscoveryOutcome::NoTarget => DiscoveryOutcomeSnapshot::NoTarget,
        DiscoveryOutcome::Unsupported => DiscoveryOutcomeSnapshot::Unsupported,
    };
    RustTargetPreviewEntry {
        project_name: preview.project_name,
        discovery,
        decision: preview.decision,
    }
}

fn preview_with_runner(
    config: &Config,
    registry: &ProjectRegistry,
    project_name: &str,
    profile: CleanupProfile,
    runner: impl Fn(&Path) -> Result<Vec<u8>, ()>,
) -> RustTargetPreview {
    let Some(project) = registry.get(project_name) else {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            RetentionDecision::SkipUnregistered,
        );
    };

    let root_path = project.path();
    let anticipated_target = root_path.join("target");
    if config.overlaps_resolved_keep_path(&anticipated_target) {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            RetentionDecision::KeepProtected,
        );
    }
    if !config.categories(profile).contains(&Category::RustTarget) {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            RetentionDecision::CategoryDisabled,
        );
    }

    let Some(root) = validated_root(root_path) else {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            decision_for(project.mode(), profile, false),
        );
    };
    let Some(manifest) = valid_manifest(&root) else {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            decision_for(project.mode(), profile, false),
        );
    };
    let Ok(output) = runner(&manifest) else {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            decision_for(project.mode(), profile, false),
        );
    };
    let Ok(metadata) = serde_json::from_slice::<CargoMetadata>(&output) else {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            decision_for(project.mode(), profile, false),
        );
    };

    let target = metadata.target_directory;
    if !target.is_absolute()
        || target
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            decision_for(project.mode(), profile, false),
        );
    }
    let Some(target_state) = validated_target(&root, &target) else {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            decision_for(project.mode(), profile, false),
        );
    };
    let canonical_target = match target_state {
        TargetValidation::Missing => {
            return preview(
                project_name,
                DiscoveryOutcome::NoTarget,
                decision_for(project.mode(), profile, false),
            );
        }
        TargetValidation::Present(path) => path,
    };
    if config.overlaps_resolved_keep_path(&canonical_target) {
        return preview(
            project_name,
            DiscoveryOutcome::Unsupported,
            RetentionDecision::KeepProtected,
        );
    }
    preview(
        project_name,
        DiscoveryOutcome::Discovered(canonical_target),
        decision_for(project.mode(), profile, false),
    )
}

fn preview(
    project_name: &str,
    outcome: DiscoveryOutcome,
    decision: RetentionDecision,
) -> RustTargetPreview {
    RustTargetPreview {
        project_name: project_name.to_owned(),
        outcome,
        decision,
    }
}

fn decision_for(
    mode: crate::ProjectMode,
    profile: CleanupProfile,
    protected: bool,
) -> RetentionDecision {
    decide_retention(profile, Some(mode), protected)
}

fn validated_root(path: &Path) -> Option<PathBuf> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return None;
    }
    let canonical = fs::canonicalize(path).ok()?;
    (canonical == path).then_some(canonical)
}

fn valid_manifest(root: &Path) -> Option<PathBuf> {
    let path = root.join("Cargo.toml");
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    Some(path)
}

enum TargetValidation {
    Present(PathBuf),
    Missing,
}

fn validated_target(root: &Path, target: &Path) -> Option<TargetValidation> {
    let relative = target.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }

    let mut current = root.to_path_buf();
    let mut missing = false;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !missing => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return None;
                }
            }
            Ok(_) => return None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(_) => return None,
        }
    }

    if missing {
        return Some(TargetValidation::Missing);
    }

    let canonical = fs::canonicalize(target).ok()?;
    if canonical == root || !canonical.starts_with(root) {
        return None;
    }
    Some(TargetValidation::Present(canonical))
}

fn run_cargo_metadata(manifest: &Path) -> Result<Vec<u8>, ()> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--offline",
            "--locked",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProjectMode, config::Config, state::ProjectRegistry};
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Fixture(tempfile::TempDir);
    impl Fixture {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
        fn project(&self, target: &str) -> (Config, ProjectRegistry) {
            let root = self.path().join("project with spaces");
            fs::create_dir_all(root.join(target)).unwrap();
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
            )
            .unwrap();
            let root = fs::canonicalize(root).unwrap();
            let mut registry = ProjectRegistry::default();
            registry
                .register("fixture".into(), root.clone(), ProjectMode::Ephemeral)
                .unwrap();
            (config(true, vec![]), registry)
        }
    }
    fn config(enabled: bool, keep: Vec<&str>) -> Config {
        let categories = if enabled { "[\"rust_target\"]" } else { "[]" };
        Config::from_toml(&format!("schema_version=2\ndefault_profile='normal'\nadditional_keep_paths={:?}\n[profiles.low]\ncategories={}\n[profiles.normal]\ncategories={}\n[profiles.aggressive]\ncategories={}\n", keep, categories, categories, categories)).unwrap()
    }
    fn metadata(path: &Path) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"target_directory": path})).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn keep_path_symlink_alias_protects_the_validated_target() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let (_base_config, registry) = fixture.project("build/nested");
        let target = registry.get("fixture").unwrap().path().join("build/nested");
        let alias = fixture.path().join("target-alias");
        symlink(&target, &alias).unwrap();
        let config = config(true, vec![alias.to_str().unwrap()]);
        let result = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&target)),
        );
        assert_eq!(result.decision(), RetentionDecision::KeepProtected);
        assert_eq!(result.target_path(), None);
    }

    #[cfg(unix)]
    #[test]
    fn keep_path_with_intermediate_symlink_protects_target() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let (_base_config, registry) = fixture.project("build/nested");
        let root = registry.get("fixture").unwrap().path();
        let target = root.join("build/nested");
        let alias = fixture.path().join("build-alias");
        symlink(root.join("build"), &alias).unwrap();
        let keep = alias.join("nested/keep");
        let config = config(true, vec![keep.to_str().unwrap()]);
        let result = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&target)),
        );
        assert_eq!(result.decision(), RetentionDecision::KeepProtected);
        assert_eq!(result.target_path(), None);
    }

    #[test]
    fn discovers_nested_target_from_metadata_and_handles_spaces() {
        let fixture = Fixture::new();
        let (config, registry) = fixture.project("build/nested");
        let target = registry.get("fixture").unwrap().path().join("build/nested");
        let result = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&target)),
        );
        assert_eq!(result.target_path(), Some(target.as_path()));
    }

    #[test]
    fn reports_verified_absent_nested_target_without_authorizing_cleanup() {
        let fixture = Fixture::new();
        let (config, registry) = fixture.project("build");
        let root = registry.get("fixture").unwrap().path();
        let target = root.join("build/nested/target");
        assert!(!target.exists());

        let result = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&target)),
        );
        assert_eq!(result.outcome(), &DiscoveryOutcome::NoTarget);
        assert_eq!(result.target_path(), None);
        assert_eq!(result.decision(), RetentionDecision::Eligible);

        let report = crate::report::render_rust_target_preview(&result);
        assert!(report.contains("target: -\n"));
        assert!(report.contains("discovery: no-target/no-artifacts\n"));
        assert!(!report.contains(target.to_str().unwrap()));

        let plan = preview_plan_with_runner(
            &config,
            &registry,
            RustTargetSelection::Named("fixture".into()),
            CleanupProfile::Normal,
            |_| Ok(metadata(&target)),
        );
        let json = serde_json::to_value(plan).unwrap();
        assert_eq!(json["authorization"], "not_authorized");
        assert_eq!(
            json["entries"][0]["discovery"],
            serde_json::json!({"outcome": "no_target"})
        );
        assert_eq!(json["entries"][0]["decision"], "eligible");
    }

    #[test]
    fn unregistered_disabled_and_protected_do_not_run_provider() {
        let fixture = Fixture::new();
        let (enabled, registry) = fixture.project("target");
        let calls = AtomicU64::new(0);
        let result = preview_with_runner(
            &enabled,
            &registry,
            "missing",
            CleanupProfile::Normal,
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(vec![])
            },
        );
        assert_eq!(result.decision(), RetentionDecision::SkipUnregistered);
        let disabled = config(false, vec![]);
        preview_with_runner(
            &disabled,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(vec![])
            },
        );
        let protected = config(
            true,
            vec![
                registry
                    .get("fixture")
                    .unwrap()
                    .path()
                    .join("target")
                    .to_str()
                    .unwrap(),
            ],
        );
        preview_with_runner(
            &protected,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(vec![])
            },
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rejects_external_symlink_and_bad_metadata_without_leaking_error_details() {
        let fixture = Fixture::new();
        let (config, registry) = fixture.project("target");
        let root = registry.get("fixture").unwrap().path().to_path_buf();
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let external = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&outside)),
        );
        assert_eq!(external.outcome(), &DiscoveryOutcome::Unsupported);
        let traversal = root.join("../outside");
        let traversing = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&traversal)),
        );
        assert_eq!(traversing.outcome(), &DiscoveryOutcome::Unsupported);
        let absent = root.join("not-created");
        let missing_target = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&absent)),
        );
        assert_eq!(missing_target.outcome(), &DiscoveryOutcome::NoTarget);
        let root_target = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&root)),
        );
        assert_eq!(root_target.outcome(), &DiscoveryOutcome::Unsupported);
        let blocked = root.join("blocked");
        fs::write(&blocked, "fixture obstruction").unwrap();
        let obstructed_target = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(metadata(&blocked.join("target"))),
        );
        assert_eq!(obstructed_target.outcome(), &DiscoveryOutcome::Unsupported);
        let bad = preview_with_runner(
            &config,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            |_| Ok(b"not json secret".to_vec()),
        );
        assert_eq!(bad.outcome(), &DiscoveryOutcome::Unsupported);
        #[cfg(unix)]
        {
            let link = root.join("linked");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let symlink = preview_with_runner(
                &config,
                &registry,
                "fixture",
                CleanupProfile::Normal,
                |_| Ok(metadata(&link)),
            );
            assert_eq!(symlink.outcome(), &DiscoveryOutcome::Unsupported);
            let linked_ancestor = root.join("linked-ancestor");
            std::os::unix::fs::symlink(&outside, &linked_ancestor).unwrap();
            let missing_below_symlink = preview_with_runner(
                &config,
                &registry,
                "fixture",
                CleanupProfile::Normal,
                |_| Ok(metadata(&linked_ancestor.join("not-created"))),
            );
            assert_eq!(
                missing_below_symlink.outcome(),
                &DiscoveryOutcome::Unsupported
            );
            let broken_link = root.join("broken-link");
            std::os::unix::fs::symlink(outside.join("not-created"), &broken_link).unwrap();
            let broken_symlink = preview_with_runner(
                &config,
                &registry,
                "fixture",
                CleanupProfile::Normal,
                |_| Ok(metadata(&broken_link)),
            );
            assert_eq!(broken_symlink.outcome(), &DiscoveryOutcome::Unsupported);
        }
    }

    #[test]
    fn cargo_metadata_json_parser_rejects_missing_or_ambiguous_target() {
        assert!(serde_json::from_slice::<CargoMetadata>(br#"{"packages":[]}"#).is_err());
        assert!(
            serde_json::from_slice::<CargoMetadata>(
                br#"{"target_directory":"/tmp/a","target_directory":"/tmp/b"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn plan_serialization_is_not_authorization_and_omits_paths_when_unsupported() {
        let plan = assemble_plan(
            CleanupProfile::Aggressive,
            vec![preview(
                "fixture",
                DiscoveryOutcome::Unsupported,
                RetentionDecision::Eligible,
            )],
        );
        let value: serde_json::Value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["authorization"], "not_authorized");
        assert_eq!(value["entries"][0]["discovery"]["outcome"], "unsupported");
        assert!(value["entries"][0]["discovery"].get("path").is_none());
        assert_eq!(value["entries"][0]["decision"], "eligible");
    }

    #[cfg(unix)]
    #[test]
    fn plan_serialization_rejects_non_utf8_discovered_paths() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let path = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
        let plan = assemble_plan(
            CleanupProfile::Normal,
            vec![preview(
                "fixture",
                DiscoveryOutcome::Discovered(path),
                RetentionDecision::Eligible,
            )],
        );
        assert!(serde_json::to_string_pretty(&plan).is_err());
    }
}
