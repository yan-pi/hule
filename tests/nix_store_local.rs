//! Native Hule/Nix validation; every nix-store invocation is pinned to a disposable local URI.
use hule::{
    CleanupProfile,
    config::Config,
    nix_store::{self, CommandNixRunner, PathApplyStatus},
};
use std::{fs, path::Path, process::Command};

fn nix(uri: &str, args: &[&str]) -> Result<std::process::Output, String> {
    let mut command = Command::new("nix-store");
    command.arg("--store").arg(uri).args(args);
    command
        .output()
        .map_err(|_| "nix-store executable unavailable".to_owned())
}
#[test]
#[ignore = "requires nix-store and a supported isolated local?root URI; run explicitly for native acceptance"]
fn isolated_local_store_add_query_delete_preserves_sentinel() {
    let Ok(temp) = tempfile::tempdir() else {
        panic!("temp fixture unavailable")
    };
    let root = fs::canonicalize(temp.path()).expect("canonical temp root");
    let uri = format!("local?root={}", root.display());
    let source = root.join("fixture.txt");
    let sentinel = root.join("sentinel.txt");
    fs::write(&source, b"synthetic disposable fixture").unwrap();
    fs::write(&sentinel, b"sentinel remains").unwrap();
    let output = nix(&uri, &["--version"]).expect("nix-store must be installed");
    assert!(output.status.success(), "nix-store --version failed");
    let add = |path: &Path| {
        let input = path.to_str().unwrap();
        nix(&uri, &["--option", "min-free", "0", "--add", input])
    };
    let fixture = add(&source).expect("nix-store add");
    let sentinel_obj = add(&sentinel).expect("nix-store add sentinel");
    assert!(fixture.status.success(), "isolated nix-store --add failed");
    assert!(
        sentinel_obj.status.success(),
        "isolated sentinel add failed"
    );
    let object = String::from_utf8(fixture.stdout).unwrap().trim().to_owned();
    let sentinel_path = String::from_utf8(sentinel_obj.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(object.starts_with("/nix/store/"));
    assert!(sentinel_path.starts_with("/nix/store/"));
    let physical_object = root
        .join("nix/store")
        .join(Path::new(&object).file_name().unwrap());
    let physical_sentinel = root
        .join("nix/store")
        .join(Path::new(&sentinel_path).file_name().unwrap());
    assert!(
        physical_object.exists(),
        "local URI must place object below fixture root"
    );
    assert!(
        physical_sentinel.exists(),
        "local URI must place sentinel below fixture root"
    );
    let q = nix(&uri, &["--query", "--requisites", &object]).unwrap();
    assert!(q.status.success());
    assert!(
        String::from_utf8(q.stdout)
            .unwrap()
            .lines()
            .any(|line| line == object)
    );
    #[cfg(unix)]
    {
        let gcroot = root.join("gcroots/hule-test-root");
        fs::create_dir_all(gcroot.parent().unwrap()).unwrap();
        let gcroot_text = gcroot.to_str().unwrap();
        let rooted = nix(
            &uri,
            &[
                "--add-root",
                gcroot_text,
                "--indirect",
                "--realise",
                &object,
            ],
        )
        .unwrap();
        assert!(
            rooted.status.success(),
            "isolated native GC-root creation failed"
        );
        let roots = nix(&uri, &["--query", "--roots", &object]).unwrap();
        assert!(roots.status.success());
        assert!(
            !roots.stdout.is_empty(),
            "fixture object must be natively rooted"
        );

        let config = Config::from_toml(
            "schema_version=2\ndefault_profile='normal'\n[profiles.low]\ncategories=['nix_store_gc']\n[profiles.normal]\ncategories=['nix_store_gc']\n[profiles.aggressive]\ncategories=[]\n",
        ).unwrap();
        let mut runner = CommandNixRunner;
        let plan = nix_store::local_test_support::create_plan(
            &mut runner,
            &config,
            CleanupProfile::Normal,
            vec![physical_object.clone()],
            &uri,
        )
        .unwrap();
        let refused = nix_store::local_test_support::apply(
            &mut runner,
            &config,
            CleanupProfile::Normal,
            &plan,
            &uri,
            |_| Ok(true),
        )
        .unwrap();
        assert_eq!(
            refused.paths[0].status,
            PathApplyStatus::RetainedOrNotDeleted
        );
        assert!(physical_object.exists(), "live fixture path must remain");
        fs::remove_file(gcroot).unwrap();

        let plan = nix_store::local_test_support::create_plan(
            &mut runner,
            &config,
            CleanupProfile::Normal,
            vec![physical_object.clone()],
            &uri,
        )
        .unwrap();
        let deleted = nix_store::local_test_support::apply(
            &mut runner,
            &config,
            CleanupProfile::Normal,
            &plan,
            &uri,
            |_| Ok(true),
        )
        .unwrap();
        assert!(deleted.command_succeeded, "isolated Hule delete failed");
        assert_eq!(deleted.paths[0].status, PathApplyStatus::Removed);
    }
    assert!(
        physical_sentinel.exists(),
        "unselected sentinel is preserved"
    );
    let kept = nix(&uri, &["--query", "--requisites", &sentinel_path]).unwrap();
    assert!(kept.status.success());
    assert!(
        String::from_utf8(kept.stdout)
            .unwrap()
            .lines()
            .any(|line| line == sentinel_path)
    );
}
