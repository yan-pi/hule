use hule::state;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(config_contents: impl FnOnce(&Path) -> String) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("hule-cli-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("create isolated fixture");
        let config = config_contents(&path);
        fs::write(path.join("config.toml"), config).expect("write config fixture");
        fs::write(
            path.join("projects.toml"),
            format!(
                "version = 1\n[projects.persist]\npath = \"{}/project\"\nmode = \"persistent\"\n",
                path.display()
            ),
        )
        .expect("write isolated state fixture");
        Self(path)
    }

    fn config(&self) -> PathBuf {
        self.0.join("config.toml")
    }

    fn root(&self) -> &Path {
        &self.0
    }

    fn state(&self) -> PathBuf {
        self.0.join("projects.toml")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove isolated CLI fixture");
    }
}

fn config_text(_root: &Path) -> String {
    "schema_version = 2\ndefault_profile = \"normal\"\n\n[profiles.low]\ncategories = [\"rust_target\"]\n[profiles.normal]\ncategories = [\"rust_target\"]\n[profiles.aggressive]\ncategories = [\"rust_target\"]\n".to_string()
}

fn invoke(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hule"))
        .args(args)
        .output()
        .expect("run hule")
}

fn invoke_without_home(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hule"));
    command
        .args(args)
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME");
    command
        .output()
        .expect("run hule without HOME or XDG_STATE_HOME")
}

fn preview(fixture: &Fixture, args: &[&str]) -> Output {
    let config_path = fixture.config();
    let mut command = vec!["preview", "rust-target", "--config"];
    command.push(config_path.to_str().expect("fixture path is UTF-8"));
    command.extend_from_slice(args);
    command.push("--state");
    let state_path = fixture.state();
    command.push(state_path.to_str().expect("fixture state path is UTF-8"));
    invoke(&command)
}

#[test]
fn explicit_state_paths_work_without_home_or_xdg_state_home() {
    let fixture = Fixture::new(config_text);
    let state = fixture.state().to_str().unwrap().to_owned();
    let project = fixture.root().join("project");
    fs::create_dir(&project).unwrap();
    let mut registry = state::ProjectRegistry::default();
    registry
        .register(
            "persist".into(),
            project.clone(),
            hule::ProjectMode::Persistent,
        )
        .unwrap();
    state::save(&fixture.state(), &registry).unwrap();

    let listed = invoke_without_home(&["project", "list", "--state", &state]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(String::from_utf8_lossy(&listed.stdout).contains("persist"));

    let config = fixture.config().to_str().unwrap().to_owned();
    let preview = invoke_without_home(&[
        "preview",
        "rust-target",
        "--config",
        &config,
        "--project",
        "persist",
        "--state",
        &state,
    ]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(String::from_utf8_lossy(&preview.stdout).contains("target"));
}

#[test]
fn preview_all_reports_registered_projects_in_name_order_without_scanning_or_deleting() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let mut registry = state::ProjectRegistry::default();
    for (name, subdir) in [("zeta", "z-project"), ("alpha", "a-project")] {
        let project = root.join(subdir);
        fs::create_dir_all(project.join("target")).unwrap();
        if name == "alpha" {
            fs::write(
                project.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .unwrap();
            fs::write(
                project.join("Cargo.lock"),
                format!("version = 4\n\n[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            fs::create_dir(project.join("src")).unwrap();
            fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
        }
        fs::write(project.join("target/keep-me"), "fixture contents").unwrap();
        registry
            .register(
                name.into(),
                fs::canonicalize(project).unwrap(),
                hule::ProjectMode::Ephemeral,
            )
            .unwrap();
    }
    let unregistered = root.join("unregistered");
    fs::create_dir_all(unregistered.join("target")).unwrap();
    fs::write(
        unregistered.join("Cargo.toml"),
        "invalid and must not be scanned",
    )
    .unwrap();
    let config_path = root.join("config.toml");
    fs::write(&config_path, config_text(root)).unwrap();
    let state_path = root.join("projects.toml");
    state::save(&state_path, &registry).unwrap();

    let output = invoke(&[
        "preview",
        "rust-target",
        "--config",
        config_path.to_str().unwrap(),
        "--all",
        "--state",
        state_path.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.find("project: \"alpha\"").unwrap() < report.find("project: \"zeta\"").unwrap());
    assert_eq!(
        report.matches("discovery: discovered").count(),
        1,
        "{report}"
    );
    assert_eq!(
        report.matches("discovery: unsupported/skipped").count(),
        1,
        "{report}"
    );
    assert_eq!(
        report.matches("decision: retention-eligible").count(),
        2,
        "{report}"
    );
    assert_eq!(report.matches("--- project ---").count(), 2, "{report}");
    assert!(!report.contains("unregistered"), "{report}");
    for subdir in ["a-project", "z-project"] {
        assert_eq!(
            fs::read(root.join(subdir).join("target/keep-me")).unwrap(),
            b"fixture contents"
        );
    }
}

#[test]
fn preview_all_empty_registry_succeeds_without_cargo_or_filesystem_scan() {
    let fixture = tempfile::tempdir().unwrap();
    let config_path = fixture.path().join("config.toml");
    fs::write(&config_path, config_text(fixture.path())).unwrap();
    let state_path = fixture.path().join("empty.toml");
    state::save(&state_path, &state::ProjectRegistry::default()).unwrap();
    let output = invoke(&[
        "preview",
        "rust-target",
        "--config",
        config_path.to_str().unwrap(),
        "--all",
        "--state",
        state_path.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("no registered projects"));
}

#[test]
fn preview_requires_exactly_one_project_selector() {
    let fixture = Fixture::new(config_text);
    let config = fixture.config().to_str().unwrap().to_owned();
    let state = fixture.state().to_str().unwrap().to_owned();
    for selectors in [vec![], vec!["--project", "persist", "--all"]] {
        let mut args = vec!["preview", "rust-target", "--config", &config];
        args.extend(selectors);
        args.extend(["--state", &state]);
        let output = invoke(&args);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("exactly one of --project or --all")
        );
    }
}

#[test]
fn json_preview_is_an_informational_plan_without_human_delimiters() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let project = root.join("project");
    fs::create_dir_all(project.join("target")).unwrap();
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"json-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        project.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"json-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir(project.join("src")).unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    let mut registry = state::ProjectRegistry::default();
    registry
        .register(
            "persist".into(),
            fs::canonicalize(&project).unwrap(),
            hule::ProjectMode::Persistent,
        )
        .unwrap();
    let state_path = root.join("projects.toml");
    state::save(&state_path, &registry).unwrap();
    let config_path = root.join("config.toml");
    fs::write(&config_path, config_text(root)).unwrap();

    let output = invoke(&[
        "preview",
        "rust-target",
        "--config",
        config_path.to_str().unwrap(),
        "--project",
        "persist",
        "--state",
        state_path.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["profile"], "normal");
    assert_eq!(value["category"], "rust_target");
    assert_eq!(value["authorization"], "not_authorized");
    assert_eq!(value["entries"][0]["project_name"], "persist");
    assert_eq!(value["entries"][0]["discovery"]["outcome"], "discovered");
    assert!(value["entries"][0]["discovery"]["path"].is_string());
    assert_eq!(value["entries"][0]["decision"], "keep_persistent");
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(!report.contains("--- project ---"));
}

#[test]
fn json_preview_all_empty_registry_returns_an_empty_plan() {
    let fixture = tempfile::tempdir().unwrap();
    let config_path = fixture.path().join("config.toml");
    fs::write(&config_path, config_text(fixture.path())).unwrap();
    let state_path = fixture.path().join("projects.toml");
    state::save(&state_path, &state::ProjectRegistry::default()).unwrap();

    let output = invoke(&[
        "preview",
        "rust-target",
        "--config",
        config_path.to_str().unwrap(),
        "--all",
        "--state",
        state_path.to_str().unwrap(),
        "--json",
    ]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["entries"], serde_json::json!([]));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("no registered projects"));
}

#[test]
fn real_cargo_metadata_preview_discovers_fixture_target() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let project = root.join("project");
    fs::create_dir_all(project.join("target")).unwrap();
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"isolated-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        project.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"isolated-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir(project.join("src")).unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    let mut registry = state::ProjectRegistry::default();
    let canonical_project = fs::canonicalize(&project).unwrap();
    registry
        .register(
            "persist".into(),
            canonical_project.clone(),
            hule::ProjectMode::Persistent,
        )
        .unwrap();
    let state_path = root.join("projects.toml");
    state::save(&state_path, &registry).unwrap();
    let config_path = root.join("config.toml");
    fs::write(&config_path, config_text(root)).unwrap();

    let cargo_home = root.join("cargo-home");
    fs::create_dir(&cargo_home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hule"));
    command
        .args([
            "preview",
            "rust-target",
            "--config",
            config_path.to_str().unwrap(),
            "--project",
            "persist",
            "--state",
            state_path.to_str().unwrap(),
        ])
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_HOME", &cargo_home);
    let output = command
        .output()
        .expect("run isolated Cargo metadata preview");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(report.contains("discovery: discovered"), "{report}");
    assert!(
        report.contains(
            &canonical_project
                .join("target")
                .to_string_lossy()
                .to_string()
        ),
        "{report}"
    );
    assert!(project.join("target").is_dir());
}

#[test]
fn project_register_list_unregister_and_reject_invalid_operations() {
    let fixture = Fixture::new(config_text);
    let project = fixture.root().join("project directory");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("untouched"), "contents").unwrap();
    let state = fixture.state().to_str().unwrap().to_owned();
    let project_path = project.to_str().unwrap().to_owned();
    let register = invoke(&[
        "project",
        "register",
        "--name",
        "alpha",
        "--path",
        &project_path,
        "--mode",
        "persistent",
        "--state",
        &state,
    ]);
    assert!(
        register.status.success(),
        "{}",
        String::from_utf8_lossy(&register.stderr)
    );
    assert_eq!(fs::read(project.join("untouched")).unwrap(), b"contents");
    let listed = invoke(&["project", "list", "--state", &state]);
    assert!(listed.status.success());
    let listing = String::from_utf8(listed.stdout).unwrap();
    assert!(listing.contains("alpha\t"));
    assert!(listing.contains("persistent"));
    let duplicate = invoke(&[
        "project",
        "register",
        "--name",
        "alpha",
        "--path",
        &project_path,
        "--mode",
        "ephemeral",
        "--state",
        &state,
    ]);
    assert!(!duplicate.status.success());
    assert!(!String::from_utf8_lossy(&duplicate.stderr).contains(&project_path));
    let child = project.join("child");
    fs::create_dir(&child).unwrap();
    let overlapping = invoke(&[
        "project",
        "register",
        "--name",
        "child",
        "--path",
        &child.to_string_lossy(),
        "--mode",
        "ephemeral",
        "--state",
        &state,
    ]);
    assert!(!overlapping.status.success());
    let missing_path = invoke(&[
        "project",
        "register",
        "--name",
        "bad",
        "--path",
        &fixture.root().join("absent").to_string_lossy(),
        "--mode",
        "ephemeral",
        "--state",
        &state,
    ]);
    assert!(!missing_path.status.success());
    let not_dir = invoke(&[
        "project",
        "register",
        "--name",
        "bad",
        "--path",
        &project.join("untouched").to_string_lossy(),
        "--mode",
        "ephemeral",
        "--state",
        &state,
    ]);
    assert!(!not_dir.status.success());
    let unknown = invoke(&[
        "project",
        "unregister",
        "--name",
        "unknown",
        "--state",
        &state,
    ]);
    assert!(!unknown.status.success());
    let removed = invoke(&[
        "project",
        "unregister",
        "--name",
        "alpha",
        "--state",
        &state,
    ]);
    assert!(removed.status.success());
    assert!(
        state::load(&fixture.state())
            .unwrap()
            .get("alpha")
            .is_none()
    );
}

#[test]
fn project_listing_escapes_control_characters_and_errors_do_not_echo_names_or_paths() {
    let fixture = Fixture::new(config_text);
    let project = fixture.root().join("project");
    fs::create_dir(&project).unwrap();
    let state = fixture.state().to_str().unwrap().to_owned();
    let path = project.to_str().unwrap().to_owned();
    let out = invoke(&[
        "project",
        "register",
        "--name",
        "bad\\nname",
        "--path",
        &path,
        "--mode",
        "ephemeral",
        "--state",
        &state,
    ]);
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("bad\\nname"));
}

#[test]
fn help_and_version_are_supported() {
    let help = invoke(&["--help"]);
    assert!(help.status.success());
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("doctor --config PATH [--json]"));
    assert!(help_text.contains("inspect nix-store --config PATH --store-path PATH..."));
    assert!(help_text.contains("inspect nix-generations --config PATH --nix-profile NAME"));
    assert!(help_text.contains("inventory rust-target --config PATH --project NAME --target PATH"));
    assert!(help_text.contains("inventory xcode-derived-data --path PATH"));
    assert!(help_text.contains("Nix inspection queries only explicitly selected store objects"));
    assert!(help_text.contains("preview rust-target"));
    assert!(help_text.contains("plan nix-generations"));
    assert!(help_text.contains("plan docker --docker-purge"));
    assert!(help_text.contains("--docker-cli /ABS/PATH/docker"));
    assert!(help_text.contains("--nix-profile NAME --generation ID"));
    assert!(help_text.contains(
        "Cargo metadata (`cargo metadata --no-deps --offline --locked`) may be invoked only when each project is registered"
    ));
    assert!(help_text.contains("(--project NAME | --all)"));
    assert!(help_text.contains("[--json]"));
    assert!(help_text.contains(
        "rust_target is enabled for the selected profile, and the anticipated target is not already protected"
    ));
    assert!(help_text.contains("Cargo documentation does not formally guarantee absence of all side effects or code execution"));

    let version = invoke(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout)
            .expect("stdout is UTF-8")
            .trim(),
        format!("hule {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn doctor_reports_configuration_without_invoking_provider_commands_or_echoing_paths() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().unwrap();
    let config = fixture.path().join("config.toml");
    fs::write(
        &config,
        r#"schema_version = 2
 default_profile = "aggressive"
 additional_keep_paths = ["/private/keep-secret"]
 [profiles.low]
 categories = []
 [profiles.normal]
 categories = []
 [profiles.aggressive]
 categories = ["docker_purge"]
 "#,
    )
    .unwrap();

    let provider_bin = fixture.path().join("bin");
    fs::create_dir(&provider_bin).unwrap();
    let marker = fixture.path().join("provider-was-invoked");
    for provider in ["cargo", "nix-store", "nix-env", "docker", "orb", "orbctl"] {
        let executable = provider_bin.join(provider);
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf invoked > '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let json = Command::new(env!("CARGO_BIN_EXE_hule"))
        .args(["doctor", "--config", config.to_str().unwrap(), "--json"])
        .env("PATH", &provider_bin)
        .output()
        .unwrap();
    assert!(json.status.success());
    assert!(json.stderr.is_empty());
    let json_text = String::from_utf8(json.stdout).unwrap();
    assert!(json_text.contains("\"docker_system_prune\""));
    assert!(!json_text.contains("/private/keep-secret"));
    assert!(!json_text.contains(config.to_str().unwrap()));
    assert!(!marker.exists());

    let human = invoke(&["doctor", "--config", config.to_str().unwrap()]);
    assert!(human.status.success());
    let human_text = String::from_utf8(human.stdout).unwrap();
    assert!(human_text.contains("configuration: available"));
    assert!(!human_text.contains("/private/keep-secret"));
    assert!(!marker.exists());
}

#[test]
fn doctor_invalid_configuration_reports_unsafe_without_echoing_contents_or_path() {
    let fixture = tempfile::tempdir().unwrap();
    let config = fixture.path().join("secret-config.toml");
    fs::write(&config, "schema_version = 99\nsecret = \"do-not-echo\"\n").unwrap();
    let output = invoke(&["doctor", "--config", config.to_str().unwrap(), "--json"]);
    assert!(output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("\"configuration\": \"unsafe\""));
    assert!(!json.contains("do-not-echo"));
    assert!(!json.contains(config.to_str().unwrap()));
}

#[test]
fn nix_generation_inspection_parser_rejects_duplicate_ids_before_loading_config() {
    let duplicate = invoke(&[
        "inspect",
        "nix-generations",
        "--config",
        "/missing/config.toml",
        "--nix-profile",
        "work",
        "--generation",
        "1",
        "--generation",
        "1",
    ]);
    assert!(!duplicate.status.success());
    assert_eq!(duplicate.stderr, b"hule: duplicate generation id\n");

    let noncanonical = invoke(&[
        "inspect",
        "nix-generations",
        "--config",
        "/missing/config.toml",
        "--nix-profile",
        "work",
        "--generation",
        "01",
    ]);
    assert!(!noncanonical.status.success());
    assert_eq!(noncanonical.stderr, b"hule: invalid generation id\n");
}

#[test]
fn nix_store_inspection_reports_only_explicit_fixture_selection_without_deleting() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().unwrap();
    let store = fixture.path().join("nix/store");
    fs::create_dir_all(&store).unwrap();
    let store = fs::canonicalize(store).unwrap();
    let object = store.join("abc-selected-object");
    fs::write(&object, b"fixture object").unwrap();
    let config = fixture.path().join("config.toml");
    fs::write(
        &config,
        "schema_version=2\ndefault_profile='normal'\n[profiles.low]\ncategories=[]\n[profiles.normal]\ncategories=['nix_store_gc']\n[profiles.aggressive]\ncategories=[]\n",
    )
    .unwrap();
    let provider_bin = fixture.path().join("bin");
    fs::create_dir(&provider_bin).unwrap();
    let deletion_marker = fixture.path().join("delete-called");
    let nix = provider_bin.join("nix");
    fs::write(
        &nix,
        r##"#!/bin/sh
printf '%s\n' "$HULE_TEST_STORE"
"##,
    )
    .unwrap();
    fs::set_permissions(&nix, fs::Permissions::from_mode(0o700)).unwrap();
    let nix_store = provider_bin.join("nix-store");
    fs::write(
        &nix_store,
        format!(
            r##"#!/bin/sh
for arg do
  case "$arg" in
    --version) printf 'nix-store fixture\n'; exit 0 ;;
    --query) printf '%s\n' "$HULE_TEST_OBJECT"; exit 0 ;;
    --delete) printf called > '{}'; exit 0 ;;
  esac
done
exit 73
"##,
            deletion_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&nix_store, fs::Permissions::from_mode(0o700)).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_hule"));
    command
        .args([
            "inspect",
            "nix-store",
            "--config",
            config.to_str().unwrap(),
            "--store-path",
            object.to_str().unwrap(),
            "--json",
        ])
        .env("PATH", &provider_bin)
        .env("HULE_TEST_STORE", &store)
        .env("HULE_TEST_OBJECT", &object);
    let json = command.output().unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    assert!(json.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["authorization"], "not_authorized");
    assert_eq!(report["selected_objects"][0], object.to_str().unwrap());
    assert_eq!(report["requisites"][0], object.to_str().unwrap());
    assert!(!deletion_marker.exists());

    let human = Command::new(env!("CARGO_BIN_EXE_hule"))
        .args([
            "inspect",
            "nix-store",
            "--config",
            config.to_str().unwrap(),
            "--store-path",
            object.to_str().unwrap(),
        ])
        .env("PATH", &provider_bin)
        .env("HULE_TEST_STORE", &store)
        .env("HULE_TEST_OBJECT", &object)
        .output()
        .unwrap();
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("not authorization"));
    assert!(text.contains(object.to_str().unwrap()));
    assert!(text.contains("metadata"));
    assert!(!deletion_marker.exists());
}

#[test]
fn xcode_inventory_cli_reports_fixture_without_paths_or_contents() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture
        .path()
        .join("Library/Developer/Xcode/DerivedData");
    let target = root.join("Private Project/Build/Products");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("secret-artifact.o"), b"private cache contents").unwrap();
    let root = fs::canonicalize(root).unwrap();

    let json = invoke(&[
        "inventory",
        "xcode-derived-data",
        "--path",
        root.to_str().unwrap(),
        "--json",
    ]);
    assert!(json.status.success(), "{}", String::from_utf8_lossy(&json.stderr));
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["state"], "complete");
    assert_eq!(value["authorization"], "not_authorized");
    assert_eq!(value["summary"]["file_entries"], 1);
    assert_eq!(value["summary"]["apparent_bytes"], 22);
    let json_text = String::from_utf8(json.stdout).unwrap();
    assert!(!json_text.contains(&root.to_string_lossy().to_string()));
    assert!(!json_text.contains("Private Project"));
    assert!(!json_text.contains("secret-artifact.o"));
    assert!(!json_text.contains("private cache contents"));

    let human = invoke(&[
        "inventory",
        "xcode-derived-data",
        "--path",
        root.to_str().unwrap(),
    ]);
    assert!(human.status.success());
    let human_text = String::from_utf8(human.stdout).unwrap();
    assert!(human_text.contains("not authorized"));
    assert!(human_text.contains("apparent bytes: 22"));
    assert!(human_text.contains("best-effort"));
    assert!(!human_text.contains(&root.to_string_lossy().to_string()));
    assert!(!human_text.contains("secret-artifact.o"));
}

#[test]
fn rust_target_inventory_cli_requires_registered_opted_in_unprotected_target() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project with spaces");
    let target = project.join("custom-build/target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("private-name"), b"not read").unwrap();
    let project = fs::canonicalize(project).unwrap();
    let target = fs::canonicalize(target).unwrap();
    let config = fixture.path().join("config.toml");
    fs::write(
        &config,
        r#"schema_version=2
 default_profile='normal'
 [profiles.low]
 categories=['rust_target']
 [profiles.normal]
 categories=['rust_target']
 [profiles.aggressive]
 categories=['rust_target']
 "#,
    )
    .unwrap();
    let state = fixture.path().join("projects.toml");
    fs::write(
        &state,
        format!(
            r#"version=1
 [projects.fixture]
 path='{}'
 mode='ephemeral'
 "#,
            project.display()
        ),
    )
    .unwrap();

    let provider_bin = fixture.path().join("bin");
    fs::create_dir(&provider_bin).unwrap();
    let marker = fixture.path().join("provider-invoked");
    for provider in ["cargo", "nix", "nix-store", "docker", "orb", "orbctl"] {
        let executable = provider_bin.join(provider);
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
printf invoked > '{}'
"#,
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_hule"));
    command
        .args([
            "inventory",
            "rust-target",
            "--config",
            config.to_str().unwrap(),
            "--project",
            "fixture",
            "--target",
            target.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "--profile",
            "aggressive",
            "--json",
        ])
        .env("PATH", &provider_bin);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "complete");
    assert_eq!(value["kind"], "rust_target");
    assert_eq!(value["authorization"], "not_authorized");
    assert_eq!(value["summary"]["file_entries"], 1);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains(&project.to_string_lossy().to_string()));
    assert!(!text.contains(&target.to_string_lossy().to_string()));
    assert!(!text.contains("private-name"));
    assert!(!marker.exists());

    let disabled = fixture.path().join("disabled.toml");
    fs::write(
        &disabled,
        r#"schema_version=2
 default_profile='normal'
 [profiles.low]
 categories=[]
 [profiles.normal]
 categories=[]
 [profiles.aggressive]
 categories=[]
 "#,
    )
    .unwrap();
    let refused = Command::new(env!("CARGO_BIN_EXE_hule"))
        .args([
            "inventory",
            "rust-target",
            "--config",
            disabled.to_str().unwrap(),
            "--project",
            "fixture",
            "--target",
            target.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ])
        .env("PATH", &provider_bin)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("rust_target is not enabled"));
    assert!(!marker.exists());
}

#[test]
fn docker_purge_requires_explicit_selection_and_apply_cannot_upgrade_scope() {
    let fixture = tempfile::tempdir().unwrap();
    let missing_selection = invoke(&[
        "plan",
        "docker",
        "--config",
        "/missing/config.toml",
        "--docker-endpoint",
        "unix:///tmp/not-a-docker-socket",
        "--output",
        "/tmp/docker-plan.json",
    ]);
    assert_eq!(
        missing_selection.stderr,
        b"hule: missing required flag: --docker-purge\n"
    );

    let duplicate_selection = invoke(&[
        "plan",
        "docker",
        "--docker-purge",
        "--docker-purge",
        "--config",
        "/missing/config.toml",
        "--docker-endpoint",
        "unix:///tmp/not-a-docker-socket",
        "--output",
        "/tmp/docker-plan.json",
    ]);
    assert_eq!(
        duplicate_selection.stderr,
        b"hule: duplicate argument: --docker-purge\n"
    );

    let apply_flag = invoke(&[
        "apply",
        "--plan",
        "/missing/plan.json",
        "--config",
        "/missing/config.toml",
        "--docker-purge",
        "yes",
    ]);
    assert_eq!(
        apply_flag.stderr,
        b"hule: invalid arguments; use --help for usage\n"
    );
    assert!(!fixture.path().join("docker-plan.json").exists());
}

#[test]
fn docker_plan_rejects_inherited_endpoint_context_or_config_overrides_before_execution() {
    let fixture = tempfile::tempdir().unwrap();
    let config = fixture.path().join("config.toml");
    fs::write(
        &config,
        "schema_version = 2\ndefault_profile = \"aggressive\"\n\n[profiles.low]\ncategories = []\n[profiles.normal]\ncategories = []\n[profiles.aggressive]\ncategories = [\"docker_purge\"]\n",
    )
    .unwrap();
    let output_path = fixture.path().join("plan.json");
    let socket = fixture.path().join("missing.sock");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hule"));
    command
        .args([
            "plan",
            "docker",
            "--docker-purge",
            "--config",
            config.to_str().unwrap(),
            "--docker-cli",
            "/usr/bin/docker",
            "--docker-endpoint",
            &format!("unix://{}", socket.display()),
            "--output",
            output_path.to_str().unwrap(),
            "--profile",
            "aggressive",
        ])
        .env("DOCKER_CONTEXT", "fixture-context");
    let result = command.output().unwrap();
    assert_eq!(
        result.stderr,
        b"hule: Docker purge refuses inherited Docker endpoint, context, API, TLS or config overrides\n"
    );
    assert!(!output_path.exists());
}

#[test]
fn generation_cli_rejects_noncanonical_ids_and_endpoint_injection_before_loading_config() {
    for id in ["0", "01", "+1", "-1", "all", "30d", "2147483648", "--store"] {
        let output = invoke(&[
            "plan",
            "nix-generations",
            "--config",
            "/definitely/not/a/real/config.toml",
            "--nix-profile",
            "hm",
            "--generation",
            id,
            "--output",
            "/tmp/hule-invalid-generation-plan.json",
        ]);
        assert!(!output.status.success(), "accepted id: {id}");
        assert_eq!(output.stderr, b"hule: invalid generation id\n", "id: {id}");
    }
}

#[test]
fn generation_cli_rejects_duplicate_and_excessive_ids() {
    let duplicate = invoke(&[
        "plan",
        "nix-generations",
        "--config",
        "/missing",
        "--nix-profile",
        "hm",
        "--generation",
        "1",
        "--generation",
        "1",
        "--output",
        "/tmp/plan.json",
    ]);
    assert_eq!(duplicate.stderr, b"hule: duplicate generation id\n");

    let mut args = vec![
        "plan".to_string(),
        "nix-generations".to_string(),
        "--config".to_string(),
        "/missing".to_string(),
        "--nix-profile".to_string(),
        "hm".to_string(),
    ];
    for id in 1..=33 {
        args.push("--generation".into());
        args.push(id.to_string());
    }
    args.extend(["--output".into(), "/tmp/plan.json".into()]);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let excessive = invoke(&refs);
    assert_eq!(excessive.stderr, b"hule: too many generations selected\n");
}

#[test]
fn config_preset_prints_toml_without_writing_a_file() {
    for name in ["minimal", "developer"] {
        let output = invoke(&["config", "preset", name]);
        assert!(output.status.success());
        let toml = String::from_utf8(output.stdout).expect("preset is UTF-8");
        assert!(toml.contains("schema_version = 2"));
        assert!(toml.contains("[profiles.aggressive]"));
        assert!(!toml.contains("[providers"));
    }
    let output = invoke(&["config", "preset", "unknown\\n\\u{1b}[31m"]);
    assert!(!output.status.success());
    assert!(!output.stderr.contains(&0x1b));
}

#[test]
fn uses_config_default_profile_and_accepts_override() {
    let fixture = Fixture::new(config_text);
    let default = preview(&fixture, &["--project", "persist"]);
    assert!(default.status.success());
    assert!(String::from_utf8_lossy(&default.stdout).contains("decision: keep-persistent\n"));

    let override_profile = preview(
        &fixture,
        &["--project", "persist", "--profile", "aggressive"],
    );
    assert!(override_profile.status.success());
    assert!(
        String::from_utf8_lossy(&override_profile.stdout)
            .contains("decision: retention-eligible\n")
    );
}

#[test]
fn missing_invalid_config_and_invalid_arguments_fail_on_stderr() {
    let fixture = Fixture::new(config_text);
    let missing = invoke(&[
        "preview",
        "rust-target",
        "--config",
        fixture.root().join("missing.toml").to_str().unwrap(),
        "--project",
        "persist",
    ]);
    assert!(!missing.status.success());
    assert!(!missing.stderr.is_empty());

    let invalid_fixture = Fixture::new(|_| "this is not valid TOML = =".to_string());
    let invalid = preview(&invalid_fixture, &["--project", "persist"]);
    assert!(!invalid.status.success());
    assert!(!invalid.stderr.is_empty());

    let invalid_args = invoke(&["preview", "rust-target", "--project", "persist"]);
    assert!(!invalid_args.status.success());
    assert!(!invalid_args.stderr.is_empty());

    let invalid_profile = preview(&fixture, &["--project", "persist", "--profile", "fast"]);
    assert!(!invalid_profile.status.success());
    assert!(!invalid_profile.stderr.is_empty());

    let duplicate_json = preview(&fixture, &["--project", "persist", "--json", "--json"]);
    assert!(!duplicate_json.status.success());
    assert_eq!(duplicate_json.stderr, b"hule: duplicate argument: --json\n");

    let conflicting_selection = preview(&fixture, &["--project", "persist", "--all", "--json"]);
    assert!(!conflicting_selection.status.success());
    assert_eq!(
        conflicting_selection.stderr,
        b"hule: exactly one of --project or --all is required\n"
    );
}

#[test]
fn unknown_argument_controls_are_not_written_to_stderr() {
    let fixture = Fixture::new(config_text);
    let output = preview(
        &fixture,
        &["--project", "persist", "bad\n\u{1b}[31m", "value"],
    );

    assert!(!output.status.success());
    assert_eq!(output.stderr, b"hule: unknown argument\n");
}

#[test]
fn invalid_profile_controls_are_not_written_to_stderr() {
    let fixture = Fixture::new(config_text);
    let output = preview(
        &fixture,
        &["--project", "persist", "--profile", "fast\n\u{1b}[31m"],
    );

    assert!(!output.status.success());
    assert_eq!(output.stderr, b"hule: invalid profile\n");
}

#[test]
fn unknown_projects_are_skipped_without_target_and_preview_does_not_mutate() {
    let fixture = Fixture::new(config_text);
    let output = preview(&fixture, &["--project", "not-registered"]);

    assert!(output.status.success());
    let report = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(report.contains("decision: skip-unregistered\n"));
    assert!(report.contains("target: -\n"));
    assert!(!report.contains("not-registered/target"));
    assert!(!fixture.root().join("project/target").exists());
    assert_eq!(fs::read_dir(fixture.root()).unwrap().count(), 2);
}
