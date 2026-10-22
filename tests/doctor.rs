use hule::{
    CleanupProfile,
    config::{Category, Config},
    doctor::{DoctorReport, DoctorStatus},
};

const CONFIG: &str = r#"
schema_version = 2
default_profile = "normal"
additional_keep_paths = ["/private/keep-path-secret"]

[profiles.low]
categories = ["rust_target"]
[profiles.normal]
categories = ["nix_store_gc"]
[profiles.aggressive]
categories = ["nix_user_generations", "docker_purge"]

[nix_profiles.work]
kind = "user"
path = "/Users/example/.local/state/nix/profiles/profile"
"#;

#[test]
fn reports_policy_and_static_capability_limits_without_exposing_paths() {
    let config = Config::from_toml(CONFIG).unwrap();
    let report = DoctorReport::from_config(&config);

    assert_eq!(report.configuration, DoctorStatus::Available);
    assert_eq!(report.default_profile, Some(CleanupProfile::Normal));
    assert_eq!(report.keep_path_count, Some(1));
    assert_eq!(
        report.nix_profile_names.as_ref().unwrap(),
        &vec!["work".to_string()]
    );
    let profiles = report.profiles.as_ref().unwrap();
    assert_eq!(profiles.len(), 3);
    assert_eq!(profiles[0].name, CleanupProfile::Low);
    assert_eq!(profiles[0].categories, [Category::RustTarget]);

    let rust_preview = report.capability("rust_target_preview").unwrap();
    assert_eq!(rust_preview.status, DoctorStatus::Available);
    assert_eq!(rust_preview.configured, Some(true));

    let rust_cleanup = report.capability("rust_target_cleanup").unwrap();
    assert_eq!(rust_cleanup.status, DoctorStatus::Unsupported);

    let nix_generations = report.capability("nix_generations").unwrap();
    assert_eq!(nix_generations.status, DoctorStatus::Unchecked);
    assert_eq!(nix_generations.configured, Some(true));
    assert!(nix_generations.summary.contains("2.34.6"));

    let nix_store = report.capability("nix_store_selected_objects").unwrap();
    assert_eq!(nix_store.status, DoctorStatus::Unchecked);
    assert!(
        nix_store
            .summary
            .contains("Nix host/store scope is not verified")
    );

    let docker = report.capability("docker_system_prune").unwrap();
    assert_eq!(docker.status, DoctorStatus::Unchecked);
    assert_eq!(docker.configured, Some(true));
    assert!(docker.summary.contains("mock-only"));
    assert!(docker.summary.contains("no resource-level keep exclusions"));

    assert_eq!(
        report.capability("orbstack_reset").unwrap().status,
        DoctorStatus::Unsupported
    );
    assert_eq!(
        report.capability("macos_cache_cleanup").unwrap().status,
        DoctorStatus::Unsupported
    );

    let json = report.to_json().unwrap();
    assert!(json.contains("\"status\": \"unchecked\""));
    assert!(json.contains("\"nix_profile_names\": ["));
    assert!(json.contains("\"work\""));
    assert!(!json.contains("/private/keep-path-secret"));
    assert!(!json.contains("/Users/example"));

    let human = report.to_human();
    assert!(human.contains("configuration: available"));
    assert!(human.contains("nix_generations [unchecked; configured: yes]"));
    assert!(human.contains("no resource-level keep exclusions"));
    assert!(!human.contains("/private/keep-path-secret"));
    assert!(!human.contains("/Users/example"));
}

#[test]
fn invalid_configuration_is_reported_as_unsafe_without_reflecting_input() {
    let invalid = "schema_version = 99\nsecret = \"do-not-echo\"\n";
    assert!(Config::from_toml(invalid).is_err());

    let report = DoctorReport::invalid_configuration();
    assert_eq!(report.configuration, DoctorStatus::Unsafe);
    assert_eq!(report.default_profile, None);
    assert!(report.profiles.is_none());
    assert!(report.keep_path_count.is_none());
    assert_eq!(
        report.capability("docker_system_prune").unwrap().configured,
        None
    );

    let json = report.to_json().unwrap();
    assert!(json.contains("\"configuration\": \"unsafe\""));
    assert!(!json.contains("do-not-echo"));
    let human = report.to_human();
    assert!(human.contains("configuration: unsafe"));
    assert!(!human.contains("do-not-echo"));
}
