use hule::{
    CleanupProfile, ProjectMode,
    config::{Config, ConfigError},
};
use std::path::Path;

const BASE: &str = r#"
schema_version = 1
default_profile = "normal"
"#;

#[test]
fn parses_projects_and_additional_keep_paths() {
    let config = Config::from_toml(
        r#"
   schema_version = 1
   default_profile = "normal"
   additional_keep_paths = ["/work/bdk"]

   [projects.bdk]
   path = "/work/bdk"
   mode = "persistent"

   [projects.experiment]
   path = "/work/experiment"
   mode = "ephemeral"
   "#,
    )
    .expect("valid fixture");

    assert_eq!(config.default_profile(), CleanupProfile::Normal);

    let bdk = config.projects().get("bdk").expect("registered project");
    assert_eq!(bdk.path(), Path::new("/work/bdk"));
    assert_eq!(bdk.mode(), ProjectMode::Persistent);

    let experiment = config
        .projects()
        .get("experiment")
        .expect("registered project");
    assert_eq!(experiment.mode(), ProjectMode::Ephemeral);
    assert_eq!(
        config.additional_keep_paths(),
        &[Path::new("/work/bdk").to_path_buf()]
    );

    assert!(config.project("unregistered").is_none());
}

#[test]
fn ommited_collections_are_empty() {
    let config = Config::from_toml(BASE).expect("valid minimal fixture");
    assert!(config.projects().is_empty());
    assert!(config.additional_keep_paths().is_empty());
}

#[test]
fn rejects_unsupported_schema_version() {
    let input = BASE.replace("schema_version = 1", "schema_version = 2");
    assert!(matches!(
        Config::from_toml(&input),
        Err(ConfigError::UnsupportedSchemaVersion)
    ));
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
            "unknown project field",
            r#"
   [projects.demo]
   path = "/work/demo"
   mode = "ephemeral"
   unexpected = true
   "#,
            ConfigError::InvalidToml,
        ),
        (
            "unknown project mode",
            r#"
   [projects.demo]
   path = "/work/demo"
   mode = "temporary"
   "#,
            ConfigError::InvalidToml,
        ),
        (
            "invalid project name",
            r#"
   [projects."bad name"]
   path = "/work/demo"
   mode = "ephemeral"
   "#,
            ConfigError::InvalidProjectName,
        ),
        (
            "relative project path",
            r#"
   [projects.demo]
   path = "relative/demo"
   mode = "ephemeral"
   "#,
            ConfigError::InvalidProjectPath,
        ),
        (
            "filesystem root",
            r#"
   [projects.demo]
   path = "/"
   mode = "ephemeral"
   "#,
            ConfigError::InvalidProjectPath,
        ),
        (
            "parent traversal",
            r#"
   [projects.demo]
   path = "/work/../other"
   mode = "ephemeral"
   "#,
            ConfigError::InvalidProjectPath,
        ),
        (
            "overlapping projects",
            r#"
   [projects.parent]
   path = "/work/demo"
   mode = "persistent"

   [projects.child]
   path = "/work/demo/child"
   mode = "ephemeral"
   "#,
            ConfigError::OverlappingProjectRoots,
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
        let error = Config::from_toml(&input).expect_err(name);

        assert_eq!(error, expected, "case: {name}");
    }
}

#[test]
fn parsing_errors_do_not_echo_input_values() {
    let input = BASE.replace("\"normal\"", "\"do-not-echo-this-value\"");
    let error = Config::from_toml(&input).expect_err("unknown profile");

    assert_eq!(error, ConfigError::InvalidToml);
    assert!(!error.to_string().contains("do-not-echo-this-value"));
    assert!(!format!("{error:?}").contains("do-not-echo-this-value"));
}

#[test]
fn detects_overlaps_with_additional_keep_paths() {
    let config = Config::from_toml(
        r#"
   schema_version = 1
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
    let config = Config::from_toml(BASE).expect("valid fixture");
    assert!(!config.overlaps_keep_path(Path::new("/work/demo/target")));
}

#[test]
fn checks_all_keep_paths_with_spaces_and_normalized_components() {
    let config = Config::from_toml(
        r#"
   schema_version = 1
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

    let config = Config::from_toml(
        r#"
   schema_version = 1
   default_profile = "normal"
   additional_keep_paths = ["/work/demo/target/keep"]

   [projects.demo]
   path = "/work/demo"
   mode = "persistent"
   "#,
    )
    .expect("valid fixture");

    let project = config.project("demo").expect("registered project");
    let protected = config.overlaps_keep_path(Path::new("/work/demo/target"));

    for profile in [
        CleanupProfile::Low,
        CleanupProfile::Normal,
        CleanupProfile::Aggressive,
    ] {
        assert_eq!(
            decide_retention(profile, Some(project.mode()), protected),
            RetentionDecision::KeepProtected,
            "profile: {profile:?} mode: {project:?} protected: {protected}"
        );
    }
}
