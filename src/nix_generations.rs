use crate::{
    CleanupProfile,
    action_plan::{
        ActivationRootSnapshot, ActivationRootState, FileIdentity, GenerationIdentity,
        GenerationSnapshot, MAX_GENERATIONS, NixGenerationActionPlan, top_store_path,
    },
    config::{Category, Config, NixProfileConfig, NixProfileKind},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::{CStr, OsStr},
    fmt, fs,
    io::Read,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const DAEMON: &str = "daemon";
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
pub const MAX_GENERATION_IDS: usize = 32;
const MAX_PROFILE_ENTRIES: usize = MAX_GENERATIONS * 4;
const SUPPORTED_VERSION: &str = "2.34.6";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationError {
    Command,
    InvalidOutput,
    UnsafePath,
    Disabled,
    Protected,
    Stale,
    Declined,
    Incomplete,
    NonInteractive,
}

impl fmt::Display for GenerationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Command => {
                "Nix generation operation failed; removal may have partially completed"
            }
            Self::InvalidOutput => "Nix returned invalid generation data",
            Self::UnsafePath => "Nix profile is outside the supported user namespace",
            Self::Disabled => "Nix generation cleanup is not enabled for this profile",
            Self::Protected => "a selected Nix generation is protected",
            Self::Stale => "saved generation plan is stale; create a new plan",
            Self::Declined => "operation declined",
            Self::Incomplete => {
                "generation cleanup was incomplete or preservation could not be verified"
            }
            Self::NonInteractive => "apply requires interactive stdin and stdout",
        })
    }
}
impl Error for GenerationError {}

pub trait GenerationRunner {
    fn identity(&mut self, endpoint: &str) -> Result<GenerationIdentity, GenerationError>;
    fn delete_generations(
        &mut self,
        endpoint: &str,
        profile: &Path,
        ids: &[u32],
    ) -> Result<(), GenerationError>;
}

#[derive(Default)]
pub struct CommandGenerationRunner {
    #[cfg(feature = "nix-local-test-support")]
    fixture_endpoint: Option<String>,
}

impl GenerationRunner for CommandGenerationRunner {
    fn identity(&mut self, endpoint: &str) -> Result<GenerationIdentity, GenerationError> {
        let output = command(
            "nix",
            &[
                "--extra-experimental-features",
                "nix-command",
                "eval",
                "--raw",
                "--expr",
                "builtins.storeDir",
                "--store",
                endpoint,
            ],
        )?;
        let store_dir = PathBuf::from(one_line(output)?);
        if !store_dir.is_absolute()
            || store_dir
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || fs::canonicalize(&store_dir).map_err(|_| GenerationError::InvalidOutput)?
                != store_dir
        {
            return Err(GenerationError::InvalidOutput);
        }
        let store_version = one_line(command("nix-store", &["--store", endpoint, "--version"])?)?;
        let nix_env_version = one_line(command("nix-env", &["--version"])?)?;
        if store_version != format!("nix-store (Nix) {SUPPORTED_VERSION}")
            || nix_env_version != format!("nix-env (Nix) {SUPPORTED_VERSION}")
        {
            return Err(GenerationError::InvalidOutput);
        }
        Ok(GenerationIdentity {
            endpoint: DAEMON.into(),
            store_version,
            nix_env_version,
            store_dir,
        })
    }

    fn delete_generations(
        &mut self,
        endpoint: &str,
        profile: &Path,
        ids: &[u32],
    ) -> Result<(), GenerationError> {
        let endpoint_allowed = endpoint == DAEMON || {
            #[cfg(feature = "nix-local-test-support")]
            {
                self.fixture_endpoint.as_deref() == Some(endpoint)
            }
            #[cfg(not(feature = "nix-local-test-support"))]
            {
                false
            }
        };
        if !endpoint_allowed || ids.is_empty() || ids.len() > MAX_GENERATION_IDS {
            return Err(GenerationError::UnsafePath);
        }
        let mut command = Command::new("nix-env");
        command
            .args(["--store", endpoint, "--profile"])
            .arg(profile)
            .arg("--delete-generations");
        for id in ids {
            if *id == 0 {
                return Err(GenerationError::UnsafePath);
            }
            command.arg(id.to_string());
        }
        run_bounded(command).map(|_| ())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationApplyResult {
    pub command_succeeded: bool,
    pub preservation_verified: bool,
    pub statuses: Vec<GenerationApplyStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationApplyStatus {
    pub id: u32,
    pub status: GenerationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    Removed,
    RetainedOrNotDeleted,
    AbsentAfterFailure,
    Unknown,
}

impl GenerationApplyResult {
    pub fn is_complete(&self) -> bool {
        self.command_succeeded
            && self.preservation_verified
            && self
                .statuses
                .iter()
                .all(|status| status.status == GenerationStatus::Removed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserIdentity {
    uid: u32,
    home: PathBuf,
    username: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileSnapshot {
    identity: GenerationIdentity,
    uid: u32,
    profile_path: PathBuf,
    parent_identity: FileIdentity,
    current_link_identity: FileIdentity,
    current_link_target: String,
    current_generation: u32,
    history: Vec<GenerationSnapshot>,
    activation_roots: Vec<ActivationRootSnapshot>,
}

pub fn create_plan(
    runner: &mut impl GenerationRunner,
    config: &Config,
    cleanup_profile: CleanupProfile,
    profile_name: &str,
    selected_ids: &[u32],
) -> Result<NixGenerationActionPlan, GenerationError> {
    create_plan_at(
        runner,
        config,
        cleanup_profile,
        profile_name,
        selected_ids,
        DAEMON,
        None,
    )
}

/// Reads one explicitly configured profile and classifies its current and historical generations.
/// The result is informational and is not accepted by the apply dispatcher.
pub fn inspect_profile(
    runner: &mut impl GenerationRunner,
    config: &Config,
    cleanup_profile: CleanupProfile,
    profile_name: &str,
    selected_ids: &[u32],
) -> Result<crate::nix_inspection::NixGenerationInspection, GenerationError> {
    inspect_profile_at(
        runner,
        config,
        cleanup_profile,
        profile_name,
        selected_ids,
        DAEMON,
        None,
    )
}

fn inspect_profile_at(
    runner: &mut impl GenerationRunner,
    config: &Config,
    cleanup_profile: CleanupProfile,
    profile_name: &str,
    selected_ids: &[u32],
    endpoint: &str,
    fixture_root: Option<&Path>,
) -> Result<crate::nix_inspection::NixGenerationInspection, GenerationError> {
    if selected_ids.len() > MAX_GENERATION_IDS {
        return Err(GenerationError::UnsafePath);
    }
    let mut selected_ids = selected_ids.to_vec();
    selected_ids.sort_unstable();
    if selected_ids
        .iter()
        .any(|id| *id == 0 || *id > i32::MAX as u32)
        || selected_ids.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(GenerationError::UnsafePath);
    }
    let profile_config = config
        .nix_profiles()
        .get(profile_name)
        .ok_or(GenerationError::UnsafePath)?;
    let category = match profile_config.kind() {
        NixProfileKind::User => Category::NixUserGenerations,
        NixProfileKind::HomeManager => Category::NixHomeManagerGenerations,
    };
    if !config.categories(cleanup_profile).contains(&category) {
        return Err(GenerationError::Disabled);
    }
    let user = current_user()?;
    let profile_path = validate_profile_path(profile_config, &user, fixture_root)?;
    let identity = runner.identity(endpoint)?;
    if identity.endpoint != DAEMON
        || identity.store_version != format!("nix-store (Nix) {SUPPORTED_VERSION}")
        || identity.nix_env_version != format!("nix-env (Nix) {SUPPORTED_VERSION}")
        || !identity.store_dir.is_absolute()
        || fs::canonicalize(&identity.store_dir).map_err(|_| GenerationError::InvalidOutput)?
            != identity.store_dir
    {
        return Err(GenerationError::InvalidOutput);
    }
    let snapshot = snapshot_profile(profile_config, profile_path, identity, &user, fixture_root)?;
    if config.overlaps_resolved_keep_path(&snapshot.profile_path)
        || snapshot
            .activation_roots
            .iter()
            .any(|root| config.overlaps_resolved_keep_path(&root.path))
    {
        return Err(GenerationError::Protected);
    }
    let history_by_id = snapshot
        .history
        .iter()
        .map(|generation| (generation.id, generation))
        .collect::<BTreeMap<_, _>>();
    if selected_ids
        .iter()
        .any(|id| *id == snapshot.current_generation || !history_by_id.contains_key(id))
    {
        return Err(GenerationError::UnsafePath);
    }
    if selected_ids.iter().any(|id| {
        let generation = history_by_id[id];
        config.overlaps_resolved_keep_path(&generation.link)
            || config.overlaps_resolved_keep_path(&generation.target)
    }) {
        return Err(GenerationError::Protected);
    }
    let generations = snapshot
        .history
        .iter()
        .map(|generation| crate::nix_inspection::NixGenerationEntry {
            id: generation.id,
            role: if generation.id == snapshot.current_generation {
                crate::nix_inspection::GenerationRole::Current
            } else if selected_ids.contains(&generation.id) {
                crate::nix_inspection::GenerationRole::SelectedHistorical
            } else {
                crate::nix_inspection::GenerationRole::Historical
            },
        })
        .collect();
    Ok(crate::nix_inspection::NixGenerationInspection {
        format: "hule-nix-generation-inspection",
        schema_version: 1,
        cleanup_profile,
        profile_name: profile_name.to_owned(),
        kind: profile_config.kind(),
        current_generation: snapshot.current_generation,
        generations,
        selected_ids,
        host_scope: crate::nix_inspection::InspectionStatus::Unchecked,
        version_scope: crate::nix_inspection::InspectionStatus::Unchecked,
        store_scope: crate::nix_inspection::InspectionStatus::Unchecked,
        authorization: crate::nix_inspection::InspectionAuthorization::NotAuthorized,
        history_removes_profile_links_only: true,
        store_gc_performed: false,
        limitation: "M2 is restricted to Nix 2.34.6; history removal affects selected profile links only, does not delete store objects or run garbage collection, and this report does not authorize cleanup",
    })
}

fn create_plan_at(
    runner: &mut impl GenerationRunner,
    config: &Config,
    cleanup_profile: CleanupProfile,
    profile_name: &str,
    selected_ids: &[u32],
    endpoint: &str,
    fixture_root: Option<&Path>,
) -> Result<NixGenerationActionPlan, GenerationError> {
    if selected_ids.is_empty() || selected_ids.len() > MAX_GENERATION_IDS {
        return Err(GenerationError::UnsafePath);
    }
    let mut selected_ids = selected_ids.to_vec();
    selected_ids.sort_unstable();
    if selected_ids[0] == 0
        || selected_ids.iter().any(|id| *id > i32::MAX as u32)
        || selected_ids.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(GenerationError::UnsafePath);
    }
    let profile_config = config
        .nix_profiles()
        .get(profile_name)
        .ok_or(GenerationError::UnsafePath)?;
    let category = match profile_config.kind() {
        NixProfileKind::User => Category::NixUserGenerations,
        NixProfileKind::HomeManager => Category::NixHomeManagerGenerations,
    };
    if !config.categories(cleanup_profile).contains(&category) {
        return Err(GenerationError::Disabled);
    }
    let user = current_user()?;
    let profile_path = validate_profile_path(profile_config, &user, fixture_root)?;
    let identity = runner.identity(endpoint)?;
    if identity.endpoint != DAEMON
        || !identity.store_dir.is_absolute()
        || fs::canonicalize(&identity.store_dir).map_err(|_| GenerationError::InvalidOutput)?
            != identity.store_dir
    {
        return Err(GenerationError::InvalidOutput);
    }
    let snapshot = snapshot_profile(profile_config, profile_path, identity, &user, fixture_root)?;
    let by_id = snapshot
        .history
        .iter()
        .map(|item| (item.id, item))
        .collect::<BTreeMap<_, _>>();
    if selected_ids
        .iter()
        .any(|id| *id == snapshot.current_generation || !by_id.contains_key(id))
    {
        return Err(GenerationError::UnsafePath);
    }
    let mut protected = vec![snapshot.profile_path.clone()];
    for id in &selected_ids {
        let item = by_id.get(id).ok_or(GenerationError::UnsafePath)?;
        protected.push(item.link.clone());
        protected.push(item.target.clone());
    }
    for root in &snapshot.activation_roots {
        protected.push(root.path.clone());
    }
    if protected
        .iter()
        .any(|path| config.overlaps_resolved_keep_path(path))
    {
        return Err(GenerationError::Protected);
    }
    let plan = NixGenerationActionPlan {
        format: "hule-nix-generation-action".into(),
        schema_version: 1,
        profile: cleanup_profile,
        category,
        nix_profile: profile_name.into(),
        kind: profile_config.kind(),
        profile_path: snapshot.profile_path,
        identity: snapshot.identity,
        uid: snapshot.uid,
        parent_identity: snapshot.parent_identity,
        current_link_identity: snapshot.current_link_identity,
        current_link_target: snapshot.current_link_target,
        history: snapshot.history,
        current_generation: snapshot.current_generation,
        selected_ids,
        activation_roots: snapshot.activation_roots,
    };
    plan.validate()
        .map_err(|_| GenerationError::InvalidOutput)?;
    Ok(plan)
}

pub fn revalidate(
    runner: &mut impl GenerationRunner,
    config: &Config,
    cleanup_profile: CleanupProfile,
    plan: &NixGenerationActionPlan,
) -> Result<(), GenerationError> {
    revalidate_at(runner, config, cleanup_profile, plan, DAEMON, None)
}

fn revalidate_at(
    runner: &mut impl GenerationRunner,
    config: &Config,
    cleanup_profile: CleanupProfile,
    plan: &NixGenerationActionPlan,
    endpoint: &str,
    fixture_root: Option<&Path>,
) -> Result<(), GenerationError> {
    plan.validate().map_err(|_| GenerationError::Stale)?;
    if cleanup_profile != plan.profile {
        return Err(GenerationError::Stale);
    }
    let fresh = create_plan_at(
        runner,
        config,
        cleanup_profile,
        &plan.nix_profile,
        &plan.selected_ids,
        endpoint,
        fixture_root,
    )
    .map_err(|error| match error {
        GenerationError::Disabled | GenerationError::Protected => error,
        _ => GenerationError::Stale,
    })?;
    if &fresh != plan {
        return Err(GenerationError::Stale);
    }
    Ok(())
}

pub fn apply(
    runner: &mut impl GenerationRunner,
    config: &Config,
    cleanup_profile: CleanupProfile,
    plan: &NixGenerationActionPlan,
    confirm: impl FnOnce(&NixGenerationActionPlan) -> Result<bool, GenerationError>,
) -> Result<GenerationApplyResult, GenerationError> {
    apply_at(runner, config, cleanup_profile, plan, DAEMON, None, confirm)
}

fn apply_at(
    runner: &mut impl GenerationRunner,
    config: &Config,
    cleanup_profile: CleanupProfile,
    plan: &NixGenerationActionPlan,
    endpoint: &str,
    fixture_root: Option<&Path>,
    confirm: impl FnOnce(&NixGenerationActionPlan) -> Result<bool, GenerationError>,
) -> Result<GenerationApplyResult, GenerationError> {
    revalidate_at(
        runner,
        config,
        cleanup_profile,
        plan,
        endpoint,
        fixture_root,
    )?;
    if !confirm(plan)? {
        return Err(GenerationError::Declined);
    }
    revalidate_at(
        runner,
        config,
        cleanup_profile,
        plan,
        endpoint,
        fixture_root,
    )?;
    let command_succeeded = runner
        .delete_generations(endpoint, &plan.profile_path, &plan.selected_ids)
        .is_ok();
    let post = snapshot_profile_for_plan(runner, plan, endpoint, fixture_root);
    let preservation_verified = post
        .as_ref()
        .is_ok_and(|after| preservation_matches(plan, after));
    let statuses = plan
        .selected_ids
        .iter()
        .map(|id| {
            let link = plan
                .profile_path
                .with_file_name(generation_link_name(&plan.profile_path, *id));
            let status = match fs::symlink_metadata(&link) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && command_succeeded => {
                    GenerationStatus::Removed
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    GenerationStatus::AbsentAfterFailure
                }
                Err(_) => GenerationStatus::Unknown,
                Ok(_) if command_succeeded => GenerationStatus::RetainedOrNotDeleted,
                Ok(_) => GenerationStatus::RetainedOrNotDeleted,
            };
            GenerationApplyStatus { id: *id, status }
        })
        .collect();
    Ok(GenerationApplyResult {
        command_succeeded,
        preservation_verified,
        statuses,
    })
}

fn snapshot_profile_for_plan(
    runner: &mut impl GenerationRunner,
    plan: &NixGenerationActionPlan,
    endpoint: &str,
    fixture_root: Option<&Path>,
) -> Result<ProfileSnapshot, GenerationError> {
    let user = current_user()?;
    let profile = match plan.kind {
        NixProfileKind::User => NixProfileConfig::User {
            path: plan.profile_path.clone(),
        },
        NixProfileKind::HomeManager => NixProfileConfig::HomeManager {
            path: plan.profile_path.clone(),
            current_home_roots: plan
                .activation_roots
                .iter()
                .map(|root| root.path.clone())
                .collect(),
        },
    };
    let identity = runner.identity(endpoint)?;
    if identity != plan.identity {
        return Err(GenerationError::Stale);
    }
    snapshot_profile(
        &profile,
        plan.profile_path.clone(),
        identity,
        &user,
        fixture_root,
    )
}

fn preservation_matches(plan: &NixGenerationActionPlan, after: &ProfileSnapshot) -> bool {
    if after.uid != plan.uid
        || after.profile_path != plan.profile_path
        || after.parent_identity != plan.parent_identity
        || after.current_generation != plan.current_generation
        || after.current_link_identity != plan.current_link_identity
        || after.current_link_target != plan.current_link_target
        || after.activation_roots != plan.activation_roots
    {
        return false;
    }
    for before in &plan.history {
        if plan.selected_ids.contains(&before.id) {
            if fs::symlink_metadata(&before.target)
                .ok()
                .is_none_or(|metadata| {
                    metadata.file_type().is_symlink()
                        || file_identity(&metadata) != before.target_identity
                })
            {
                return false;
            }
        } else if !after.history.contains(before) {
            return false;
        }
    }
    true
}

fn snapshot_profile(
    profile: &NixProfileConfig,
    profile_path: PathBuf,
    identity: GenerationIdentity,
    user: &UserIdentity,
    fixture_root: Option<&Path>,
) -> Result<ProfileSnapshot, GenerationError> {
    validate_profile_directory(&profile_path, user, fixture_root)?;
    let parent = profile_path.parent().ok_or(GenerationError::UnsafePath)?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|_| GenerationError::UnsafePath)?;
    let parent_identity = file_identity(&parent_metadata);
    let current_metadata =
        fs::symlink_metadata(&profile_path).map_err(|_| GenerationError::UnsafePath)?;
    if !current_metadata.file_type().is_symlink() || current_metadata.uid() != user.uid {
        return Err(GenerationError::UnsafePath);
    }
    let current_link_target =
        fs::read_link(&profile_path).map_err(|_| GenerationError::UnsafePath)?;
    let current_generation = parse_generation_link(&profile_path, &current_link_target)?;
    let history = read_history(&profile_path, &identity.store_dir, user.uid)?;
    let current = history
        .iter()
        .find(|item| item.id == current_generation)
        .ok_or(GenerationError::UnsafePath)?;
    let roots = read_activation_roots(profile, &identity.store_dir, user, fixture_root, current)?;
    Ok(ProfileSnapshot {
        identity,
        uid: user.uid,
        profile_path,
        parent_identity,
        current_link_identity: file_identity(&current_metadata),
        current_link_target: current_link_target
            .into_os_string()
            .into_string()
            .map_err(|_| GenerationError::UnsafePath)?,
        current_generation,
        history,
        activation_roots: roots,
    })
}

fn read_history(
    profile_path: &Path,
    store_dir: &Path,
    uid: u32,
) -> Result<Vec<GenerationSnapshot>, GenerationError> {
    let parent = profile_path.parent().ok_or(GenerationError::UnsafePath)?;
    let leaf = profile_path
        .file_name()
        .ok_or(GenerationError::UnsafePath)?
        .to_string_lossy();
    let prefix = format!("{leaf}-");
    let mut history = Vec::new();
    let mut entries = 0usize;
    for entry in fs::read_dir(parent).map_err(|_| GenerationError::UnsafePath)? {
        entries += 1;
        if entries > MAX_PROFILE_ENTRIES {
            return Err(GenerationError::InvalidOutput);
        }
        let entry = entry.map_err(|_| GenerationError::UnsafePath)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with("-link") {
            continue;
        }
        let number = &name[prefix.len()..name.len() - "-link".len()];
        if number.is_empty()
            || !number.bytes().all(|byte| byte.is_ascii_digit())
            || number.starts_with('0')
        {
            return Err(GenerationError::InvalidOutput);
        }
        let id = number
            .parse::<u32>()
            .map_err(|_| GenerationError::InvalidOutput)?;
        let link = entry.path();
        let link_metadata = fs::symlink_metadata(&link).map_err(|_| GenerationError::UnsafePath)?;
        if !link_metadata.file_type().is_symlink() || link_metadata.uid() != uid {
            return Err(GenerationError::UnsafePath);
        }
        let raw_target = fs::read_link(&link).map_err(|_| GenerationError::UnsafePath)?;
        if !raw_target.is_absolute() || !top_store_path(&raw_target, store_dir) {
            return Err(GenerationError::UnsafePath);
        }
        let target_metadata =
            fs::symlink_metadata(&raw_target).map_err(|_| GenerationError::UnsafePath)?;
        if target_metadata.file_type().is_symlink()
            || !(target_metadata.is_file() || target_metadata.is_dir())
            || target_metadata.mode() & 0o222 != 0
            || fs::canonicalize(&raw_target).map_err(|_| GenerationError::UnsafePath)? != raw_target
        {
            return Err(GenerationError::UnsafePath);
        }
        history.push(GenerationSnapshot {
            id,
            link,
            link_identity: file_identity(&link_metadata),
            raw_target: raw_target.clone(),
            target: raw_target,
            target_identity: file_identity(&target_metadata),
        });
        if history.len() > MAX_GENERATIONS {
            return Err(GenerationError::InvalidOutput);
        }
    }
    history.sort_by_key(|item| item.id);
    if history.windows(2).any(|pair| pair[0].id == pair[1].id)
        || history
            .iter()
            .map(|item| &item.target)
            .collect::<BTreeSet<_>>()
            .len()
            != history.len()
    {
        return Err(GenerationError::InvalidOutput);
    }
    Ok(history)
}

fn read_activation_roots(
    profile: &NixProfileConfig,
    store_dir: &Path,
    user: &UserIdentity,
    fixture_root: Option<&Path>,
    current: &GenerationSnapshot,
) -> Result<Vec<ActivationRootSnapshot>, GenerationError> {
    let NixProfileConfig::HomeManager {
        current_home_roots, ..
    } = profile
    else {
        return Ok(Vec::new());
    };
    let supported = supported_home_roots(user);
    if fixture_root.is_none() && current_home_roots != &supported {
        return Err(GenerationError::UnsafePath);
    }
    if fixture_root.is_some()
        && (current_home_roots.is_empty()
            || current_home_roots
                .iter()
                .any(|path| !path.starts_with(fixture_root.unwrap())))
    {
        return Err(GenerationError::UnsafePath);
    }
    let mut snapshots = Vec::new();
    let mut present_targets = BTreeSet::new();
    for root in current_home_roots {
        validate_root_parent(root, user, fixture_root)?;
        match fs::symlink_metadata(root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                snapshots.push(ActivationRootSnapshot {
                    path: root.clone(),
                    state: ActivationRootState::Absent,
                })
            }
            Err(_) => return Err(GenerationError::UnsafePath),
            Ok(metadata) => {
                if !metadata.file_type().is_symlink() || metadata.uid() != user.uid {
                    return Err(GenerationError::UnsafePath);
                }
                let raw_target = fs::read_link(root).map_err(|_| GenerationError::UnsafePath)?;
                if !raw_target.is_absolute() || !top_store_path(&raw_target, store_dir) {
                    return Err(GenerationError::UnsafePath);
                }
                let target_metadata =
                    fs::symlink_metadata(&raw_target).map_err(|_| GenerationError::UnsafePath)?;
                if target_metadata.file_type().is_symlink()
                    || !target_metadata.is_dir()
                    || target_metadata.mode() & 0o222 != 0
                    || fs::canonicalize(&raw_target).map_err(|_| GenerationError::UnsafePath)?
                        != raw_target
                    || raw_target != current.target
                {
                    return Err(GenerationError::UnsafePath);
                }
                present_targets.insert(raw_target.clone());
                snapshots.push(ActivationRootSnapshot {
                    path: root.clone(),
                    state: ActivationRootState::Present {
                        link_identity: file_identity(&metadata),
                        raw_target: raw_target.clone(),
                        target: raw_target,
                        target_identity: file_identity(&target_metadata),
                    },
                });
            }
        }
    }
    if snapshots.is_empty() || present_targets.len() != 1 {
        return Err(GenerationError::UnsafePath);
    }
    Ok(snapshots)
}

fn validate_profile_path(
    profile: &NixProfileConfig,
    user: &UserIdentity,
    fixture_root: Option<&Path>,
) -> Result<PathBuf, GenerationError> {
    let path = profile.path();
    if !valid_profile_leaf(path, profile.kind()) {
        return Err(GenerationError::UnsafePath);
    }
    let expected = user
        .home
        .join(".local/state/nix/profiles")
        .join(path.file_name().ok_or(GenerationError::UnsafePath)?);
    if fixture_root.is_none() && path != expected {
        return Err(GenerationError::UnsafePath);
    }
    if let Some(root) = fixture_root
        && (!path.starts_with(root) || !path.is_absolute())
    {
        return Err(GenerationError::UnsafePath);
    }
    validate_profile_directory(path, user, fixture_root)?;
    let parent = path.parent().ok_or(GenerationError::UnsafePath)?;
    let canonical_parent = fs::canonicalize(parent).map_err(|_| GenerationError::UnsafePath)?;
    if canonical_parent != parent {
        return Err(GenerationError::UnsafePath);
    }
    Ok(canonical_parent.join(path.file_name().ok_or(GenerationError::UnsafePath)?))
}

fn validate_profile_directory(
    profile_path: &Path,
    user: &UserIdentity,
    fixture_root: Option<&Path>,
) -> Result<(), GenerationError> {
    let parent = profile_path.parent().ok_or(GenerationError::UnsafePath)?;
    validate_directory_ancestry(parent, user, fixture_root)
}

fn validate_root_parent(
    root: &Path,
    user: &UserIdentity,
    fixture_root: Option<&Path>,
) -> Result<(), GenerationError> {
    if !root.is_absolute()
        || root.file_name().is_none_or(|leaf| leaf != "current-home")
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || fixture_root.is_some_and(|base| !root.starts_with(base))
    {
        return Err(GenerationError::UnsafePath);
    }
    let parent = root.parent().ok_or(GenerationError::UnsafePath)?;
    let mut existing = parent;
    loop {
        match fs::symlink_metadata(existing) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(GenerationError::UnsafePath);
                }
                validate_directory_ancestry(existing, user, fixture_root)?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or(GenerationError::UnsafePath)?;
            }
            Err(_) => return Err(GenerationError::UnsafePath),
        }
    }
}

fn validate_directory_ancestry(
    path: &Path,
    user: &UserIdentity,
    fixture_root: Option<&Path>,
) -> Result<(), GenerationError> {
    if !path.is_absolute()
        || fixture_root.is_some_and(|root| !path.starts_with(root))
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(GenerationError::UnsafePath);
    }
    let global_user_roots = Path::new("/nix/var/nix/gcroots/per-user").join(&user.username);
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push("/"),
            Component::Normal(part) => current.push(part),
            _ => return Err(GenerationError::UnsafePath),
        }
        let metadata = fs::symlink_metadata(&current).map_err(|_| GenerationError::UnsafePath)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || fs::canonicalize(&current).map_err(|_| GenerationError::UnsafePath)? != current
        {
            return Err(GenerationError::UnsafePath);
        }
        let under_fixture = fixture_root.is_some_and(|root| current.starts_with(root));
        let user_owned = under_fixture
            || current.starts_with(&user.home)
            || current.starts_with(&global_user_roots);
        let owner_is_safe = if user_owned {
            metadata.uid() == user.uid
        } else if fixture_root.is_some() {
            metadata.uid() == 0 || metadata.uid() == user.uid
        } else {
            metadata.uid() == 0
        };
        if !owner_is_safe {
            return Err(GenerationError::UnsafePath);
        }
        let mode = metadata.mode();
        let safe_sticky_fixture_ancestor =
            fixture_root.is_some() && !under_fixture && metadata.uid() == 0 && mode & 0o1000 != 0;
        if mode & 0o022 != 0 && !safe_sticky_fixture_ancestor {
            return Err(GenerationError::UnsafePath);
        }
    }
    Ok(())
}

fn supported_home_roots(user: &UserIdentity) -> Vec<PathBuf> {
    vec![
        user.home
            .join(".local/share/home-manager/gcroots/current-home"),
        Path::new("/nix/var/nix/gcroots/per-user")
            .join(&user.username)
            .join("current-home"),
    ]
}

fn valid_profile_leaf(path: &Path, kind: NixProfileKind) -> bool {
    let expected = match kind {
        NixProfileKind::User => "profile",
        NixProfileKind::HomeManager => "home-manager",
    };
    path.is_absolute()
        && path.file_name().is_some_and(|leaf| leaf == expected)
        && path
            .parent()
            .is_some_and(|parent| parent.file_name().is_some_and(|leaf| leaf == "profiles"))
        && path
            .parent()
            .and_then(Path::parent)
            .is_some_and(|parent| parent.file_name().is_some_and(|leaf| leaf == "nix"))
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

fn parse_generation_link(profile: &Path, target: &Path) -> Result<u32, GenerationError> {
    if target.components().count() != 1 {
        return Err(GenerationError::UnsafePath);
    }
    let target = target.to_str().ok_or(GenerationError::UnsafePath)?;
    let prefix = format!(
        "{}-",
        profile
            .file_name()
            .ok_or(GenerationError::UnsafePath)?
            .to_string_lossy()
    );
    let number = target
        .strip_prefix(&prefix)
        .and_then(|name| name.strip_suffix("-link"))
        .ok_or(GenerationError::UnsafePath)?;
    if number.is_empty()
        || number.starts_with('0')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(GenerationError::UnsafePath);
    }
    number
        .parse::<u32>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or(GenerationError::UnsafePath)
}

fn generation_link_name(profile: &Path, id: u32) -> String {
    format!(
        "{}-{id}-link",
        profile.file_name().unwrap_or_default().to_string_lossy()
    )
}

fn current_user() -> Result<UserIdentity, GenerationError> {
    let uid = unsafe { libc::getuid() };
    let euid = unsafe { libc::geteuid() };
    if uid == 0 || euid == 0 || uid != euid {
        return Err(GenerationError::UnsafePath);
    }
    let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0u8; 16 * 1024];
    let mut result = std::ptr::null_mut();
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            passwd.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if code != 0 || result.is_null() {
        return Err(GenerationError::UnsafePath);
    }
    let passwd = unsafe { passwd.assume_init() };
    if passwd.pw_dir.is_null() || passwd.pw_name.is_null() {
        return Err(GenerationError::UnsafePath);
    }
    let home = PathBuf::from(OsStr::from_bytes(
        unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes(),
    ));
    let username = unsafe { CStr::from_ptr(passwd.pw_name) }
        .to_str()
        .map_err(|_| GenerationError::UnsafePath)?
        .to_owned();
    let home = fs::canonicalize(home).map_err(|_| GenerationError::UnsafePath)?;
    let metadata = fs::symlink_metadata(&home).map_err(|_| GenerationError::UnsafePath)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(GenerationError::UnsafePath);
    }
    Ok(UserIdentity {
        uid,
        home,
        username,
    })
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode() & 0o777,
    }
}

fn command(program: &str, args: &[&str]) -> Result<Vec<u8>, GenerationError> {
    let mut command = Command::new(program);
    command.args(args);
    run_bounded(command)
}

fn run_bounded(mut command: Command) -> Result<Vec<u8>, GenerationError> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| GenerationError::Command)?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(GenerationError::Command);
    };
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take((MAX_OUTPUT + 1) as u64).read_to_end(&mut bytes);
        (result.is_ok(), bytes)
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (read_ok, bytes) = reader.join().map_err(|_| GenerationError::Command)?;
                if !read_ok || bytes.len() > MAX_OUTPUT || !status.success() {
                    return Err(GenerationError::Command);
                }
                return Ok(bytes);
            }
            Ok(None) if start.elapsed() <= Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(GenerationError::Command);
            }
        }
    }
}

fn one_line(bytes: Vec<u8>) -> Result<String, GenerationError> {
    let text = String::from_utf8(bytes).map_err(|_| GenerationError::InvalidOutput)?;
    let text = text.trim_end_matches(['\n', '\r']);
    if text.is_empty() || text.contains(['\n', '\r']) || text.chars().any(char::is_control) {
        return Err(GenerationError::InvalidOutput);
    }
    Ok(text.to_owned())
}

#[cfg(feature = "nix-local-test-support")]
#[doc(hidden)]
pub mod local_test_support {
    use super::*;

    pub fn command_runner(
        endpoint: &str,
        fixture_root: &Path,
    ) -> Result<CommandGenerationRunner, GenerationError> {
        let canonical = fs::canonicalize(fixture_root).map_err(|_| GenerationError::UnsafePath)?;
        let temp_root =
            fs::canonicalize(std::env::temp_dir()).map_err(|_| GenerationError::UnsafePath)?;
        let expected = format!(
            "local?store={}&state={}/state&log={}/log",
            canonical.join("store").display(),
            canonical.display(),
            canonical.display()
        );
        if canonical != fixture_root
            || canonical == temp_root
            || !canonical.starts_with(temp_root)
            || endpoint != expected
        {
            return Err(GenerationError::UnsafePath);
        }
        Ok(CommandGenerationRunner {
            fixture_endpoint: Some(endpoint.to_owned()),
        })
    }

    pub fn create_plan(
        runner: &mut impl GenerationRunner,
        config: &Config,
        cleanup_profile: CleanupProfile,
        profile_name: &str,
        selected_ids: &[u32],
        endpoint: &str,
        fixture_root: &Path,
    ) -> Result<NixGenerationActionPlan, GenerationError> {
        checked_fixture(endpoint, fixture_root, config)?;
        create_plan_at(
            runner,
            config,
            cleanup_profile,
            profile_name,
            selected_ids,
            endpoint,
            Some(fixture_root),
        )
    }

    pub fn inspect_profile(
        runner: &mut impl GenerationRunner,
        config: &Config,
        cleanup_profile: CleanupProfile,
        profile_name: &str,
        selected_ids: &[u32],
        fixture_root: &Path,
    ) -> Result<crate::nix_inspection::NixGenerationInspection, GenerationError> {
        let endpoint = format!(
            "local?store={}&state={}/state&log={}/log",
            fixture_root.join("store").display(),
            fixture_root.display(),
            fixture_root.display()
        );
        checked_fixture(&endpoint, fixture_root, config)?;
        inspect_profile_at(
            runner,
            config,
            cleanup_profile,
            profile_name,
            selected_ids,
            &endpoint,
            Some(fixture_root),
        )
    }

    pub fn apply(
        runner: &mut impl GenerationRunner,
        config: &Config,
        cleanup_profile: CleanupProfile,
        plan: &NixGenerationActionPlan,
        endpoint: &str,
        fixture_root: &Path,
        confirm: impl FnOnce(&NixGenerationActionPlan) -> Result<bool, GenerationError>,
    ) -> Result<GenerationApplyResult, GenerationError> {
        checked_fixture(endpoint, fixture_root, config)?;
        apply_at(
            runner,
            config,
            cleanup_profile,
            plan,
            endpoint,
            Some(fixture_root),
            confirm,
        )
    }

    fn checked_fixture(
        endpoint: &str,
        root: &Path,
        config: &Config,
    ) -> Result<(), GenerationError> {
        let canonical = fs::canonicalize(root).map_err(|_| GenerationError::UnsafePath)?;
        let temp_root =
            fs::canonicalize(std::env::temp_dir()).map_err(|_| GenerationError::UnsafePath)?;
        if !canonical.is_dir()
            || canonical != root
            || !canonical.starts_with(&temp_root)
            || canonical == temp_root
            || endpoint
                != format!(
                    "local?store={}&state={}/state&log={}/log",
                    canonical.join("store").display(),
                    canonical.display(),
                    canonical.display()
                )
        {
            return Err(GenerationError::UnsafePath);
        }
        for profile in config.nix_profiles().values() {
            if !profile.path().starts_with(&canonical)
                || profile
                    .current_home_roots()
                    .iter()
                    .any(|path| !path.starts_with(&canonical))
            {
                return Err(GenerationError::UnsafePath);
            }
        }
        Ok(())
    }
}
