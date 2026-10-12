use hule::{
    CleanupProfile,
    config::{Category, Config, ConfigError},
};
use std::path::Path;

const BASE: &str = r#"
schema_version = 2
default_profile = "normal"
"#;

fn parse_config(input: &str) -> Result<Config, ConfigError> {
    let complete = format!(
        "{input}\n[profiles.low]\ncategories = []\n[profiles.normal]\ncategories = []\n[profiles.aggressive]\ncategories = []\n"
    );
    Config::from_toml(&complete)
}

#[test]
fn policy_config_has_keep_paths_but_no_project_registry() {
    let config = parse_config(
        r#"
   schema_version = 2
   default_profile = "normal"
   additional_keep_paths = ["/work/bdk"]
   "#,
    )
    .expect("valid fixture");
    assert_eq!(config.default_profile(), CleanupProfile::Normal);
    assert_eq!(
        config.additional_keep_paths(),
        &[Path::new("/work/bdk").to_path_buf()]
    );
}

#[test]
fn rejects_inline_projects_after_registry_extraction() {
    let input = format!("{BASE}\n[projects.demo]\npath = \"/work/demo\"\nmode = \"persistent\"\n");
    assert_eq!(parse_config(&input).unwrap_err(), ConfigError::InvalidToml);
}

#[test]
fn omitted_collections_are_empty_and_profile_categories_are_typed() {
    let config = parse_config(BASE).expect("valid minimal fixture");
    assert!(config.additional_keep_paths().is_empty());
    for profile in [
        CleanupProfile::Low,
        CleanupProfile::Normal,
        CleanupProfile::Aggressive,
    ] {
        assert!(config.categories(profile).is_empty());
    }
}

#[test]
fn rejects_unsupported_schema_version() {
    let input = BASE.replace("schema_version = 2", "schema_version = 3");
    assert!(matches!(
        parse_config(&input),
        Err(ConfigError::UnsupportedSchemaVersion)
    ));
}

#[test]
fn profile_categories_are_required_and_unknown_categories_fail_closed() {
    let valid = r#"
schema_version = 2
default_profile = "normal"
[profiles.low]
categories = []
[profiles.normal]
categories = ["rust_target"]
[profiles.aggressive]
categories = []
"#;
    let config = Config::from_toml(valid).expect("valid profiles");
    assert_eq!(
        config.categories(CleanupProfile::Normal),
        &[Category::RustTarget]
    );

    let missing = valid.replace("[profiles.aggressive]\ncategories = []", "");
    assert_eq!(
        Config::from_toml(&missing).unwrap_err(),
        ConfigError::InvalidToml
    );

    let invalid = valid.replace("rust_target", "unknown_category");
    assert_eq!(
        Config::from_toml(&invalid).unwrap_err(),
        ConfigError::InvalidToml
    );

    let invalid_profile = valid.replace("[profiles.low]", "[profiles.fast]");
    assert_eq!(
        Config::from_toml(&invalid_profile).unwrap_err(),
        ConfigError::InvalidToml
    );
}

#[test]
fn nix_store_gc_is_independently_opt_in_per_profile() {
    let config = Config::from_toml(
        r#"schema_version = 2
default_profile = "normal"
[profiles.low]
categories = []
[profiles.normal]
categories = ["nix_store_gc"]
[profiles.aggressive]
categories = ["rust_target"]
"#,
    )
    .expect("valid Nix opt-in policy");
    assert!(
        !config
            .categories(CleanupProfile::Low)
            .contains(&Category::NixStoreGc)
    );
    assert!(
        config
            .categories(CleanupProfile::Normal)
            .contains(&Category::NixStoreGc)
    );
    assert!(
        !config
            .categories(CleanupProfile::Aggressive)
            .contains(&Category::NixStoreGc)
    );
}

#[test]
fn rejects_duplicate_categories_within_each_profile() {
    let valid = r#"
schema_version = 2
default_profile = "normal"
[profiles.low]
categories = ["rust_target"]
[profiles.normal]
categories = ["rust_target"]
[profiles.aggressive]
categories = ["rust_target"]
"#;

    for profile in ["low", "normal", "aggressive"] {
        let duplicate = valid.replace(
            &format!("[profiles.{profile}]\ncategories = [\"rust_target\"]"),
            &format!("[profiles.{profile}]\ncategories = [\"rust_target\", \"rust_target\"]"),
        );
        assert_eq!(
            Config::from_toml(&duplicate).unwrap_err(),
            ConfigError::InvalidToml,
            "profile: {profile}"
        );
    }
}

#[test]
fn rejects_invalid_configuration() {
    let cases = [
        (
            "unknown root field",
            "unexpected = true",
            ConfigError::InvalidToml,
        ),
        (
            "relative keep path",
            r#"additional_keep_paths = ["../important"]"#,
            ConfigError::InvalidKeepPath,
        ),
        (
            "unsupported glob",
            r#"additional_keep_paths = ["/work/*"]"#,
            ConfigError::InvalidKeepPath,
        ),
    ];

    for (name, body, expected) in cases {
        let input = format!("{BASE}\n{body}");
        let error = parse_config(&input).expect_err(name);

        assert_eq!(error, expected, "case: {name}");
    }
}

#[test]
fn parsing_errors_do_not_echo_input_values() {
    let input = BASE.replace("\"normal\"", "\"do-not-echo-this-value\"");
    let error = parse_config(&input).expect_err("unknown profile");

    assert_eq!(error, ConfigError::InvalidToml);
    assert!(!error.to_string().contains("do-not-echo-this-value"));
    assert!(!format!("{error:?}").contains("do-not-echo-this-value"));
}

#[test]
fn detects_overlaps_with_additional_keep_paths() {
    let config = parse_config(
        r#"
   schema_version = 2
   default_profile = "normal"
   additional_keep_paths = ["/work/demo/target/keep"]
   "#,
    )
    .expect("valid fixture");

    let cases = [
        ("/work/demo/target/keep", true),
        ("/work/demo/target/keep/file", true),
        ("/work/demo/target", true),
        ("/work/demo", true),
        ("/work/demo/target/tmp", false),
        ("/work/demo/target/keep-copy", false),
        ("/work/other/target", false),
    ];

    for (path, expected) in cases {
        assert_eq!(
            config.overlaps_keep_path(Path::new(path)),
            expected,
            "path: {path} expected: {expected}"
        )
    }
}

#[test]
fn no_keep_path_means_no_configurated_overla() {
    let config = parse_config(BASE).expect("valid fixture");
    assert!(!config.overlaps_keep_path(Path::new("/work/demo/target")));
}

#[test]
fn checks_all_keep_paths_with_spaces_and_normalized_components() {
    let config = parse_config(
        r#"
   schema_version = 2
   default_profile = "normal"
   additional_keep_paths = [
       "/work/first/target/keep",
       "/work/demo project/./target//keep/",
   ]
   "#,
    )
    .expect("valid fixture");

    let cases = [
        ("/work/first/target/keep/file", true),
        ("/work/demo project/target/keep/file", true),
        ("/work/demo project/target", true),
        ("/work/demo project/target/keep-copy", false),
        ("/work/other/target", false),
    ];

    for (path, expected) in cases {
        assert_eq!(
            config.overlaps_keep_path(Path::new(path)),
            expected,
            "candidate: {path}"
        );
    }
}

#[test]
fn configurated_keep_path_overrides_every_cleanup_profile() {
    use hule::{RetentionDecision, decide_retention};

    let config = parse_config(
        r#"
   schema_version = 2
   default_profile = "normal"
   additional_keep_paths = ["/work/demo/target/keep"]
   "#,
    )
    .expect("valid fixture");

    let protected = config.overlaps_keep_path(Path::new("/work/demo/target"));

    for profile in [
        CleanupProfile::Low,
        CleanupProfile::Normal,
        CleanupProfile::Aggressive,
    ] {
        assert_eq!(
            decide_retention(profile, Some(hule::ProjectMode::Persistent), protected),
            RetentionDecision::KeepProtected,
            "profile: {profile:?} protected: {protected}"
        );
    }
}
