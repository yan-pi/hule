use hule::{
    CleanupProfile,
    action_plan::{
        ActionPlan, ActivationRootSnapshot, ActivationRootState, FileIdentity, GenerationIdentity,
        GenerationSnapshot, NixGenerationActionPlan, read_action_plan, write_generation_new,
    },
    config::{Category, NixProfileKind},
};
use std::{fs, path::Path};

fn identity() -> FileIdentity {
    FileIdentity {
        device: 1,
        inode: 2,
        uid: 501,
        gid: 20,
        mode: 0o755,
    }
}

fn plan(root: &Path) -> NixGenerationActionPlan {
    let profile_dir = root.join(".local/state/nix/profiles");
    let store = root.join("store");
    fs::create_dir_all(&profile_dir).unwrap();
    fs::create_dir_all(&store).unwrap();
    let profile_path = profile_dir.join("home-manager");
    let targets = ["one", "two", "three"].map(|name| store.join(format!("{}-{name}", name.len())));
    let history = (1..=3)
        .map(|id| {
            let target = targets[(id - 1) as usize].clone();
            GenerationSnapshot {
                id,
                link: profile_dir.join(format!("home-manager-{id}-link")),
                link_identity: identity(),
                raw_target: target.clone(),
                target: target.clone(),
                target_identity: identity(),
            }
        })
        .collect();
    NixGenerationActionPlan {
        format: "hule-nix-generation-action".into(),
        schema_version: 1,
        profile: CleanupProfile::Normal,
        category: Category::NixHomeManagerGenerations,
        nix_profile: "hm".into(),
        kind: NixProfileKind::HomeManager,
        profile_path: profile_path.clone(),
        identity: GenerationIdentity {
            endpoint: "daemon".into(),
            store_version: "nix-store (Nix) 2.34.6".into(),
            nix_env_version: "nix-env (Nix) 2.34.6".into(),
            store_dir: store,
        },
        uid: 501,
        parent_identity: identity(),
        current_link_identity: identity(),
        current_link_target: "home-manager-3-link".into(),
        history,
        current_generation: 3,
        selected_ids: vec![1],
        activation_roots: vec![ActivationRootSnapshot {
            path: root.join(".local/share/home-manager/gcroots/current-home"),
            state: ActivationRootState::Present {
                link_identity: identity(),
                raw_target: targets[2].clone(),
                target: targets[2].clone(),
                target_identity: identity(),
            },
        }],
    }
}

#[test]
fn generation_plan_is_strict_and_roundtrips_separately_from_m1() {
    let fixture = tempfile::tempdir().unwrap();
    let p = plan(fixture.path());
    let output = fixture.path().join("generation-plan.json");
    write_generation_new(&output, &p).unwrap();
    assert_eq!(
        read_action_plan(&output).unwrap(),
        ActionPlan::NixGenerations(p)
    );
    let bytes = fs::read(&output).unwrap();
    assert!(bytes.len() <= 256 * 1024);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(output).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn generation_plan_rejects_forged_or_ambiguous_selection() {
    let fixture = tempfile::tempdir().unwrap();
    let mut p = plan(fixture.path());
    p.selected_ids = vec![3];
    assert!(
        p.validate().is_err(),
        "current generation cannot be selected"
    );

    let mut p = plan(fixture.path());
    p.history[1].target = p.history[0].target.clone();
    assert!(p.validate().is_err(), "duplicate targets are ambiguous");

    let mut p = plan(fixture.path());
    p.identity.endpoint = format!("local?store={}", fixture.path().display());
    assert!(
        p.validate().is_err(),
        "plans cannot assert local endpoint authority"
    );
}

#[test]
fn generation_plan_output_cannot_overlap_profile_or_activation_roots() {
    let fixture = tempfile::tempdir().unwrap();
    let p = plan(fixture.path());
    assert!(write_generation_new(&p.profile_path.join("plan.json"), &p).is_err());
    let root = &p.activation_roots[0].path;
    fs::create_dir_all(root.parent().unwrap()).unwrap();
    assert!(write_generation_new(&root.join("plan.json"), &p).is_err());
}

#[test]
fn action_plan_dispatch_never_falls_back_between_formats() {
    let fixture = tempfile::tempdir().unwrap();
    let p = plan(fixture.path());
    let output = fixture.path().join("plan.json");
    write_generation_new(&output, &p).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    value["format"] = serde_json::json!("unknown-provider");
    fs::write(&output, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(read_action_plan(&output).is_err());
}
