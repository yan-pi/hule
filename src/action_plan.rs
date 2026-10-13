use crate::{
    CleanupProfile,
    config::{Category, NixProfileKind},
};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub const MAX_PLAN_BYTES: u64 = 256 * 1024;
pub const MAX_INPUTS: usize = 32;
pub const MAX_CLOSURE_PATHS: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NixStoreActionPlan {
    pub format: String,
    pub schema_version: u8,
    pub profile: CleanupProfile,
    pub category: Category,
    pub inputs: Vec<PathBuf>,
    pub closure: Vec<PathBuf>,
    pub identity: StoreIdentity,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoreIdentity {
    pub endpoint: String,
    pub version: String,
    pub store_dir: PathBuf,
}

pub const MAX_GENERATION_IDS: usize = 32;
pub const MAX_GENERATIONS: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DockerFileIdentity {
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub size: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DockerDirectoryIdentity {
    pub path: PathBuf,
    pub identity: FileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationSnapshot {
    pub id: u32,
    pub link: PathBuf,
    pub link_identity: FileIdentity,
    pub raw_target: PathBuf,
    pub target: PathBuf,
    pub target_identity: FileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationIdentity {
    pub endpoint: String,
    pub store_version: String,
    pub nix_env_version: String,
    pub store_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivationRootSnapshot {
    pub path: PathBuf,
    pub state: ActivationRootState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActivationRootState {
    Absent,
    Present {
        link_identity: FileIdentity,
        raw_target: PathBuf,
        target: PathBuf,
        target_identity: FileIdentity,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NixGenerationActionPlan {
    pub format: String,
    pub schema_version: u8,
    pub profile: CleanupProfile,
    pub category: Category,
    pub nix_profile: String,
    pub kind: NixProfileKind,
    pub profile_path: PathBuf,
    pub identity: GenerationIdentity,
    pub uid: u32,
    pub parent_identity: FileIdentity,
    pub current_link_identity: FileIdentity,
    pub current_link_target: String,
    pub history: Vec<GenerationSnapshot>,
    pub current_generation: u32,
    pub selected_ids: Vec<u32>,
    pub activation_roots: Vec<ActivationRootSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DockerPurgeActionPlan {
    pub format: String,
    pub schema_version: u8,
    pub profile: CleanupProfile,
    pub category: Category,
    pub endpoint: String,
    pub endpoint_identity: DockerFileIdentity,
    pub endpoint_parent_identities: Vec<DockerDirectoryIdentity>,
    pub identity: DockerPurgeIdentity,
    pub scope: DockerPurgeScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DockerPurgeIdentity {
    pub docker_cli_path: PathBuf,
    pub docker_cli_file_identity: DockerFileIdentity,
    pub docker_cli_parent_identities: Vec<DockerDirectoryIdentity>,
    pub docker_cli_version: String,
    pub engine_id: String,
    pub daemon_data_root: PathBuf,
    pub engine_version: String,
    pub client_api_version: String,
    pub server_api_version: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DockerPurgeScope {
    SystemPruneAllWithAnonymousVolumes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionPlan {
    NixStore(NixStoreActionPlan),
    NixGenerations(NixGenerationActionPlan),
    DockerPurge(DockerPurgeActionPlan),
}

impl NixGenerationActionPlan {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != "hule-nix-generation-action"
            || self.schema_version != 1
            || self.identity.endpoint != "daemon"
            || self.nix_profile.is_empty()
            || !self
                .nix_profile
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            || self.uid == 0
            || self.parent_identity.uid != self.uid
            || self.current_link_identity.uid != self.uid
            || self.parent_identity.mode & 0o022 != 0
            || !valid_store_dir(&self.identity.store_dir)
            || self.identity.store_version != "nix-store (Nix) 2.34.6"
            || self.identity.nix_env_version != "nix-env (Nix) 2.34.6"
            || self.identity.store_version.chars().any(char::is_control)
            || self.identity.nix_env_version.chars().any(char::is_control)
            || !valid_profile_path(&self.profile_path, self.kind)
        {
            return Err("invalid plan");
        }
        let expected_category = match self.kind {
            NixProfileKind::User => Category::NixUserGenerations,
            NixProfileKind::HomeManager => Category::NixHomeManagerGenerations,
        };
        if self.category != expected_category
            || self.history.is_empty()
            || self.history.len() > MAX_GENERATIONS
            || self.selected_ids.is_empty()
            || self.selected_ids.len() > MAX_GENERATION_IDS
            || self.current_generation == 0
            || self.current_link_target
                != generation_link_name(&self.profile_path, self.current_generation)
            || self.history.windows(2).any(|pair| pair[0].id >= pair[1].id)
            || self.selected_ids.windows(2).any(|pair| pair[0] >= pair[1])
            || self.selected_ids.iter().any(|id| *id > i32::MAX as u32)
        {
            return Err("invalid plan");
        }
        let current = self
            .history
            .iter()
            .find(|item| item.id == self.current_generation)
            .ok_or("invalid plan")?;
        if self.history.iter().any(|item| {
            item.id == 0
                || item.id > i32::MAX as u32
                || item.link
                    != self
                        .profile_path
                        .with_file_name(generation_link_name(&self.profile_path, item.id))
                || !top_store_path(&item.target, &self.identity.store_dir)
                || item.raw_target != item.target
                || item.link_identity.uid != self.uid
                || item.target_identity.mode & 0o022 != 0
        }) {
            return Err("invalid plan");
        }
        let targets = self
            .history
            .iter()
            .map(|item| &item.target)
            .collect::<BTreeSet<_>>();
        if targets.len() != self.history.len()
            || self.selected_ids.iter().any(|id| {
                *id == self.current_generation || !self.history.iter().any(|item| item.id == *id)
            })
        {
            return Err("invalid plan");
        }
        match self.kind {
            NixProfileKind::User if !self.activation_roots.is_empty() => {
                return Err("invalid plan");
            }
            NixProfileKind::HomeManager => {
                if self.activation_roots.is_empty()
                    || self
                        .activation_roots
                        .iter()
                        .map(|root| &root.path)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != self.activation_roots.len()
                    || !self
                        .activation_roots
                        .iter()
                        .any(|root| matches!(&root.state, ActivationRootState::Present { .. }))
                {
                    return Err("invalid plan");
                }
                for root in &self.activation_roots {
                    if !valid_absolute_path(&root.path) {
                        return Err("invalid plan");
                    }
                    if let ActivationRootState::Present {
                        link_identity,
                        raw_target,
                        target,
                        target_identity,
                    } = &root.state
                        && (link_identity.uid != self.uid
                            || !top_store_path(target, &self.identity.store_dir)
                            || raw_target != target
                            || target_identity.mode & 0o022 != 0
                            || target != &current.target)
                    {
                        return Err("invalid plan");
                    }
                }
            }
            NixProfileKind::User => {}
        }
        Ok(())
    }
}

impl DockerPurgeActionPlan {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != "hule-docker-purge-action"
            || self.schema_version != 1
            || self.profile != CleanupProfile::Aggressive
            || self.category != Category::DockerPurge
            || !valid_docker_socket_endpoint(&self.endpoint)
            || self.endpoint_identity.mode & 0o170000 != 0o140000
            || !trusted_docker_owner(self.endpoint_identity.uid)
            || !valid_docker_parent_identities(
                Path::new(self.endpoint.strip_prefix("unix://").unwrap_or_default()),
                &self.endpoint_parent_identities,
            )
            || !valid_absolute_normalized_path(&self.identity.docker_cli_path)
            || self.identity.docker_cli_path.file_name() != Some(std::ffi::OsStr::new("docker"))
            || !valid_docker_cli_file_identity(&self.identity.docker_cli_file_identity)
            || !valid_docker_parent_identities(
                &self.identity.docker_cli_path,
                &self.identity.docker_cli_parent_identities,
            )
            || !valid_version_string(&self.identity.docker_cli_version)
            || !valid_version_string(&self.identity.engine_id)
            || !valid_absolute_normalized_path(&self.identity.daemon_data_root)
            || !valid_version_string(&self.identity.engine_version)
            || !valid_api_version(&self.identity.client_api_version)
            || !valid_api_version(&self.identity.server_api_version)
        {
            return Err("invalid plan");
        }
        Ok(())
    }
}

fn valid_docker_cli_file_identity(identity: &DockerFileIdentity) -> bool {
    identity.mode & 0o170000 == 0o100000
        && identity.mode & 0o111 != 0
        && identity.mode & 0o022 == 0
        && trusted_docker_owner(identity.uid)
}

fn trusted_docker_owner(uid: u32) -> bool {
    uid == 0 || uid == unsafe { libc::geteuid() }
}

fn valid_docker_parent_identities(
    file_path: &Path,
    identities: &[DockerDirectoryIdentity],
) -> bool {
    let Some(parent) = file_path.parent() else {
        return false;
    };
    let mut expected = vec![PathBuf::from("/")];
    let mut current = PathBuf::from("/");
    for component in parent.components().skip(1) {
        let Component::Normal(name) = component else {
            return false;
        };
        current.push(name);
        expected.push(current.clone());
    }
    identities.len() == expected.len()
        && identities.iter().zip(expected).all(|(snapshot, path)| {
            let mode = snapshot.identity.mode;
            let safe_permissions =
                mode & 0o022 == 0 || (snapshot.identity.uid == 0 && mode & 0o1000 != 0);
            snapshot.path == path
                && trusted_docker_owner(snapshot.identity.uid)
                && (snapshot.path == Path::new("/")
                    || valid_absolute_normalized_path(&snapshot.path))
                && mode & 0o170000 == 0o040000
                && safe_permissions
        })
}

pub(crate) fn valid_docker_socket_endpoint(endpoint: &str) -> bool {
    let Some(path_text) = endpoint.strip_prefix("unix://") else {
        return false;
    };
    if !path_text.starts_with('/')
        || path_text.contains(['%', '?', '#'])
        || endpoint.chars().any(char::is_control)
    {
        return false;
    }
    let path = Path::new(path_text);
    path != Path::new("/")
        && path.is_absolute()
        && path.to_string_lossy() == path_text
        && path == path.components().collect::<PathBuf>()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn valid_absolute_normalized_path(path: &Path) -> bool {
    path != Path::new("/")
        && path.is_absolute()
        && path == path.components().collect::<PathBuf>()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        && !path.to_string_lossy().chars().any(char::is_control)
}

fn valid_version_string(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.is_ascii()
        && value.bytes().all(|byte| !byte.is_ascii_control())
}

fn valid_api_version(value: &str) -> bool {
    let mut components = value.split('.');
    let Some(major_text) = components.next() else {
        return false;
    };
    let Some(minor_text) = components.next() else {
        return false;
    };
    let (Ok(major), Ok(minor)) = (major_text.parse::<u32>(), minor_text.parse::<u32>()) else {
        return false;
    };
    components.next().is_none()
        && major == 1
        && minor >= 42
        && major_text == major.to_string()
        && minor_text == minor.to_string()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

impl NixStoreActionPlan {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_for_endpoint("daemon")
    }

    pub(crate) fn validate_for_endpoint(&self, endpoint: &str) -> Result<(), &'static str> {
        if self.format != "hule-nix-store-action"
            || self.schema_version != 1
            || self.category != Category::NixStoreGc
            || self.identity.endpoint != endpoint
        {
            return Err("invalid plan");
        }
        if self.inputs.is_empty()
            || self.inputs.len() > MAX_INPUTS
            || self.closure.is_empty()
            || self.closure.len() > MAX_CLOSURE_PATHS
        {
            return Err("invalid plan");
        }
        if !valid_store_dir(&self.identity.store_dir)
            || self.identity.version.is_empty()
            || self.identity.version.len() > 128
            || self.identity.version.chars().any(char::is_control)
        {
            return Err("invalid plan");
        }
        let inputs = self.inputs.iter().collect::<BTreeSet<_>>();
        let closure = self.closure.iter().collect::<BTreeSet<_>>();
        if inputs.len() != self.inputs.len()
            || closure.len() != self.closure.len()
            || !inputs.is_subset(&closure)
        {
            return Err("invalid plan");
        }
        if self
            .inputs
            .iter()
            .chain(&self.closure)
            .any(|p| !top_store_path(p, &self.identity.store_dir))
        {
            return Err("invalid plan");
        }
        Ok(())
    }
}
pub fn top_store_path(path: &Path, store_dir: &Path) -> bool {
    path.is_absolute()
        && path.parent() == Some(store_dir)
        && path
            .file_name()
            .is_some_and(|n| !n.is_empty() && !n.to_string_lossy().chars().any(char::is_control))
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn valid_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        && !path.to_string_lossy().chars().any(char::is_control)
}

fn valid_profile_path(path: &Path, kind: NixProfileKind) -> bool {
    if !valid_absolute_path(path) {
        return false;
    }
    let leaf = match kind {
        NixProfileKind::User => "profile",
        NixProfileKind::HomeManager => "home-manager",
    };
    path.file_name().is_some_and(|value| value == leaf)
        && path
            .parent()
            .is_some_and(|parent| parent.file_name().is_some_and(|value| value == "profiles"))
        && path
            .parent()
            .and_then(Path::parent)
            .is_some_and(|parent| parent.file_name().is_some_and(|value| value == "nix"))
}

fn generation_link_name(profile: &Path, id: u32) -> String {
    format!(
        "{}-{id}-link",
        profile.file_name().unwrap_or_default().to_string_lossy()
    )
}
fn valid_store_dir(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}
pub fn write_new(path: &Path, plan: &NixStoreActionPlan) -> Result<(), &'static str> {
    plan.validate()?;
    validate_output_location(path, &plan.identity.store_dir)?;
    let bytes = serde_json::to_vec_pretty(plan).map_err(|_| "could not save plan")?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err("invalid plan");
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| "could not create plan")?;
    file.write_all(&bytes)
        .map_err(|_| "could not create plan")?;
    Ok(())
}
fn validate_output_location(path: &Path, store_dir: &Path) -> Result<(), &'static str> {
    let parent = path.parent().ok_or("invalid plan output path")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let file_name = path.file_name().ok_or("invalid plan output path")?;
    let canonical_parent = fs::canonicalize(parent).map_err(|_| "invalid plan output path")?;
    if !canonical_parent.is_dir() {
        return Err("invalid plan output path");
    }
    let canonical_store = fs::canonicalize(store_dir).map_err(|_| "invalid store directory")?;
    let output = canonical_parent.join(file_name);
    let lexical_output = normalize_absolute(path)?;
    let lexical_store = normalize_absolute(store_dir)?;
    if overlaps(&output, &canonical_store) || overlaps(&lexical_output, &lexical_store) {
        return Err("plan output must be outside the Nix store");
    }
    Ok(())
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, &'static str> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| "invalid plan output path")?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

pub fn read_plan(path: &Path) -> Result<NixStoreActionPlan, &'static str> {
    let bytes = read_bounded_plan(path)?;
    let plan: NixStoreActionPlan = serde_json::from_slice(&bytes).map_err(|_| "invalid plan")?;
    plan.validate()?;
    Ok(plan)
}

pub fn write_generation_new(
    path: &Path,
    plan: &NixGenerationActionPlan,
) -> Result<(), &'static str> {
    plan.validate()?;
    validate_generation_output(path, plan)?;
    let bytes = serde_json::to_vec_pretty(plan).map_err(|_| "could not save plan")?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err("invalid plan");
    }
    write_private_new(path, &bytes)
}

pub fn write_docker_purge_new(
    path: &Path,
    plan: &DockerPurgeActionPlan,
) -> Result<(), &'static str> {
    plan.validate()?;
    validate_docker_purge_location(path, plan)?;
    let bytes = serde_json::to_vec_pretty(plan).map_err(|_| "could not save plan")?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err("invalid plan");
    }
    write_private_new(path, &bytes)
}

pub fn validate_docker_purge_location(
    path: &Path,
    plan: &DockerPurgeActionPlan,
) -> Result<(), &'static str> {
    plan.validate()?;
    validate_new_output_parent(path, &plan.identity.daemon_data_root)
}

fn validate_new_output_parent(path: &Path, daemon_data_root: &Path) -> Result<(), &'static str> {
    let parent = path.parent().ok_or("invalid plan output path")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let _file_name = path.file_name().ok_or("invalid plan output path")?;
    let canonical_parent = fs::canonicalize(parent).map_err(|_| "invalid plan output path")?;
    if !canonical_parent.is_dir() {
        return Err("invalid plan output path");
    }
    let canonical_output = canonical_parent.join(_file_name);
    if overlaps(&canonical_output, daemon_data_root) {
        return Err("invalid plan output path");
    }
    match fs::canonicalize(daemon_data_root) {
        Ok(canonical_root) => {
            if !canonical_root.is_dir() || overlaps(&canonical_output, &canonical_root) {
                return Err("invalid plan output path");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("invalid plan output path"),
    }
    Ok(())
}

pub fn read_action_plan(path: &Path) -> Result<ActionPlan, &'static str> {
    let bytes = read_bounded_plan(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "invalid plan")?;
    match value.get("format").and_then(serde_json::Value::as_str) {
        Some("hule-nix-store-action") => {
            let plan: NixStoreActionPlan =
                serde_json::from_value(value).map_err(|_| "invalid plan")?;
            plan.validate()?;
            Ok(ActionPlan::NixStore(plan))
        }
        Some("hule-nix-generation-action") => {
            let plan: NixGenerationActionPlan =
                serde_json::from_value(value).map_err(|_| "invalid plan")?;
            plan.validate()?;
            Ok(ActionPlan::NixGenerations(plan))
        }
        Some("hule-docker-purge-action") => {
            let plan: DockerPurgeActionPlan =
                serde_json::from_value(value).map_err(|_| "invalid plan")?;
            plan.validate()?;
            Ok(ActionPlan::DockerPurge(plan))
        }
        _ => Err("invalid plan"),
    }
}

fn read_bounded_plan(path: &Path) -> Result<Vec<u8>, &'static str> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "invalid plan")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_PLAN_BYTES {
        return Err("invalid plan");
    }
    let file = fs::File::open(path).map_err(|_| "invalid plan")?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "invalid plan")?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err("invalid plan");
    }
    reject_duplicate_json_keys(&bytes)?;
    Ok(bytes)
}

fn reject_duplicate_json_keys(bytes: &[u8]) -> Result<(), &'static str> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    DuplicateKeyScan::deserialize(&mut deserializer).map_err(|_| "invalid plan")?;
    deserializer.end().map_err(|_| "invalid plan")
}

struct DuplicateKeyScan;

impl<'de> Deserialize<'de> for DuplicateKeyScan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateKeyVisitor)
    }
}

struct DuplicateKeyVisitor;

impl<'de> Visitor<'de> for DuplicateKeyVisitor {
    type Value = DuplicateKeyScan;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value with unique object keys")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_bytes<E>(self, _: &[u8]) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_byte_buf<E>(self, _: Vec<u8>) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateKeyScan)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        DuplicateKeyScan::deserialize(deserializer)
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        DuplicateKeyScan::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<DuplicateKeyScan>()?.is_some() {}
        Ok(DuplicateKeyScan)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            map.next_value::<DuplicateKeyScan>()?;
        }
        Ok(DuplicateKeyScan)
    }
}

fn validate_generation_output(
    path: &Path,
    plan: &NixGenerationActionPlan,
) -> Result<(), &'static str> {
    validate_output_location(path, &plan.identity.store_dir)?;
    let parent = path.parent().ok_or("invalid plan output path")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let canonical_parent = fs::canonicalize(parent).map_err(|_| "invalid plan output path")?;
    let output = canonical_parent.join(path.file_name().ok_or("invalid plan output path")?);
    let lexical_output = normalize_absolute(path)?;
    let mut protected = vec![
        plan.profile_path
            .parent()
            .ok_or("invalid plan")?
            .to_path_buf(),
    ];
    protected.extend(
        plan.activation_roots
            .iter()
            .filter_map(|root| root.path.parent().map(Path::to_path_buf)),
    );
    for directory in protected {
        let canonical = canonicalize_with_missing_tail(&directory)?;
        let lexical = normalize_absolute(&directory)?;
        if overlaps(&output, &canonical) || overlaps(&lexical_output, &lexical) {
            return Err("plan output must be outside generation profile and activation roots");
        }
    }
    Ok(())
}

fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf, &'static str> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if !missing.is_empty() && !metadata.is_dir() {
                    return Err("invalid plan output path");
                }
                let mut resolved =
                    fs::canonicalize(ancestor).map_err(|_| "invalid plan output path")?;
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or("invalid plan output path")?
                        .to_os_string(),
                );
                ancestor = ancestor.parent().ok_or("invalid plan output path")?;
            }
            Err(_) => return Err("invalid plan output path"),
        }
    }
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), &'static str> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| "could not create plan")?;
    file.write_all(bytes).map_err(|_| "could not create plan")?;
    Ok(())
}
