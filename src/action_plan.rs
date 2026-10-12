use crate::{CleanupProfile, config::Category};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
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
    let plan: NixStoreActionPlan = serde_json::from_slice(&bytes).map_err(|_| "invalid plan")?;
    plan.validate()?;
    Ok(plan)
}
