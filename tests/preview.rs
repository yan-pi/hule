use std::path::PathBuf;

use hule::{
    CleanupProfile, ProjectMode,
    RetentionDecision::{Eligible, KeepPersistent, KeepProtected, SkipUnregistered},
    config::Config,
    preview::{
        DiscoveryOutcome, RustTargetSelection, preview_rust_target, preview_rust_target_plan,
    },
    state::ProjectRegistry,
};

fn fixture() -> (Config, ProjectRegistry) {
    let config = Config::from_toml(
        r#"
   schema_version = 2
   default_profile = "normal"
   additional_keep_paths = ["/work/protected/target/keep", "/work/persistent/docs"]
   [profiles.low]
   categories = ["rust_target"]
   [profiles.normal]
   categories = ["rust_target"]
   [profiles.aggressive]
   categories = ["rust_target"]
   "#,
    )
    .expect("valid policy fixture");
    let mut registry = ProjectRegistry::default();
    registry
        .register(
            "persistent".into(),
            PathBuf::from("/work/persistent"),
            ProjectMode::Persistent,
        )
        .unwrap();
    registry
        .register(
            "scratch".into(),
            PathBuf::from("/work/scratch project"),
            ProjectMode::Ephemeral,
        )
        .unwrap();
    registry
        .register(
            "protected".into(),
            PathBuf::from("/work/protected"),
            ProjectMode::Ephemeral,
        )
        .unwrap();
    (config, registry)
}

#[test]
fn projects_only_the_standard_target_path() {
    let (config, registry) = fixture();
    let preview = preview_rust_target(&config, &registry, "scratch", config.default_profile());
    assert_eq!(preview.project_name(), "scratch");
    assert_eq!(preview.target_path(), None);
    assert_eq!(preview.outcome(), &DiscoveryOutcome::Unsupported);
    assert_eq!(preview.decision(), Eligible);
}

#[test]
fn preview_combines_registry_retention_and_keep_paths() {
    let (config, registry) = fixture();
    for (profile, persistent_decision) in [
        (CleanupProfile::Low, KeepPersistent),
        (CleanupProfile::Normal, KeepPersistent),
        (CleanupProfile::Aggressive, Eligible),
    ] {
        for (name, expected) in [
            ("persistent", persistent_decision),
            ("scratch", Eligible),
            ("protected", KeepProtected),
        ] {
            let preview = preview_rust_target(&config, &registry, name, profile);
            assert_eq!(
                preview.decision(),
                expected,
                "profile: {profile:?} project: {name}"
            );
            assert_eq!(preview.outcome(), &DiscoveryOutcome::Unsupported);
            assert_eq!(preview.target_path(), None);
        }
    }
}

#[test]
fn rust_target_without_profile_opt_in_is_category_disabled() {
    let config = Config::from_toml(
        r#"schema_version = 2
default_profile = "normal"
[profiles.low]
categories = []
[profiles.normal]
categories = []
[profiles.aggressive]
categories = []"#,
    )
    .expect("valid opt-out config");
    let mut registry = ProjectRegistry::default();
    registry
        .register(
            "demo".into(),
            PathBuf::from("/work/demo"),
            ProjectMode::Ephemeral,
        )
        .unwrap();
    let preview = preview_rust_target(&config, &registry, "demo", CleanupProfile::Normal);
    assert_eq!(
        preview.decision(),
        hule::RetentionDecision::CategoryDisabled
    );
    assert_eq!(preview.target_path(), None);
    assert_eq!(preview.outcome(), &DiscoveryOutcome::Unsupported);
}

#[test]
fn preview_plan_keeps_named_skip_and_all_registry_order() {
    let (config, registry) = fixture();
    let named = preview_rust_target_plan(
        &config,
        &registry,
        RustTargetSelection::Named("missing".into()),
        CleanupProfile::Normal,
    );
    assert_eq!(named.entries().len(), 1);
    assert_eq!(named.entries()[0].project_name(), "missing");
    assert_eq!(named.entries()[0].decision(), SkipUnregistered);

    let all = preview_rust_target_plan(
        &config,
        &registry,
        RustTargetSelection::All,
        CleanupProfile::Normal,
    );
    let names: Vec<_> = all
        .entries()
        .iter()
        .map(|entry| entry.project_name())
        .collect();
    assert_eq!(names, vec!["persistent", "protected", "scratch"]);
    assert_eq!(all.schema_version(), 1);
    assert_eq!(all.category(), hule::config::Category::RustTarget);
    assert_eq!(
        all.authorization(),
        hule::preview::PreviewAuthorization::NotAuthorized
    );
}

#[test]
fn unknown_projects_are_skipped_without_constructing_paths() {
    let (config, registry) = fixture();
    for profile in [
        CleanupProfile::Low,
        CleanupProfile::Normal,
        CleanupProfile::Aggressive,
    ] {
        for name in ["missing", "../../outside", "/work/persistent"] {
            let preview = preview_rust_target(&config, &registry, name, profile);
            assert_eq!(preview.project_name(), name);
            assert_eq!(preview.decision(), SkipUnregistered);
            assert_eq!(preview.target_path(), None);
        }
    }
}
