use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use hule::{
    ProjectMode,
    state::{self, ProjectRegistry, StateError},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "hule-state-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create isolated temp directory");
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn populated_registry() -> ProjectRegistry {
    let mut registry = ProjectRegistry::default();
    registry
        .register(
            "app".into(),
            PathBuf::from("/tmp/work/app"),
            ProjectMode::Persistent,
        )
        .expect("register");
    registry
}

#[test]
fn state_toml_roundtrips_and_only_contains_registry_data() {
    let registry = populated_registry();
    let serialized = registry.to_toml().expect("serialize");
    assert!(serialized.contains("version = 1"));
    assert!(serialized.contains("/tmp/work/app"));
    assert_eq!(
        ProjectRegistry::from_toml(&serialized).expect("parse"),
        registry
    );
}

#[test]
fn strict_validation_rejects_invalid_names_paths_roots_and_overlap() {
    for (name, path, expected) in [
        ("bad name", "/tmp/bad", StateError::InvalidProjectName),
        ("root", "/", StateError::InvalidProjectPath),
        ("traversal", "/tmp/../bad", StateError::InvalidProjectPath),
    ] {
        let mut registry = ProjectRegistry::default();
        assert_eq!(
            registry.register(name.into(), path.into(), ProjectMode::Ephemeral),
            Err(expected)
        );
    }
    let mut registry = ProjectRegistry::default();
    registry
        .register(
            "parent".into(),
            "/tmp/work/app".into(),
            ProjectMode::Persistent,
        )
        .unwrap();
    assert_eq!(
        registry.register(
            "child".into(),
            "/tmp/work/app/child".into(),
            ProjectMode::Ephemeral
        ),
        Err(StateError::OverlappingProjectRoots)
    );
}

#[test]
fn parsing_is_strict_and_errors_do_not_echo_toml_values() {
    let invalid = "version = 1\nsecret_value = \"do-not-echo\"";
    let error = ProjectRegistry::from_toml(invalid).unwrap_err();
    assert_eq!(error, StateError::InvalidToml);
    assert!(!error.to_string().contains("do-not-echo"));
    assert_eq!(
        ProjectRegistry::from_toml("version = 9").unwrap_err(),
        StateError::UnsupportedVersion
    );
}

#[test]
fn missing_is_empty_and_save_load_roundtrip_is_atomic_file_update() {
    let temp = TempDir::new();
    let path = temp.0.join("projects.toml");
    assert!(state::load(&path).unwrap().list().is_empty());
    let registry = populated_registry();
    state::save(&path, &registry).unwrap();
    assert_eq!(state::load(&path).unwrap(), registry);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.with_file_name("projects.toml.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn save_twice_releases_lock_and_creates_private_directories() {
    let temp = TempDir::new();
    let parent = temp.0.join("new").join("state");
    let path = parent.join("projects.toml");
    let registry = populated_registry();

    state::save(&path, &registry).unwrap();
    state::save(&path, &registry).unwrap();
    assert_eq!(state::load(&path).unwrap(), registry);
    assert!(parent.join("projects.toml.lock").is_file());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(parent.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[cfg(unix)]
#[test]
fn group_or_other_writable_state_directory_is_rejected_without_changes() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new();
    let parent = temp.0.join("shared");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    let path = parent.join("projects.toml");
    let contents = "existing state contents";
    fs::write(&path, contents).unwrap();

    assert_eq!(
        state::save(&path, &populated_registry()).unwrap_err(),
        StateError::UnsafeDestination
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    assert_eq!(
        fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o777
    );
}

#[cfg(unix)]
#[test]
fn saving_does_not_change_permissions_of_existing_state_directory() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new();
    let parent = temp.0.join("existing");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    state::save(&parent.join("projects.toml"), &populated_registry()).unwrap();
    assert_eq!(
        fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn lock_file_persists_and_exclusive_lock_is_released_when_handle_drops() {
    use std::fs::OpenOptions;

    let temp = TempDir::new();
    let path = temp.0.join("projects.toml");
    let lock_path = temp.0.join("projects.toml.lock");
    let registry = populated_registry();

    state::save(&path, &registry).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock.try_lock().unwrap();
    assert_eq!(
        state::save(&path, &registry).unwrap_err(),
        StateError::LockExists
    );
    drop(lock);

    state::save(&path, &registry).unwrap();
    assert_eq!(state::load(&path).unwrap(), registry);
    assert!(lock_path.is_file());
}

#[test]
fn invalid_existing_state_is_never_overwritten_by_loading() {
    let temp = TempDir::new();
    let path = temp.0.join("projects.toml");
    fs::write(&path, "invalid secret content").unwrap();
    assert_eq!(state::load(&path).unwrap_err(), StateError::InvalidToml);
    assert_eq!(fs::read_to_string(&path).unwrap(), "invalid secret content");
}

#[cfg(unix)]
#[test]
fn group_or_other_readable_lock_sidecar_is_rejected_without_changes() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new();
    let path = temp.0.join("projects.toml");
    let lock_path = temp.0.join("projects.toml.lock");
    let state_contents = "existing state contents";
    let lock_contents = "existing lock contents";
    fs::write(&path, state_contents).unwrap();
    fs::write(&lock_path, lock_contents).unwrap();
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(
        state::save(&path, &populated_registry()).unwrap_err(),
        StateError::UnsafeDestination
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), state_contents);
    assert_eq!(fs::read_to_string(&lock_path).unwrap(), lock_contents);
    assert_eq!(
        fs::metadata(lock_path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[cfg(unix)]
#[test]
fn lock_sidecar_symlink_is_rejected_without_touching_link_target() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new();
    let path = temp.0.join("projects.toml");
    let lock = temp.0.join("projects.toml.lock");
    let target = temp.0.join("other");
    fs::write(&target, "untouched").unwrap();
    symlink(&target, &lock).unwrap();
    assert_eq!(
        state::save(&path, &populated_registry()).unwrap_err(),
        StateError::UnsafeDestination
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
}

#[cfg(unix)]
#[test]
fn destination_symlink_is_rejected_without_touching_link_target() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new();
    let path = temp.0.join("projects.toml");
    let target = temp.0.join("other");
    fs::write(&target, "untouched").unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(
        state::save(&path, &populated_registry()).unwrap_err(),
        StateError::UnsafeDestination
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
}

#[test]
fn transaction_rejects_lock_contention_without_mutating_registry() {
    use std::fs::OpenOptions;
    let temp = TempDir::new();
    let path = temp.0.join("projects.toml");
    state::save(&path, &populated_registry()).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.0.join("projects.toml.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let result = state::update(&path, |registry| registry.unregister("app").map(|_| ()));
    assert_eq!(result.unwrap_err(), StateError::LockExists);
    assert!(state::load(&path).unwrap().get("app").is_some());
}

#[cfg(unix)]
#[test]
fn load_rejects_symlinks_and_non_regular_existing_files() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new();
    let target = temp.0.join("target");
    fs::write(&target, "version = 1").unwrap();
    let link = temp.0.join("link");
    symlink(target, &link).unwrap();
    assert_eq!(
        state::load(&link).unwrap_err(),
        StateError::UnsafeDestination
    );
    assert_eq!(
        state::load(&temp.0).unwrap_err(),
        StateError::UnsafeDestination
    );
}

#[test]
fn pure_unregister_requires_an_explicit_existing_name() {
    let mut registry = populated_registry();
    assert_eq!(
        registry.unregister("missing").unwrap_err(),
        StateError::ProjectNotRegistered
    );
    assert!(registry.unregister("app").is_ok());
    assert!(registry.list().is_empty());
}
