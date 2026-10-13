use hule::{
    CleanupProfile,
    action_plan::GenerationIdentity,
    config::Config,
    nix_generations::{self, GenerationError, GenerationRunner, GenerationStatus},
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

struct MockRunner {
    identity: GenerationIdentity,
    remove_ids: Vec<u32>,
    succeeds: bool,
    calls: Vec<(String, PathBuf, Vec<u32>)>,
}

impl GenerationRunner for MockRunner {
    fn identity(&mut self, _: &str) -> Result<GenerationIdentity, GenerationError> {
        Ok(self.identity.clone())
    }

    fn delete_generations(
        &mut self,
        endpoint: &str,
        profile: &Path,
        ids: &[u32],
    ) -> Result<(), GenerationError> {
        self.calls
            .push((endpoint.into(), profile.to_path_buf(), ids.to_vec()));
        for id in ids {
            if self.remove_ids.contains(id) {
                let _ = fs::remove_file(profile.with_file_name(format!("home-manager-{id}-link")));
            }
        }
        if self.succeeds {
            Ok(())
        } else {
            Err(GenerationError::Command)
        }
    }
}

struct TempRoot {
    _temp: tempfile::TempDir,
    canonical: PathBuf,
}

impl TempRoot {
    fn path(&self) -> &Path {
        &self.canonical
    }
}

struct Fixture {
    root: TempRoot,
    endpoint: String,
    profile: PathBuf,
    roots: Vec<PathBuf>,
    targets: Vec<PathBuf>,
    config: Config,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let home = root_path.join("home");
        let profile_dir = home.join(".local/state/nix/profiles");
        let store = root_path.join("store");
        let root_path_modern = home.join(".local/share/home-manager/gcroots/current-home");
        fs::create_dir_all(&profile_dir).unwrap();
        fs::create_dir_all(root_path_modern.parent().unwrap()).unwrap();
        fs::create_dir_all(&store).unwrap();
        let targets = (1..=3)
            .map(|id| {
                let target = store.join(format!("synthetic-generation-{id}"));
                fs::create_dir(&target).unwrap();
                fs::write(target.join("payload"), format!("generation-{id}")).unwrap();
                fs::set_permissions(&target, fs::Permissions::from_mode(0o555)).unwrap();
                target
            })
            .collect::<Vec<_>>();
        let profile = profile_dir.join("home-manager");
        for (id, target) in (1..=3).zip(&targets) {
            symlink(target, profile_dir.join(format!("home-manager-{id}-link"))).unwrap();
        }
        symlink("home-manager-3-link", &profile).unwrap();
        symlink(&targets[2], &root_path_modern).unwrap();
        let endpoint = format!(
            "local?store={}&state={}/state&log={}/log",
            root_path.join("store").display(),
            root_path.display(),
            root_path.display()
        );
        let config = Config::from_toml(&format!(
            "schema_version=2\ndefault_profile='normal'\n[profiles.low]\ncategories=[]\n[profiles.normal]\ncategories=['nix_home_manager_generations']\n[profiles.aggressive]\ncategories=[]\n[nix_profiles.hm]\nkind='home_manager'\npath='{}'\ncurrent_home_roots=['{}']\n",
            profile.display(),
            root_path_modern.display()
        ))
        .unwrap();
        Self {
            root: TempRoot {
                _temp: root,
                canonical: root_path,
            },
            endpoint,
            profile,
            roots: vec![root_path_modern],
            targets,
            config,
        }
    }

    fn runner(&self, remove_ids: &[u32], succeeds: bool) -> MockRunner {
        MockRunner {
            identity: GenerationIdentity {
                endpoint: "daemon".into(),
                store_version: "nix-store (Nix) 2.34.6".into(),
                nix_env_version: "nix-env (Nix) 2.34.6".into(),
                store_dir: self.root.path().join("store"),
            },
            remove_ids: remove_ids.to_vec(),
            succeeds,
            calls: Vec::new(),
        }
    }
}

#[test]
fn apply_deletes_only_sorted_selected_ids_and_preserves_current_roots_and_objects() {
    let fixture = Fixture::new();
    let mut runner = fixture.runner(&[1, 2], true);
    let plan = nix_generations::local_test_support::create_plan(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        "hm",
        &[2, 1],
        &fixture.endpoint,
        fixture.root.path(),
    )
    .unwrap();
    assert_eq!(plan.selected_ids, [1, 2]);
    let result = nix_generations::local_test_support::apply(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        &plan,
        &fixture.endpoint,
        fixture.root.path(),
        |_| Ok(true),
    )
    .unwrap();
    assert!(result.is_complete());
    assert_eq!(
        result.statuses.iter().map(|s| s.status).collect::<Vec<_>>(),
        [GenerationStatus::Removed; 2]
    );
    assert_eq!(
        runner.calls,
        [(
            fixture.endpoint.clone(),
            fixture.profile.clone(),
            vec![1, 2]
        )]
    );
    assert!(
        !fixture
            .profile
            .with_file_name("home-manager-1-link")
            .exists()
    );
    assert!(
        !fixture
            .profile
            .with_file_name("home-manager-2-link")
            .exists()
    );
    assert!(
        fixture
            .profile
            .with_file_name("home-manager-3-link")
            .exists()
    );
    assert_eq!(
        fs::read_link(&fixture.profile).unwrap(),
        Path::new("home-manager-3-link")
    );
    assert_eq!(
        fs::read_link(&fixture.roots[0]).unwrap(),
        fixture.targets[2]
    );
    for target in &fixture.targets {
        assert!(
            target.join("payload").exists(),
            "profile history removal is not GC"
        );
    }
}

#[test]
fn rejects_current_generation_before_any_native_delete() {
    let fixture = Fixture::new();
    let mut runner = fixture.runner(&[], true);
    let result = nix_generations::local_test_support::create_plan(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        "hm",
        &[3],
        &fixture.endpoint,
        fixture.root.path(),
    );
    assert_eq!(result.unwrap_err(), GenerationError::UnsafePath);
    assert!(runner.calls.is_empty());
}

#[test]
fn incomplete_native_results_are_truthful_and_never_claim_reclamation() {
    let fixture = Fixture::new();
    let mut runner = fixture.runner(&[1], false);
    let plan = nix_generations::local_test_support::create_plan(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        "hm",
        &[1],
        &fixture.endpoint,
        fixture.root.path(),
    )
    .unwrap();
    let result = nix_generations::local_test_support::apply(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        &plan,
        &fixture.endpoint,
        fixture.root.path(),
        |_| Ok(true),
    )
    .unwrap();
    assert!(!result.command_succeeded);
    assert!(!result.is_complete());
    assert_eq!(
        result.statuses[0].status,
        GenerationStatus::AbsentAfterFailure
    );
    assert!(fixture.targets[0].exists());
}

#[test]
fn rejects_fixture_endpoint_and_stale_activation_root() {
    let fixture = Fixture::new();
    let mut runner = fixture.runner(&[], true);
    assert_eq!(
        nix_generations::local_test_support::create_plan(
            &mut runner,
            &fixture.config,
            CleanupProfile::Normal,
            "hm",
            &[1],
            "local?store=/nix/store",
            fixture.root.path(),
        )
        .unwrap_err(),
        GenerationError::UnsafePath
    );
    let plan = nix_generations::local_test_support::create_plan(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        "hm",
        &[1],
        &fixture.endpoint,
        fixture.root.path(),
    )
    .unwrap();
    fs::remove_file(&fixture.roots[0]).unwrap();
    symlink(&fixture.targets[1], &fixture.roots[0]).unwrap();
    assert_eq!(
        nix_generations::local_test_support::apply(
            &mut runner,
            &fixture.config,
            CleanupProfile::Normal,
            &plan,
            &fixture.endpoint,
            fixture.root.path(),
            |_| Ok(true),
        )
        .unwrap_err(),
        GenerationError::Stale
    );
    assert!(runner.calls.is_empty());
}

#[test]
fn rejects_writable_profile_ancestor_before_planning() {
    let fixture = Fixture::new();
    let nix_dir = fixture.profile.parent().unwrap().parent().unwrap();
    fs::set_permissions(nix_dir, fs::Permissions::from_mode(0o777)).unwrap();
    let mut runner = fixture.runner(&[], true);

    let result = nix_generations::local_test_support::create_plan(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        "hm",
        &[1],
        &fixture.endpoint,
        fixture.root.path(),
    );

    assert_eq!(result.unwrap_err(), GenerationError::UnsafePath);
    assert!(runner.calls.is_empty());
}

#[test]
fn rejects_symlinked_profile_ancestor_before_planning() {
    let fixture = Fixture::new();
    let nix_dir = fixture.profile.parent().unwrap().parent().unwrap();
    let renamed = nix_dir.with_file_name("nix-real");
    fs::rename(nix_dir, &renamed).unwrap();
    symlink(&renamed, nix_dir).unwrap();
    let mut runner = fixture.runner(&[], true);

    let result = nix_generations::local_test_support::create_plan(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        "hm",
        &[1],
        &fixture.endpoint,
        fixture.root.path(),
    );

    assert_eq!(result.unwrap_err(), GenerationError::UnsafePath);
    assert!(runner.calls.is_empty());
}

#[test]
fn fixture_endpoint_rejects_store_and_profile_aliases() {
    let fixture = Fixture::new();
    let alias = fixture.root.path().join("store-alias");
    symlink(fixture.root.path().join("store"), &alias).unwrap();
    let forged_endpoint = format!(
        "local?store={}&state={}/state&log={}/log",
        alias.display(),
        fixture.root.path().display(),
        fixture.root.path().display()
    );
    let mut runner = fixture.runner(&[], true);
    assert_eq!(
        nix_generations::local_test_support::create_plan(
            &mut runner,
            &fixture.config,
            CleanupProfile::Normal,
            "hm",
            &[1],
            &forged_endpoint,
            fixture.root.path(),
        )
        .unwrap_err(),
        GenerationError::UnsafePath
    );
    assert!(
        fixture
            .config
            .nix_profiles()
            .get("hm")
            .unwrap()
            .path()
            .starts_with(fixture.root.path())
    );
}
