//! Native M2 acceptance. Invoke only inside the approved disposable Linux guest.
use hule::{
    CleanupProfile,
    config::Config,
    nix_generations::{self, GenerationError, GenerationStatus},
};
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    uri: String,
    profile: PathBuf,
    root_link: PathBuf,
    targets: Vec<PathBuf>,
    sentinel: PathBuf,
    other_profile: PathBuf,
    config: Config,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary native fixture");
        let root = fs::canonicalize(temp.path()).expect("canonical temporary root");
        let profile_dir = root.join("home/.local/state/nix/profiles");
        let store = root.join("store");
        let state = root.join("state");
        let log = root.join("log");
        let root_link = root.join("home/.local/share/home-manager/gcroots/current-home");
        fs::create_dir_all(&profile_dir).unwrap();
        fs::create_dir_all(root_link.parent().unwrap()).unwrap();
        fs::create_dir_all(&store).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&log).unwrap();
        let uri = format!(
            "local?store={}&state={}&log={}",
            store.display(),
            state.display(),
            log.display()
        );
        let mut targets = Vec::new();
        for id in 1..=3 {
            let source = root.join(format!("source/generation-{id}"));
            fs::create_dir_all(&source).unwrap();
            fs::write(source.join("payload"), format!("synthetic generation {id}")).unwrap();
            let output = nix_store(
                &uri,
                &[
                    "--option",
                    "min-free",
                    "0",
                    "--add",
                    source.to_str().unwrap(),
                ],
            )
            .expect("nix-store command available");
            assert!(output.status.success(), "nix-store --add failed");
            let path = String::from_utf8(output.stdout).unwrap().trim().to_owned();
            let target = PathBuf::from(path);
            assert!(
                target.starts_with(&store),
                "logical store target must be physically inside fixture"
            );
            assert_eq!(fs::canonicalize(&target).unwrap(), target);
            targets.push(target);
        }
        let sentinel_source = root.join("source/unselected-sentinel");
        fs::create_dir_all(&sentinel_source).unwrap();
        fs::write(sentinel_source.join("payload"), b"unselected sentinel").unwrap();
        let output = nix_store(
            &uri,
            &[
                "--option",
                "min-free",
                "0",
                "--add",
                sentinel_source.to_str().unwrap(),
            ],
        )
        .unwrap();
        assert!(output.status.success(), "sentinel store add failed");
        let sentinel = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        assert!(sentinel.starts_with(&store));

        let profile = profile_dir.join("home-manager");
        for target in &targets {
            let set = nix_env(&uri, &profile, &["--set", target.to_str().unwrap()]).unwrap();
            assert!(set.status.success(), "nix-env --set failed");
        }
        let other_profile = profile_dir.join("other");
        for target in [&targets[0], &targets[2]] {
            let set = nix_env(&uri, &other_profile, &["--set", target.to_str().unwrap()]).unwrap();
            assert!(set.status.success(), "unrelated profile setup failed");
        }
        symlink(&targets[2], &root_link).unwrap();
        let config = Config::from_toml(&format!(
            "schema_version=2\ndefault_profile='normal'\n[profiles.low]\ncategories=[]\n[profiles.normal]\ncategories=['nix_home_manager_generations']\n[profiles.aggressive]\ncategories=[]\n[nix_profiles.hm]\nkind='home_manager'\npath='{}'\ncurrent_home_roots=['{}']\n",
            profile.display(), root_link.display()
        ))
        .unwrap();
        Self {
            _temp: temp,
            root,
            uri,
            profile,
            root_link,
            targets,
            sentinel,
            other_profile,
            config,
        }
    }

    fn user_profile(&self, leaf: &str, kind: &str, roots: &[PathBuf]) -> (PathBuf, Config) {
        let profile = self.root.join("home/.local/state/nix/profiles").join(leaf);
        let profile_dir = profile.parent().unwrap();
        fs::create_dir_all(profile_dir).unwrap();
        let roots_toml = if roots.is_empty() {
            String::new()
        } else {
            format!(
                "current_home_roots=[{}]",
                roots
                    .iter()
                    .map(|path| format!("'{}'", path.display()))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        let config = Config::from_toml(&format!(
            "schema_version=2\ndefault_profile='normal'\n[profiles.low]\ncategories=[]\n[profiles.normal]\ncategories=['nix_user_generations']\n[profiles.aggressive]\ncategories=[]\n[nix_profiles.selected]\nkind='{}'\npath='{}'\n{}\n",
            kind,
            profile.display(),
            roots_toml,
        ))
        .unwrap();
        (profile, config)
    }
}

fn nix_store(uri: &str, args: &[&str]) -> Result<Output, String> {
    Command::new("nix-store")
        .arg("--store")
        .arg(uri)
        .args(args)
        .output()
        .map_err(|_| "nix-store executable unavailable".into())
}

fn nix_env(uri: &str, profile: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("nix-env")
        .arg("--store")
        .arg(uri)
        .arg("--option")
        .arg("min-free")
        .arg("0")
        .arg("--profile")
        .arg(profile)
        .args(args)
        .output()
        .map_err(|_| "nix-env executable unavailable".into())
}

fn native_derivation(uri: &str, root: &Path, name: &str, payload: &str) -> PathBuf {
    let expression = format!(
        r#"derivation {{
  name = "{name}";
  system = "aarch64-linux";
  builder = "/bin/sh";
  args = [ "-c" "/usr/bin/mkdir -p \"$out\"; printf '%s' '{payload}' > \"$out/{name}\"" ];
}}"#
    );
    let output = Command::new("nix-instantiate")
        .arg("--store")
        .arg(uri)
        .args(["--option", "min-free", "0"])
        .args(["--option", "substitute", "false"])
        .args(["--option", "sandbox", "false"])
        .arg("--option")
        .arg("build-dir")
        .arg(root.join("build"))
        .args(["--expr", &expression])
        .output()
        .expect("nix-instantiate executable available");
    assert!(
        output.status.success(),
        "nix-instantiate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn install_legacy(uri: &str, profile: &Path, root: &Path, derivation: &Path) {
    let output = Command::new("nix-env")
        .arg("--store")
        .arg(uri)
        .args(["--option", "min-free", "0"])
        .args(["--option", "substitute", "false"])
        .args(["--option", "sandbox", "false"])
        .arg("--option")
        .arg("build-dir")
        .arg(root.join("build"))
        .arg("--profile")
        .arg(profile)
        .arg("--install")
        .arg(derivation)
        .output()
        .expect("nix-env executable available");
    assert!(
        output.status.success(),
        "nix-env --install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn add_modern(uri: &str, profile: &Path, package: &Path) {
    let output = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "--store",
            uri,
        ])
        .args(["--option", "min-free", "0"])
        .args(["--option", "substitute", "false"])
        .args(["profile", "add", "--profile"])
        .arg(profile)
        .arg(package)
        .output()
        .expect("nix executable available");
    assert!(
        output.status.success(),
        "nix profile add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn switch_generation(uri: &str, profile: &Path, id: u32) -> Output {
    nix_env(uri, profile, &["--switch-generation", &id.to_string()]).unwrap()
}

fn native_package(fixture: &Fixture, name: &str, payload: &str) -> PathBuf {
    let source = fixture.root.join(format!("source/{name}"));
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join(name), payload).unwrap();
    let output = nix_store(
        &fixture.uri,
        &[
            "--option",
            "min-free",
            "0",
            "--add",
            source.to_str().unwrap(),
        ],
    )
    .unwrap();
    assert!(output.status.success(), "nix-store --add package failed");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn apply_selected_user_generation(fixture: &Fixture, profile: &Path, config: &Config) -> PathBuf {
    let selected_link = profile.with_file_name(format!(
        "{}-1-link",
        profile.file_name().unwrap().to_string_lossy()
    ));
    let selected_target = fs::read_link(&selected_link).unwrap();
    let mut runner =
        nix_generations::local_test_support::command_runner(&fixture.uri, &fixture.root).unwrap();
    let plan = nix_generations::local_test_support::create_plan(
        &mut runner,
        config,
        CleanupProfile::Normal,
        "selected",
        &[1],
        &fixture.uri,
        &fixture.root,
    )
    .expect("Hule plans a selected noncurrent user-profile generation");
    let result = nix_generations::local_test_support::apply(
        &mut runner,
        config,
        CleanupProfile::Normal,
        &plan,
        &fixture.uri,
        &fixture.root,
        |_| Ok(true),
    )
    .expect("Hule applies the selected user generation");
    assert!(result.is_complete());
    assert_eq!(result.statuses[0].status, GenerationStatus::Removed);
    assert!(fs::symlink_metadata(&selected_link).is_err());
    assert_eq!(
        fs::read_link(profile).unwrap(),
        PathBuf::from(format!(
            "{}-2-link",
            profile.file_name().unwrap().to_string_lossy()
        ))
    );
    assert!(
        selected_target.exists(),
        "history removal must retain its store object without GC"
    );
    selected_target
}

#[test]
#[ignore = "requires the approved isolated OrbStack guest and Nix 2.34.6; run explicitly for M2 native acceptance"]
fn hule_selected_generation_apply_preserves_current_root_payloads_and_other_profile() {
    let fixture = Fixture::new();
    let mut runner =
        nix_generations::local_test_support::command_runner(&fixture.uri, &fixture.root)
            .expect("sealed test command runner");
    let plan = nix_generations::local_test_support::create_plan(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        "hm",
        &[2, 1],
        &fixture.uri,
        &fixture.root,
    )
    .expect("Hule creates a sealed local generation plan");
    assert_eq!(plan.selected_ids, [1, 2]);

    let current_preflight = nix_generations::local_test_support::create_plan(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        "hm",
        &[3],
        &fixture.uri,
        &fixture.root,
    );
    assert_eq!(current_preflight.unwrap_err(), GenerationError::UnsafePath);

    let result = nix_generations::local_test_support::apply(
        &mut runner,
        &fixture.config,
        CleanupProfile::Normal,
        &plan,
        &fixture.uri,
        &fixture.root,
        |_| Ok(true),
    )
    .expect("Hule applies saved selected IDs");
    assert!(
        result.is_complete(),
        "native apply must verify preservation"
    );
    assert_eq!(
        result
            .statuses
            .iter()
            .map(|row| row.status)
            .collect::<Vec<_>>(),
        [GenerationStatus::Removed; 2]
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
        fs::read_link(&fixture.root_link).unwrap(),
        fixture.targets[2]
    );
    for target in &fixture.targets {
        assert!(
            target.exists(),
            "deleting history must not delete store objects"
        );
        assert!(target.join("payload").exists());
    }
    assert!(fixture.sentinel.exists());
    assert!(fixture.other_profile.exists());
    assert!(
        fixture
            .other_profile
            .with_file_name("other-1-link")
            .exists()
    );
    assert!(
        fixture
            .other_profile
            .with_file_name("other-2-link")
            .exists()
    );

    let native_current = nix_env(
        &fixture.uri,
        &fixture.profile,
        &["--delete-generations", "3"],
    )
    .unwrap();
    assert!(
        !native_current.status.success(),
        "Nix must refuse its current generation"
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
        fs::read_link(&fixture.root_link).unwrap(),
        fixture.targets[2]
    );
}

#[test]
#[ignore = "requires the approved isolated OrbStack guest and Nix 2.34.6; run explicitly for M2 native acceptance"]
fn hule_deletes_from_a_true_legacy_nix_env_profile_without_removing_objects() {
    let fixture = Fixture::new();
    let (profile, config) = fixture.user_profile("profile", "user", &[]);
    fs::create_dir_all(fixture.root.join("build")).unwrap();
    let first = native_derivation(&fixture.uri, &fixture.root, "legacy-one", "legacy-one");
    install_legacy(&fixture.uri, &profile, &fixture.root, &first);
    let second = native_derivation(&fixture.uri, &fixture.root, "legacy-two", "legacy-two");
    install_legacy(&fixture.uri, &profile, &fixture.root, &second);
    let current = fs::canonicalize(&profile).unwrap();
    assert!(
        current.join("manifest.nix").is_file(),
        "nix-env --install must create legacy manifest.nix"
    );
    assert!(!current.join("manifest.json").exists());
    assert!(current.starts_with(fixture.root.join("store")));

    let selected_target = apply_selected_user_generation(&fixture, &profile, &config);
    assert!(selected_target.exists());
    let current = fs::canonicalize(&profile).unwrap();
    assert!(current.join("manifest.nix").is_file());
    assert!(fs::symlink_metadata(profile.with_file_name("profile-2-link")).is_ok());
}

#[test]
#[ignore = "requires the approved isolated OrbStack guest and Nix 2.34.6; run explicitly for M2 native acceptance"]
fn hule_deletes_from_a_true_modern_nix_profile_without_removing_objects() {
    let fixture = Fixture::new();
    let (profile, config) = fixture.user_profile("profile", "user", &[]);
    let first = native_package(&fixture, "modern-one", "modern-one");
    add_modern(&fixture.uri, &profile, &first);
    let second = native_package(&fixture, "modern-two", "modern-two");
    add_modern(&fixture.uri, &profile, &second);
    let current = fs::canonicalize(&profile).unwrap();
    assert!(
        current.join("manifest.json").is_file(),
        "nix profile add must create manifest.json"
    );
    assert!(!current.join("manifest.nix").exists());
    assert!(current.starts_with(fixture.root.join("store")));

    let selected_target = apply_selected_user_generation(&fixture, &profile, &config);
    assert!(selected_target.exists());
    let current = fs::canonicalize(&profile).unwrap();
    assert!(current.join("manifest.json").is_file());
    assert!(fs::symlink_metadata(profile.with_file_name("profile-2-link")).is_ok());
    assert!(first.join("modern-one").is_file());
    assert!(second.join("modern-two").is_file());
}

#[test]
#[ignore = "requires the approved isolated OrbStack guest and Nix 2.34.6; run explicitly for M2 native acceptance"]
fn native_current_switch_after_hule_plan_is_stale_and_native_delete_refuses() {
    let fixture = Fixture::new();
    let (profile, config) = fixture.user_profile("profile", "user", &[]);
    fs::create_dir_all(fixture.root.join("build")).unwrap();
    let first = native_derivation(&fixture.uri, &fixture.root, "switch-one", "switch-one");
    install_legacy(&fixture.uri, &profile, &fixture.root, &first);
    let second = native_derivation(&fixture.uri, &fixture.root, "switch-two", "switch-two");
    install_legacy(&fixture.uri, &profile, &fixture.root, &second);
    let selected_link = profile.with_file_name("profile-1-link");
    let selected_target = fs::read_link(&selected_link).unwrap();
    assert!(
        fs::canonicalize(&profile)
            .unwrap()
            .join("manifest.nix")
            .is_file()
    );

    let mut runner =
        nix_generations::local_test_support::command_runner(&fixture.uri, &fixture.root).unwrap();
    let plan = nix_generations::local_test_support::create_plan(
        &mut runner,
        &config,
        CleanupProfile::Normal,
        "selected",
        &[1],
        &fixture.uri,
        &fixture.root,
    )
    .expect("selected noncurrent generation is initially plannable");
    let switched = switch_generation(&fixture.uri, &profile, 1);
    assert!(
        switched.status.success(),
        "nix-env --switch-generation must succeed"
    );
    assert_eq!(
        fs::read_link(&profile).unwrap(),
        Path::new("profile-1-link")
    );

    let mut confirmation_called = false;
    let stale = nix_generations::local_test_support::apply(
        &mut runner,
        &config,
        CleanupProfile::Normal,
        &plan,
        &fixture.uri,
        &fixture.root,
        |_| {
            confirmation_called = true;
            Ok(true)
        },
    );
    assert_eq!(stale.unwrap_err(), GenerationError::Stale);
    assert!(
        !confirmation_called,
        "stale plan must not reach confirmation or mutation"
    );

    let native_delete = nix_env(&fixture.uri, &profile, &["--delete-generations", "1"]).unwrap();
    assert!(
        !native_delete.status.success(),
        "native lock/check must refuse the now-current ID"
    );
    assert!(fs::symlink_metadata(&selected_link).is_ok());
    assert_eq!(
        fs::read_link(&profile).unwrap(),
        Path::new("profile-1-link")
    );
    assert!(selected_target.exists());
}
