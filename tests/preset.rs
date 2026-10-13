use hule::{
    CleanupProfile,
    config::{Category, Config},
    preset::Preset,
};

#[test]
fn presets_materialize_complete_explicit_configurations() {
    for (preset, enabled) in [(Preset::Minimal, false), (Preset::Developer, true)] {
        let toml = preset.to_toml();
        let config = Config::from_toml(&toml).expect("preset emits valid complete config");
        for profile in [
            CleanupProfile::Low,
            CleanupProfile::Normal,
            CleanupProfile::Aggressive,
        ] {
            assert_eq!(
                config.categories(profile).contains(&Category::RustTarget),
                enabled
            );
            assert!(!config.categories(profile).contains(&Category::NixStoreGc));
            assert!(!config.categories(profile).contains(&Category::DockerPurge));
        }
        assert!(toml.contains("schema_version = 2"));
        assert!(!toml.contains("projects"));
        assert!(!toml.contains("docker_purge"));
        assert!(toml.contains("additional_keep_paths = []"));
    }
}
