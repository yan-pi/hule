use hule::{
    CleanupProfile, ProjectMode, config::Config, preview::preview_rust_target,
    report::render_rust_target_preview, state::ProjectRegistry,
};
use std::path::PathBuf;

fn fixture() -> (Config, ProjectRegistry) {
    let config = Config::from_toml(
        r#"
schema_version = 2
default_profile = "normal"
additional_keep_paths = ["/work/protected/target/keep"]
[profiles.low]
categories = ["rust_target"]
[profiles.normal]
categories = ["rust_target"]
[profiles.aggressive]
categories = ["rust_target"]
"#,
    )
    .expect("valid fixture");
    let mut registry = ProjectRegistry::default();
    registry
        .register(
            "scratch".into(),
            PathBuf::from("/work/scratch project"),
            ProjectMode::Ephemeral,
        )
        .unwrap();
    registry
        .register(
            "persistent".into(),
            PathBuf::from("/work/persistent"),
            ProjectMode::Persistent,
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
fn renders_eligible_preview_with_explicit_safety_notice() {
    let (config, registry) = fixture();
    let preview = preview_rust_target(&config, &registry, "scratch", CleanupProfile::Normal);
    assert_eq!(
        render_rust_target_preview(&preview),
        concat!(
            "project: \"scratch\"\n",
            "target: -\n",
            "discovery: unsupported/skipped\n",
            "decision: retention-eligible\n",
            "reason: passed retention rules only; not removal authorization\n",
            "safety: Cargo metadata --no-deps --offline --locked may be invoked only when the project is registered, rust_target is enabled, and the anticipated target is not already protected; no build/clean requested, no target contents enumerated, no cleanup executed; Cargo docs do not guarantee zero side effects or code execution\n"
        )
    );
}

#[test]
fn explains_kept_and_skipped_decisions() {
    let (config, registry) = fixture();
    for (name, decision, reason) in [
        (
            "persistent",
            "keep-persistent",
            "persistent artifacts are retained by this profile",
        ),
        (
            "protected",
            "keep-protected",
            "target overlaps a configured keep path",
        ),
        ("missing", "skip-unregistered", "project is not registered"),
    ] {
        let preview = preview_rust_target(&config, &registry, name, CleanupProfile::Normal);
        let report = render_rust_target_preview(&preview);
        assert!(report.contains(&format!("decision: {decision}\n")));
        assert!(report.contains(&format!("reason: {reason}\n")));
        assert!(report.contains("no cleanup executed"));
        assert!(report.contains("Cargo metadata --no-deps --offline --locked may be invoked only when the project is registered"));
        assert!(report.contains(
            "rust_target is enabled, and the anticipated target is not already protected"
        ));
        assert!(report.contains("Cargo docs do not guarantee zero side effects or code execution"));
        assert_eq!(report.lines().count(), 6);
    }
}

#[test]
fn explains_disabled_category_without_authorizing_an_operation() {
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
    let report = render_rust_target_preview(&preview);
    assert!(report.contains("decision: category-disabled\n"));
    assert!(report.contains("rust_target is not enabled in this profile"));
    assert!(report.contains("no cleanup executed"));
}

#[test]
fn protected_path_reason_precedes_disabled_category_reason() {
    let config = Config::from_toml(
        r#"schema_version = 2
default_profile = "normal"
additional_keep_paths = ["/work/demo/target/keep"]
[profiles.low]
categories = []
[profiles.normal]
categories = []
[profiles.aggressive]
categories = []"#,
    )
    .expect("valid protected opt-out config");
    let mut registry = ProjectRegistry::default();
    registry
        .register(
            "demo".into(),
            PathBuf::from("/work/demo"),
            ProjectMode::Ephemeral,
        )
        .unwrap();
    let preview = preview_rust_target(&config, &registry, "demo", CleanupProfile::Normal);
    let report = render_rust_target_preview(&preview);
    assert!(report.contains("decision: keep-protected\n"));
    assert!(report.contains("reason: target overlaps a configured keep path\n"));
    assert!(!report.contains("category-disabled"));
}

#[test]
fn unknown_project_report_does_not_invent_a_target() {
    let (config, registry) = fixture();
    let preview = preview_rust_target(&config, &registry, "missing", CleanupProfile::Aggressive);
    let report = render_rust_target_preview(&preview);
    assert!(report.contains("target: -\n"));
    assert!(!report.contains("missing/target"));
    assert!(!report.contains("/work/"));
}

#[test]
fn escapes_control_characters_in_unknown_project_names() {
    let (config, registry) = fixture();
    let preview = preview_rust_target(
        &config,
        &registry,
        "missing\n\x1b[31m",
        CleanupProfile::Normal,
    );
    let report = render_rust_target_preview(&preview);
    assert_eq!(report, render_rust_target_preview(&preview));
    assert!(report.starts_with("project: \"missing\\n\\u{1b}[31m\"\n"));
    assert!(!report.contains('\x1b'));
    assert_eq!(report.lines().count(), 6);
}
