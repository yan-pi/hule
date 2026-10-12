use hule::{
    CleanupProfile,
    action_plan::{NixStoreActionPlan, StoreIdentity, read_plan, write_new},
    config::Category,
};
use std::{fs, path::PathBuf};

fn plan(root: &std::path::Path) -> NixStoreActionPlan {
    let store = root.join("store");
    fs::create_dir_all(&store).unwrap();
    let object = store.join("abc-object");
    NixStoreActionPlan {
        format: "hule-nix-store-action".into(),
        schema_version: 1,
        profile: CleanupProfile::Normal,
        category: Category::NixStoreGc,
        inputs: vec![object.clone()],
        closure: vec![object],
        identity: StoreIdentity {
            endpoint: "daemon".into(),
            version: "nix-store (Nix) 2".into(),
            store_dir: store,
        },
    }
}
#[test]
fn strict_plan_roundtrips_and_file_is_create_new() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("plan.json");
    let p = plan(d.path());
    write_new(&path, &p).unwrap();
    assert_eq!(read_plan(&path).unwrap(), p);
    assert!(write_new(&path, &p).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
#[test]
fn rejects_unknown_fields_tampering_and_mixed_paths() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("plan.json");
    let mut local_plan = plan(d.path());
    local_plan.identity.endpoint = format!("local?root={}", d.path().display());
    assert!(local_plan.validate().is_err());
    assert!(write_new(&path, &local_plan).is_err());
    let mut value = serde_json::to_value(plan(d.path())).unwrap();
    value["args"] = serde_json::json!(["--ignore-liveness"]);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(read_plan(&path).is_err());
    let mut p = plan(d.path());
    p.closure.push(PathBuf::from("/elsewhere/item"));
    assert!(p.validate().is_err());
}
#[test]
fn rejects_output_in_store_and_accepts_outside_including_symlink_parent() {
    let d = tempfile::tempdir().unwrap();
    let p = plan(d.path());
    let inside = p.identity.store_dir.join("plan.json");
    assert!(write_new(&inside, &p).is_err());
    let outside = d.path().join("plan.json");
    write_new(&outside, &p).unwrap();
    assert!(write_new(&d.path().join("missing/plan.json"), &p).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let alias = d.path().join("store-alias");
        symlink(&p.identity.store_dir, &alias).unwrap();
        assert!(write_new(&alias.join("aliased-plan.json"), &p).is_err());
    }
}

#[test]
fn rejects_nested_and_duplicate_candidates() {
    let d = tempfile::tempdir().unwrap();
    let mut p = plan(d.path());
    p.inputs[0].push("bin/tool");
    assert!(p.validate().is_err());
    let mut p = plan(d.path());
    p.closure.push(p.closure[0].clone());
    assert!(p.validate().is_err());
}
