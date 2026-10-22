use hule::{
    CleanupProfile, ProjectMode,
    config::Config,
    inventory::{
        InventoryError, InventoryIssue, InventoryKind, InventoryLimits, InventoryState,
        inspect_rust_target, inspect_xcode_derived_data,
    },
    state::ProjectRegistry,
};
use std::{fs, path::Path};

fn config(rust_enabled: bool, keep_paths: &[&Path]) -> Config {
    let categories = if rust_enabled {
        "[\"rust_target\"]"
    } else {
        "[]"
    };
    let keep = keep_paths
        .iter()
        .map(|path| format!("\"{}\"", path.display()))
        .collect::<Vec<_>>()
        .join(", ");
    Config::from_toml(&format!(
        "schema_version=2\ndefault_profile='normal'\nadditional_keep_paths=[{keep}]\n[profiles.low]\ncategories={categories}\n[profiles.normal]\ncategories={categories}\n[profiles.aggressive]\ncategories={categories}\n"
    ))
    .unwrap()
}

fn xcode_root(base: &Path) -> std::path::PathBuf {
    let root = base.join("Library/Developer/Xcode/DerivedData");
    fs::create_dir_all(&root).unwrap();
    fs::canonicalize(root).unwrap()
}

fn rust_fixture(
    mode: ProjectMode,
) -> (
    tempfile::TempDir,
    Config,
    ProjectRegistry,
    std::path::PathBuf,
) {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project with spaces");
    let target = project.join("custom-build/target");
    fs::create_dir_all(&target).unwrap();
    fs::write(project.join("Cargo.toml"), "[package]\nname='fixture'\n").unwrap();
    let project = fs::canonicalize(project).unwrap();
    let target = fs::canonicalize(target).unwrap();
    let mut registry = ProjectRegistry::default();
    registry.register("fixture".into(), project, mode).unwrap();
    (fixture, config(true, &[]), registry, target)
}

#[test]
fn xcode_inventory_is_bounded_non_authorizing_and_does_not_follow_symlinks() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().unwrap();
    let root = xcode_root(fixture.path());
    let project = root.join("Example Project");
    let build_products = project.join("Build/Products");
    fs::create_dir_all(&build_products).unwrap();
    let file = build_products.join("artifact with spaces.o");
    fs::write(&file, b"four").unwrap();
    let hard_link = build_products.join("hard-link.o");
    fs::hard_link(&file, &hard_link).unwrap();
    let outside = fixture.path().join("outside-secret");
    fs::write(&outside, b"outside bytes must not be counted").unwrap();
    #[cfg(unix)]
    symlink(&outside, build_products.join("outside-link")).unwrap();

    let report = inspect_xcode_derived_data(&root, &InventoryLimits::default()).unwrap();
    assert_eq!(report.kind(), InventoryKind::XcodeDerivedData);
    assert_eq!(report.state(), InventoryState::Complete);
    assert_eq!(
        report.authorization(),
        hule::inventory::InventoryAuthorization::NotAuthorized
    );
    let summary = report.summary().unwrap();
    assert_eq!(summary.file_entries(), 2);
    assert_eq!(summary.symlink_entries(), 1);
    assert_eq!(summary.apparent_bytes(), 4);
    let output_limit = InventoryLimits::default().max_output_bytes;
    let json = report.to_json_limited(output_limit).unwrap();
    let repeated = inspect_xcode_derived_data(&root, &InventoryLimits::default()).unwrap();
    assert_eq!(json, repeated.to_json_limited(output_limit).unwrap());
    assert!(!json.contains(&root.to_string_lossy().to_string()));
    assert!(!json.contains("Example Project"));
    assert!(!json.contains("artifact with spaces.o"));
    assert!(!json.contains("outside-secret"));
}

#[test]
fn xcode_inventory_rejects_archives_sdk_and_simulator_roots() {
    let fixture = tempfile::tempdir().unwrap();
    for path in [
        fixture.path().join("Library/Developer/Xcode/Archives"),
        fixture
            .path()
            .join("Library/Developer/CoreSimulator/Devices"),
        fixture
            .path()
            .join("Library/Developer/Xcode/Platforms/iPhoneOS.platform/Developer/SDKs"),
    ] {
        fs::create_dir_all(&path).unwrap();
        let canonical = fs::canonicalize(path).unwrap();
        assert_eq!(
            inspect_xcode_derived_data(&canonical, &InventoryLimits::default()).unwrap_err(),
            InventoryError::InvalidXcodeRoot
        );
    }
}

#[test]
fn xcode_inventory_reports_root_type_and_missing_read_errors_without_partial_totals() {
    let fixture = tempfile::tempdir().unwrap();
    let _root = xcode_root(fixture.path());
    let file_root = fixture.path().join("Library/Developer/Xcode/DerivedData");
    fs::remove_dir_all(&file_root).unwrap();
    fs::write(&file_root, b"not a directory").unwrap();
    let canonical_parent = fs::canonicalize(file_root.parent().unwrap()).unwrap();
    let file_root = canonical_parent.join("DerivedData");
    assert_eq!(
        inspect_xcode_derived_data(&file_root, &InventoryLimits::default()).unwrap_err(),
        InventoryError::InvalidXcodeRoot
    );

    let missing = fixture
        .path()
        .join("missing/Library/Developer/Xcode/DerivedData");
    let report = inspect_xcode_derived_data(&missing, &InventoryLimits::default()).unwrap();
    assert_eq!(report.state(), InventoryState::Incomplete);
    assert_eq!(report.issue(), Some(InventoryIssue::RootUnavailable));
    assert!(report.summary().is_none());
}

#[cfg(unix)]
#[test]
fn xcode_inventory_rejects_a_symlinked_root() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().unwrap();
    let real = xcode_root(fixture.path());
    let alias = fixture
        .path()
        .join("alias/Library/Developer/Xcode/DerivedData");
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    symlink(&real, &alias).unwrap();
    assert_eq!(
        inspect_xcode_derived_data(&alias, &InventoryLimits::default()).unwrap_err(),
        InventoryError::InvalidXcodeRoot
    );
}

#[test]
fn rust_inventory_requires_registration_opt_in_and_retention_eligibility() {
    let (fixture, enabled, registry, target) = rust_fixture(ProjectMode::Ephemeral);
    let report = inspect_rust_target(
        &enabled,
        &registry,
        "fixture",
        CleanupProfile::Normal,
        &target,
        &InventoryLimits::default(),
    )
    .unwrap();
    assert_eq!(report.kind(), InventoryKind::RustTarget);
    assert_eq!(report.state(), InventoryState::Complete);

    assert_eq!(
        inspect_rust_target(
            &enabled,
            &registry,
            "missing",
            CleanupProfile::Normal,
            &target,
            &InventoryLimits::default(),
        )
        .unwrap_err(),
        InventoryError::Unregistered
    );
    let disabled = config(false, &[]);
    assert_eq!(
        inspect_rust_target(
            &disabled,
            &registry,
            "fixture",
            CleanupProfile::Normal,
            Path::new("/not/scanned/target"),
            &InventoryLimits::default(),
        )
        .unwrap_err(),
        InventoryError::CategoryDisabled
    );

    let (_persistent_fixture, persistent_config, persistent_registry, persistent_target) =
        rust_fixture(ProjectMode::Persistent);
    assert_eq!(
        inspect_rust_target(
            &persistent_config,
            &persistent_registry,
            "fixture",
            CleanupProfile::Normal,
            Path::new("/not/scanned/target"),
            &InventoryLimits::default(),
        )
        .unwrap_err(),
        InventoryError::Retained
    );
    let aggressive = inspect_rust_target(
        &persistent_config,
        &persistent_registry,
        "fixture",
        CleanupProfile::Aggressive,
        &persistent_target,
        &InventoryLimits::default(),
    )
    .unwrap();
    assert_eq!(aggressive.state(), InventoryState::Complete);
    drop(fixture);
}

#[test]
fn rust_inventory_rejects_protected_targets_and_symlink_escape() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    let (fixture, enabled, registry, target) = rust_fixture(ProjectMode::Ephemeral);
    let protected = config(true, &[target.as_path()]);
    assert_eq!(
        inspect_rust_target(
            &protected,
            &registry,
            "fixture",
            CleanupProfile::Aggressive,
            &target,
            &InventoryLimits::default(),
        )
        .unwrap_err(),
        InventoryError::Protected
    );

    let outside = fixture.path().join("outside-target");
    fs::create_dir(&outside).unwrap();
    let project = registry.get("fixture").unwrap().path();
    let alias = project.join("escape-target");
    #[cfg(unix)]
    symlink(&outside, &alias).unwrap();
    assert_eq!(
        inspect_rust_target(
            &enabled,
            &registry,
            "fixture",
            CleanupProfile::Aggressive,
            &alias,
            &InventoryLimits::default(),
        )
        .unwrap_err(),
        InventoryError::UnsafeTarget
    );
}

#[test]
fn inventory_caps_and_deadline_return_incomplete_without_partial_summary() {
    let fixture = tempfile::tempdir().unwrap();
    let root = xcode_root(fixture.path());
    fs::create_dir_all(root.join("one/two/three")).unwrap();
    fs::write(root.join("one/a"), b"a").unwrap();
    fs::write(root.join("one/b"), b"b").unwrap();

    let entry_limited = inspect_xcode_derived_data(
        &root,
        &InventoryLimits {
            max_entries: 1,
            ..InventoryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(entry_limited.state(), InventoryState::Incomplete);
    assert_eq!(entry_limited.issue(), Some(InventoryIssue::EntryLimit));
    assert!(entry_limited.summary().is_none());

    let depth_limited = inspect_xcode_derived_data(
        &root,
        &InventoryLimits {
            max_depth: 1,
            ..InventoryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(depth_limited.state(), InventoryState::Incomplete);
    assert_eq!(depth_limited.issue(), Some(InventoryIssue::DepthLimit));
    assert!(depth_limited.summary().is_none());

    let deadline = inspect_xcode_derived_data(
        &root,
        &InventoryLimits {
            max_duration: std::time::Duration::ZERO,
            ..InventoryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(deadline.state(), InventoryState::TimeoutRequested);
    assert_eq!(deadline.issue(), Some(InventoryIssue::DeadlineExceeded));
    assert!(deadline.summary().is_none());

    let output_limited = inspect_xcode_derived_data(
        &root,
        &InventoryLimits {
            max_output_bytes: 1,
            ..InventoryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(output_limited.state(), InventoryState::Incomplete);
    assert_eq!(output_limited.issue(), Some(InventoryIssue::OutputLimit));
    assert!(output_limited.summary().is_none());
    assert!(output_limited.to_json_limited(1).is_none());
}

#[cfg(unix)]
#[test]
fn permission_errors_do_not_publish_partial_inventory() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let root = xcode_root(fixture.path());
    fs::write(root.join("readable"), b"ok").unwrap();
    let denied = root.join("denied");
    fs::create_dir(&denied).unwrap();
    fs::write(denied.join("hidden"), b"no").unwrap();
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();

    let report = inspect_xcode_derived_data(&root, &InventoryLimits::default()).unwrap();
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(report.state(), InventoryState::Incomplete);
    assert_eq!(report.issue(), Some(InventoryIssue::ReadFailed));
    assert!(report.summary().is_none());
}
