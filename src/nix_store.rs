use crate::{
    CleanupProfile,
    action_plan::{
        MAX_CLOSURE_PATHS, MAX_INPUTS, NixStoreActionPlan, StoreIdentity, top_store_path,
    },
    config::{Category, Config},
};
use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const DAEMON: &str = "daemon";
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NixError {
    Command,
    Incomplete,
    InvalidOutput,
    UnsafePath,
    Disabled,
    Protected,
    Stale,
    Declined,
    NonInteractive,
}
impl fmt::Display for NixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Command => "Nix operation failed; deletion may have partially completed",
            Self::Incomplete => {
                "Nix delete completed but one or more paths remain or could not be verified"
            }
            Self::InvalidOutput => "Nix returned invalid data",
            Self::UnsafePath => "invalid Nix store path",
            Self::Disabled => "nix_store_gc is not enabled for this profile",
            Self::Protected => "a selected store path is protected",
            Self::Stale => "saved plan is stale; create a new plan",
            Self::Declined => "operation declined",
            Self::NonInteractive => "apply requires interactive stdin and stdout",
        })
    }
}
impl Error for NixError {}
pub trait NixRunner {
    fn store_dir(&mut self, endpoint: &str) -> Result<PathBuf, NixError>;
    fn version(&mut self, endpoint: &str) -> Result<String, NixError>;
    fn requisites(&mut self, endpoint: &str, path: &Path) -> Result<Vec<u8>, NixError>;
    fn delete(&mut self, endpoint: &str, paths: &[PathBuf]) -> Result<(), NixError>;
}
#[derive(Default)]
pub struct CommandNixRunner;
impl NixRunner for CommandNixRunner {
    fn store_dir(&mut self, endpoint: &str) -> Result<PathBuf, NixError> {
        #[cfg(feature = "nix-local-test-support")]
        if endpoint.starts_with("local?root=") {
            let root = local_root(endpoint).ok_or(NixError::UnsafePath)?;
            return Ok(root.join("nix/store"));
        }
        let out = command(
            "nix",
            &[
                "eval",
                "--raw",
                "--expr",
                "builtins.storeDir",
                "--store",
                endpoint,
            ],
        )?;
        let s = String::from_utf8(out).map_err(|_| NixError::InvalidOutput)?;
        one_line(s).map(PathBuf::from)
    }
    fn version(&mut self, endpoint: &str) -> Result<String, NixError> {
        let out = command("nix-store", &["--store", endpoint, "--version"])?;
        let s = String::from_utf8(out).map_err(|_| NixError::InvalidOutput)?;
        let v = one_line(s)?;
        if v.len() > 128 || v.chars().any(char::is_control) {
            return Err(NixError::InvalidOutput);
        }
        Ok(v)
    }
    fn requisites(&mut self, endpoint: &str, path: &Path) -> Result<Vec<u8>, NixError> {
        #[cfg(feature = "nix-local-test-support")]
        if endpoint.starts_with("local?root=") {
            let root = local_root(endpoint).ok_or(NixError::UnsafePath)?;
            let logical_path =
                Path::new("/nix/store").join(path.file_name().ok_or(NixError::UnsafePath)?);
            let output = command_path(
                "nix-store",
                &["--query", "--requisites", "--store", endpoint],
                &logical_path,
            )?;
            let physical_store = root.join("nix/store").to_string_lossy().into_owned();
            return Ok(String::from_utf8(output)
                .map_err(|_| NixError::InvalidOutput)?
                .replace("/nix/store/", &format!("{physical_store}/"))
                .into_bytes());
        }
        command_path(
            "nix-store",
            &["--query", "--requisites", "--store", endpoint],
            path,
        )
    }
    fn delete(&mut self, endpoint: &str, paths: &[PathBuf]) -> Result<(), NixError> {
        let mut c = Command::new("nix-store");
        c.args(["--delete", "--store", endpoint]);
        #[cfg(feature = "nix-local-test-support")]
        if endpoint.starts_with("local?root=") {
            if local_root(endpoint).is_none() {
                return Err(NixError::UnsafePath);
            }
            c.args(
                paths
                    .iter()
                    .map(|path| Path::new("/nix/store").join(path.file_name().unwrap_or_default())),
            );
            return run_bounded(c).map(|_| ());
        }
        c.args(paths);
        run_bounded(c).map(|_| ())
    }
}
#[cfg(feature = "nix-local-test-support")]
fn local_root(endpoint: &str) -> Option<PathBuf> {
    let root = endpoint.strip_prefix("local?root=")?;
    let path = PathBuf::from(root);
    let temp_root = fs::canonicalize(std::env::temp_dir()).ok()?;
    (path.is_absolute()
        && path != temp_root
        && path.starts_with(&temp_root)
        && fs::canonicalize(&path).ok().as_deref() == Some(path.as_path())
        && path.join("nix/store") != Path::new("/nix/store"))
    .then_some(path)
}

fn one_line(s: String) -> Result<String, NixError> {
    let s = s.trim_end_matches(['\n', '\r']);
    if s.is_empty() || s.contains(['\n', '\r']) || s.chars().any(char::is_control) {
        Err(NixError::InvalidOutput)
    } else {
        Ok(s.to_owned())
    }
}
fn command(program: &str, args: &[&str]) -> Result<Vec<u8>, NixError> {
    let mut c = Command::new(program);
    c.args(args);
    run_bounded(c)
}
fn command_path(program: &str, args: &[&str], path: &Path) -> Result<Vec<u8>, NixError> {
    let mut c = Command::new(program);
    c.args(args).arg(path);
    run_bounded(c)
}
fn run_bounded(mut c: Command) -> Result<Vec<u8>, NixError> {
    use std::io::Read;
    c.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = c.spawn().map_err(|_| NixError::Command)?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(NixError::Command);
    };
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut limited = stdout.take((MAX_OUTPUT + 1) as u64);
        let result = limited.read_to_end(&mut out);
        (result.is_ok(), out)
    });
    let start = Instant::now();
    loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(NixError::Command);
            }
        };
        if let Some(status) = status {
            let (ok, out) = reader.join().map_err(|_| NixError::Command)?;
            if !ok || out.len() > MAX_OUTPUT || !status.success() {
                return Err(NixError::Command);
            }
            return Ok(out);
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(NixError::Command);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn gate(config: &Config, profile: CleanupProfile, paths: &[PathBuf]) -> Result<(), NixError> {
    if !config.categories(profile).contains(&Category::NixStoreGc) {
        return Err(NixError::Disabled);
    }
    if paths.iter().any(|p| config.overlaps_resolved_keep_path(p)) {
        return Err(NixError::Protected);
    }
    Ok(())
}
fn identity(r: &mut impl NixRunner, endpoint: &str) -> Result<StoreIdentity, NixError> {
    let store_dir = r.store_dir(endpoint)?;
    if !store_dir.is_absolute()
        || store_dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || fs::canonicalize(&store_dir).map_err(|_| NixError::InvalidOutput)? != store_dir
    {
        return Err(NixError::InvalidOutput);
    }
    let version = r.version(endpoint)?;
    if version.is_empty() || version.len() > 128 || version.chars().any(char::is_control) {
        return Err(NixError::InvalidOutput);
    }
    Ok(StoreIdentity {
        endpoint: endpoint.into(),
        version,
        store_dir,
    })
}
fn validate_top(path: &Path, store: &Path) -> Result<(), NixError> {
    if !top_store_path(path, store) {
        return Err(NixError::UnsafePath);
    }
    let meta = fs::symlink_metadata(path).map_err(|_| NixError::UnsafePath)?;
    if meta.file_type().is_symlink() || !meta.is_dir() && !meta.is_file() {
        return Err(NixError::UnsafePath);
    }
    let canonical = fs::canonicalize(path).map_err(|_| NixError::UnsafePath)?;
    if canonical != path {
        return Err(NixError::UnsafePath);
    }
    Ok(())
}
fn closure(
    r: &mut impl NixRunner,
    endpoint: &str,
    inputs: &[PathBuf],
    store: &Path,
) -> Result<Vec<PathBuf>, NixError> {
    if inputs.is_empty() || inputs.len() > MAX_INPUTS {
        return Err(NixError::UnsafePath);
    }
    let mut all = BTreeSet::new();
    for p in inputs {
        validate_top(p, store)?;
        let bytes = r.requisites(endpoint, p)?;
        if bytes.len() > MAX_OUTPUT {
            return Err(NixError::InvalidOutput);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| NixError::InvalidOutput)?;
        for line in text.lines() {
            let p = PathBuf::from(line);
            validate_top(&p, store)?;
            all.insert(p);
            if all.len() > MAX_CLOSURE_PATHS {
                return Err(NixError::InvalidOutput);
            }
        }
    }
    if !inputs.iter().all(|p| all.contains(p)) {
        return Err(NixError::InvalidOutput);
    }
    Ok(all.into_iter().collect())
}
pub fn create_plan(
    r: &mut impl NixRunner,
    config: &Config,
    profile: CleanupProfile,
    inputs: Vec<PathBuf>,
) -> Result<NixStoreActionPlan, NixError> {
    create_plan_at(r, config, profile, inputs, DAEMON)
}

fn validate_inputs(inputs: &[PathBuf]) -> Result<(), NixError> {
    if inputs.is_empty() || inputs.len() > MAX_INPUTS {
        return Err(NixError::UnsafePath);
    }
    if inputs.iter().any(|path| {
        !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
    }) || inputs
        .iter()
        .map(|path| path.parent())
        .any(|parent| parent.is_none() || parent != inputs[0].parent())
        || inputs.iter().collect::<BTreeSet<_>>().len() != inputs.len()
    {
        return Err(NixError::UnsafePath);
    }
    Ok(())
}

/// Inspects explicitly selected store objects without creating an action plan or deleting data.
pub fn inspect(
    r: &mut impl NixRunner,
    config: &Config,
    profile: CleanupProfile,
    inputs: Vec<PathBuf>,
) -> Result<crate::nix_inspection::NixStoreInspection, NixError> {
    validate_inputs(&inputs)?;
    gate(config, profile, &inputs)?;
    let identity = identity(r, DAEMON)?;
    for path in &inputs {
        validate_top(path, &identity.store_dir)?;
    }
    let selected_objects = inputs
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let requisites = closure(r, DAEMON, &selected_objects, &identity.store_dir)?;
    gate(config, profile, &requisites)?;
    Ok(crate::nix_inspection::NixStoreInspection {
        format: "hule-nix-store-inspection",
        schema_version: 1,
        profile,
        selected_objects,
        requisites,
        host_scope: crate::nix_inspection::InspectionStatus::Unchecked,
        version_scope: crate::nix_inspection::InspectionStatus::Unchecked,
        store_scope: crate::nix_inspection::InspectionStatus::Unchecked,
        reclaimed_bytes: crate::nix_inspection::InspectionStatus::Unchecked,
        authorization: crate::nix_inspection::InspectionAuthorization::NotAuthorized,
        limitation: "Nix queries may open, create, or migrate store metadata; requisites are a dynamic query, not deletion authorization or a byte guarantee, and host/version/store scope remains unchecked",
    })
}

fn create_plan_at(
    r: &mut impl NixRunner,
    config: &Config,
    profile: CleanupProfile,
    inputs: Vec<PathBuf>,
    endpoint: &str,
) -> Result<NixStoreActionPlan, NixError> {
    validate_inputs(&inputs)?;
    gate(config, profile, &inputs)?;
    let id = identity(r, endpoint)?;
    for p in &inputs {
        validate_top(p, &id.store_dir)?;
    }
    let inputs = inputs
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let found = closure(r, endpoint, &inputs, &id.store_dir)?;
    gate(config, profile, &found)?;
    let plan = NixStoreActionPlan {
        format: "hule-nix-store-action".into(),
        schema_version: 1,
        profile,
        category: Category::NixStoreGc,
        inputs,
        closure: found,
        identity: id,
    };
    plan.validate_for_endpoint(endpoint)
        .map_err(|_| NixError::InvalidOutput)?;
    Ok(plan)
}
pub fn revalidate(
    r: &mut impl NixRunner,
    config: &Config,
    profile: CleanupProfile,
    plan: &NixStoreActionPlan,
) -> Result<(), NixError> {
    revalidate_at(r, config, profile, plan, DAEMON)
}

fn revalidate_at(
    r: &mut impl NixRunner,
    config: &Config,
    profile: CleanupProfile,
    plan: &NixStoreActionPlan,
    endpoint: &str,
) -> Result<(), NixError> {
    plan.validate_for_endpoint(endpoint)
        .map_err(|_| NixError::Stale)?;
    if profile != plan.profile || identity(r, endpoint)? != plan.identity {
        return Err(NixError::Stale);
    }
    gate(config, profile, &plan.closure)?;
    if closure(r, endpoint, &plan.inputs, &plan.identity.store_dir)? != plan.closure {
        return Err(NixError::Stale);
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathApplyStatus {
    Removed,
    RetainedOrNotDeleted,
    AbsentAfterFailure,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathApplyResult {
    pub path: PathBuf,
    pub status: PathApplyStatus,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
    pub command_succeeded: bool,
    pub paths: Vec<PathApplyResult>,
}
impl ApplyResult {
    pub fn is_complete(&self) -> bool {
        self.command_succeeded
            && self
                .paths
                .iter()
                .all(|path| path.status == PathApplyStatus::Removed)
    }
}

pub fn apply(
    r: &mut impl NixRunner,
    config: &Config,
    profile: CleanupProfile,
    plan: &NixStoreActionPlan,
    confirm: impl FnOnce(&[PathBuf]) -> Result<bool, NixError>,
) -> Result<ApplyResult, NixError> {
    apply_at(r, config, profile, plan, DAEMON, confirm)
}

fn apply_at(
    r: &mut impl NixRunner,
    config: &Config,
    profile: CleanupProfile,
    plan: &NixStoreActionPlan,
    endpoint: &str,
    confirm: impl FnOnce(&[PathBuf]) -> Result<bool, NixError>,
) -> Result<ApplyResult, NixError> {
    revalidate_at(r, config, profile, plan, endpoint)?;
    if !confirm(&plan.closure)? {
        return Err(NixError::Declined);
    }
    revalidate_at(r, config, profile, plan, endpoint)?;
    for path in &plan.closure {
        validate_top(path, &plan.identity.store_dir)?;
    }
    let command_succeeded = r.delete(endpoint, &plan.closure).is_ok();
    let paths = plan
        .closure
        .iter()
        .map(|path| {
            let status = match fs::symlink_metadata(path) {
                Ok(_) => PathApplyStatus::RetainedOrNotDeleted,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && command_succeeded => {
                    PathApplyStatus::Removed
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    PathApplyStatus::AbsentAfterFailure
                }
                Err(_) => PathApplyStatus::Unknown,
            };
            PathApplyResult {
                path: path.clone(),
                status,
            }
        })
        .collect();
    Ok(ApplyResult {
        command_succeeded,
        paths,
    })
}

#[cfg(feature = "nix-local-test-support")]
#[doc(hidden)]
pub mod local_test_support {
    use super::*;

    fn checked_endpoint(endpoint: &str) -> Result<(), NixError> {
        let root = endpoint
            .strip_prefix("local?root=")
            .ok_or(NixError::UnsafePath)?;
        let path = Path::new(root);
        let temp_root = fs::canonicalize(std::env::temp_dir()).map_err(|_| NixError::UnsafePath)?;
        if !path.is_absolute()
            || fs::canonicalize(path).map_err(|_| NixError::UnsafePath)? != path
            || path == temp_root
            || !path.starts_with(&temp_root)
            || path == Path::new("/nix/store")
            || !path.is_dir()
        {
            return Err(NixError::UnsafePath);
        }
        Ok(())
    }

    pub fn create_plan(
        r: &mut impl NixRunner,
        config: &Config,
        profile: CleanupProfile,
        inputs: Vec<PathBuf>,
        endpoint: &str,
    ) -> Result<NixStoreActionPlan, NixError> {
        checked_endpoint(endpoint)?;
        create_plan_at(r, config, profile, inputs, endpoint)
    }

    pub fn apply(
        r: &mut impl NixRunner,
        config: &Config,
        profile: CleanupProfile,
        plan: &NixStoreActionPlan,
        endpoint: &str,
        confirm: impl FnOnce(&[PathBuf]) -> Result<bool, NixError>,
    ) -> Result<ApplyResult, NixError> {
        checked_endpoint(endpoint)?;
        apply_at(r, config, profile, plan, endpoint, confirm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct Mock {
        dir: PathBuf,
        version: &'static str,
        queries: VecDeque<Vec<u8>>,
        deletes: Vec<(String, Vec<PathBuf>)>,
        delete_succeeds: bool,
        remove_on_delete: Vec<PathBuf>,
    }
    impl NixRunner for Mock {
        fn store_dir(&mut self, _: &str) -> Result<PathBuf, NixError> {
            Ok(self.dir.clone())
        }
        fn version(&mut self, _: &str) -> Result<String, NixError> {
            Ok(self.version.into())
        }
        fn requisites(&mut self, _: &str, _: &Path) -> Result<Vec<u8>, NixError> {
            self.queries.pop_front().ok_or(NixError::Command)
        }
        fn delete(&mut self, e: &str, p: &[PathBuf]) -> Result<(), NixError> {
            self.deletes.push((e.into(), p.into()));
            for path in &self.remove_on_delete {
                let _ = fs::remove_file(path);
            }
            if self.delete_succeeds {
                Ok(())
            } else {
                Err(NixError::Command)
            }
        }
    }
    fn cfg(keep: &str) -> Config {
        Config::from_toml(&format!("schema_version=2\ndefault_profile='normal'\nadditional_keep_paths=['{keep}']\n[profiles.low]\ncategories=['nix_store_gc']\n[profiles.normal]\ncategories=['nix_store_gc']\n[profiles.aggressive]\ncategories=[]")).unwrap()
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let d = fs::canonicalize(t.path()).unwrap().join("store");
        fs::create_dir(&d).unwrap();
        let p = d.join("abc-object");
        fs::write(&p, b"fixture").unwrap();
        (t, d, p)
    }
    #[test]
    fn opted_out_or_protected_makes_no_query() {
        let (_t, d, p) = fixture();
        let mut r = Mock {
            dir: d,
            version: "v",
            queries: VecDeque::new(),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        assert_eq!(
            create_plan(
                &mut r,
                &cfg("/nope"),
                CleanupProfile::Aggressive,
                vec![p.clone()]
            )
            .unwrap_err(),
            NixError::Disabled
        );
        assert!(
            create_plan(
                &mut r,
                &cfg(p.to_str().unwrap()),
                CleanupProfile::Normal,
                vec![p]
            )
            .is_err()
        );
        assert!(r.queries.is_empty());
    }
    #[test]
    fn exact_delete_saved_selection_and_second_query() {
        let (_t, d, p) = fixture();
        let mut r = Mock {
            dir: d,
            version: "v",
            queries: VecDeque::from([
                format!("{}\n", p.display()).into_bytes(),
                format!("{}\n", p.display()).into_bytes(),
                format!("{}\n", p.display()).into_bytes(),
            ]),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![p.clone()],
        };
        let c = cfg("/not-present");
        let plan = create_plan(&mut r, &c, CleanupProfile::Normal, vec![p.clone()]).unwrap();
        let result = apply(&mut r, &c, CleanupProfile::Normal, &plan, |paths| {
            Ok(paths == [p.clone()])
        })
        .unwrap();
        assert!(result.command_succeeded);
        assert!(result.is_complete());
        assert_eq!(result.paths[0].status, PathApplyStatus::Removed);
        assert_eq!(r.deletes, vec![("daemon".into(), vec![p])]);
    }
    #[test]
    fn apply_reports_retained_and_partial_failure_per_path() {
        let (_t, d, p) = fixture();
        let response = format!("{}\n", p.display()).into_bytes();
        let mut live = Mock {
            dir: d.clone(),
            version: "v",
            queries: VecDeque::from([response.clone(), response.clone(), response.clone()]),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        let c = cfg("/not-present");
        let plan = create_plan(&mut live, &c, CleanupProfile::Normal, vec![p.clone()]).unwrap();
        let result = apply(&mut live, &c, CleanupProfile::Normal, &plan, |_| Ok(true)).unwrap();
        assert_eq!(
            result.paths[0].status,
            PathApplyStatus::RetainedOrNotDeleted
        );
        assert!(!result.is_complete());

        let mut partial = Mock {
            dir: d,
            version: "v",
            queries: VecDeque::from([response.clone(), response.clone(), response]),
            deletes: vec![],
            delete_succeeds: false,
            remove_on_delete: vec![p.clone()],
        };
        let plan = create_plan(&mut partial, &c, CleanupProfile::Normal, vec![p]).unwrap();
        let result = apply(&mut partial, &c, CleanupProfile::Normal, &plan, |_| {
            Ok(true)
        })
        .unwrap();
        assert!(!result.command_succeeded);
        assert!(!result.is_complete());
        assert_eq!(result.paths[0].status, PathApplyStatus::AbsentAfterFailure);
    }

    #[test]
    fn shared_requisites_are_deduplicated_and_revalidated() {
        let (_t, d, first) = fixture();
        let second = d.join("def-object");
        let shared = d.join("shared-object");
        fs::write(&second, b"second").unwrap();
        fs::write(&shared, b"shared").unwrap();
        let response = format!("{}\n{}\n", first.display(), shared.display()).into_bytes();
        let response_second = format!("{}\n{}\n", second.display(), shared.display()).into_bytes();
        let mut r = Mock {
            dir: d,
            version: "v",
            queries: VecDeque::from([
                response.clone(),
                response_second.clone(),
                response.clone(),
                response_second.clone(),
                response.clone(),
                response_second.clone(),
                response.clone(),
                response_second.clone(),
            ]),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        let c = cfg("/not-present");
        let plan = create_plan(
            &mut r,
            &c,
            CleanupProfile::Normal,
            vec![first.clone(), second.clone()],
        )
        .unwrap();
        assert_eq!(plan.closure, vec![first, second, shared]);
        revalidate(&mut r, &c, CleanupProfile::Normal, &plan).unwrap();
        let result = apply(&mut r, &c, CleanupProfile::Normal, &plan, |_| Ok(true)).unwrap();
        assert!(result.command_succeeded);
        assert_eq!(r.deletes, vec![("daemon".into(), plan.closure.clone())]);
    }

    #[test]
    fn new_protection_blocks_and_no_delete() {
        let (_t, d, p) = fixture();
        let mut r = Mock {
            dir: d,
            version: "v",
            queries: VecDeque::from([format!("{}\n", p.display()).into_bytes()]),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        let plan = create_plan(
            &mut r,
            &cfg("/not-present"),
            CleanupProfile::Normal,
            vec![p.clone()],
        )
        .unwrap();
        assert_eq!(
            apply(
                &mut r,
                &cfg(p.to_str().unwrap()),
                CleanupProfile::Normal,
                &plan,
                |_| Ok(true)
            )
            .unwrap_err(),
            NixError::Protected
        );
        assert!(r.deletes.is_empty());
    }
    #[test]
    fn identity_and_closure_drift_rejected() {
        let (_t, d, p) = fixture();
        let mut r = Mock {
            dir: d,
            version: "v",
            queries: VecDeque::from([
                format!("{}\n", p.display()).into_bytes(),
                b"/other\n".to_vec(),
            ]),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        let c = cfg("/none");
        let plan = create_plan(&mut r, &c, CleanupProfile::Normal, vec![p]).unwrap();
        assert_eq!(
            revalidate(&mut r, &c, CleanupProfile::Normal, &plan).unwrap_err(),
            NixError::UnsafePath
        );
    }

    #[test]
    fn changed_store_identity_is_stale_and_still_never_deletes() {
        let (_t, store, path) = fixture();
        let mut runner = Mock {
            dir: store.clone(),
            version: "v",
            queries: VecDeque::from([format!("{}\n", path.display()).into_bytes()]),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        let config = cfg("/not-present");
        let plan = create_plan(&mut runner, &config, CleanupProfile::Normal, vec![path]).unwrap();
        let replacement_store = store.parent().unwrap().join("replacement-store");
        fs::create_dir(&replacement_store).unwrap();
        runner.dir = replacement_store;

        assert_eq!(
            revalidate(&mut runner, &config, CleanupProfile::Normal, &plan).unwrap_err(),
            NixError::Stale
        );
        assert!(runner.deletes.is_empty());
    }

    #[test]
    fn inspection_is_ordered_non_authorizing_and_never_deletes() {
        let (_t, d, p) = fixture();
        let second = d.join("def-object");
        fs::write(&second, b"fixture").unwrap();
        let mut r = Mock {
            dir: d,
            version: "v",
            queries: VecDeque::from([format!(
                "{}\n{}\n{}\n",
                second.display(),
                p.display(),
                second.display()
            )
            .into_bytes()]),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        let report = inspect(
            &mut r,
            &cfg("/not-present"),
            CleanupProfile::Normal,
            vec![p.clone()],
        )
        .unwrap();

        assert_eq!(report.selected_objects.as_slice(), std::slice::from_ref(&p));
        assert_eq!(report.requisites, [p, second]);
        assert_eq!(
            report.authorization,
            crate::nix_inspection::InspectionAuthorization::NotAuthorized
        );
        assert_eq!(
            report.host_scope,
            crate::nix_inspection::InspectionStatus::Unchecked
        );
        assert_eq!(
            report.version_scope,
            crate::nix_inspection::InspectionStatus::Unchecked
        );
        assert_eq!(
            report.store_scope,
            crate::nix_inspection::InspectionStatus::Unchecked
        );
        assert_eq!(
            report.reclaimed_bytes,
            crate::nix_inspection::InspectionStatus::Unchecked
        );
        assert!(r.deletes.is_empty());
    }

    #[test]
    fn inspection_fails_closed_when_disabled_protected_or_query_fails() {
        let (_t, d, p) = fixture();
        let mut disabled = Mock {
            dir: d.clone(),
            version: "v",
            queries: VecDeque::new(),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        let off = Config::from_toml("schema_version=2\ndefault_profile='normal'\n[profiles.low]\ncategories=[]\n[profiles.normal]\ncategories=[]\n[profiles.aggressive]\ncategories=[]").unwrap();
        assert_eq!(
            inspect(&mut disabled, &off, CleanupProfile::Normal, vec![p.clone()]).unwrap_err(),
            NixError::Disabled
        );
        assert!(disabled.deletes.is_empty());

        let mut protected = Mock {
            dir: d.clone(),
            version: "v",
            queries: VecDeque::new(),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        assert_eq!(
            inspect(
                &mut protected,
                &cfg(p.to_str().unwrap()),
                CleanupProfile::Normal,
                vec![p.clone()]
            )
            .unwrap_err(),
            NixError::Protected
        );
        assert!(protected.deletes.is_empty());

        let mut failed = Mock {
            dir: d,
            version: "v",
            queries: VecDeque::new(),
            deletes: vec![],
            delete_succeeds: true,
            remove_on_delete: vec![],
        };
        assert_eq!(
            inspect(
                &mut failed,
                &cfg("/not-present"),
                CleanupProfile::Normal,
                vec![p]
            )
            .unwrap_err(),
            NixError::Command
        );
        assert!(failed.deletes.is_empty());
    }
}
