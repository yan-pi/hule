use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::ProjectMode;
use serde::{Deserialize, Serialize};

const STATE_VERSION: u32 = 1;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRegistry {
    projects: BTreeMap<String, RegisteredProject>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredProject {
    path: PathBuf,
    mode: ProjectMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateError {
    InvalidToml,
    UnsupportedVersion,
    InvalidProjectName,
    InvalidProjectPath,
    OverlappingProjectRoots,
    ProjectAlreadyRegistered,
    ProjectNotRegistered,
    InvalidStateLocation,
    UnsafeDestination,
    LockExists,
    Io,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistry {
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, RegisteredProject>,
}

impl ProjectRegistry {
    pub fn from_toml(input: &str) -> Result<Self, StateError> {
        let raw: RawRegistry = toml::from_str(input).map_err(|_| StateError::InvalidToml)?;
        if raw.version != STATE_VERSION {
            return Err(StateError::UnsupportedVersion);
        }
        validate_projects(raw.projects)
    }
    pub fn to_toml(&self) -> Result<String, StateError> {
        toml::to_string(&RawRegistry {
            version: STATE_VERSION,
            projects: self.projects.clone(),
        })
        .map_err(|_| StateError::InvalidToml)
    }
    pub fn list(&self) -> &BTreeMap<String, RegisteredProject> {
        &self.projects
    }
    pub fn get(&self, name: &str) -> Option<&RegisteredProject> {
        self.projects.get(name)
    }
    pub fn register(
        &mut self,
        name: String,
        path: PathBuf,
        mode: ProjectMode,
    ) -> Result<(), StateError> {
        if !valid_project_name(&name) {
            return Err(StateError::InvalidProjectName);
        }
        if !valid_project_path(&path) {
            return Err(StateError::InvalidProjectPath);
        }
        if self.projects.contains_key(&name) {
            return Err(StateError::ProjectAlreadyRegistered);
        }
        if self
            .projects
            .values()
            .any(|existing| path.starts_with(&existing.path) || existing.path.starts_with(&path))
        {
            return Err(StateError::OverlappingProjectRoots);
        }
        self.projects.insert(name, RegisteredProject { path, mode });
        Ok(())
    }
    pub fn unregister(&mut self, name: &str) -> Result<RegisteredProject, StateError> {
        self.projects
            .remove(name)
            .ok_or(StateError::ProjectNotRegistered)
    }
}
impl RegisteredProject {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn mode(&self) -> ProjectMode {
        self.mode
    }
}
fn validate_projects(
    projects: BTreeMap<String, RegisteredProject>,
) -> Result<ProjectRegistry, StateError> {
    let mut registry = ProjectRegistry::default();
    for (name, project) in projects {
        registry.register(name, project.path, project.mode)?;
    }
    Ok(registry)
}
fn valid_project_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}
fn valid_project_path(path: &Path) -> bool {
    path.is_absolute()
        && path.components().any(|c| matches!(c, Component::Normal(_)))
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        && !path.to_string_lossy().chars().any(char::is_control)
}

pub fn default_state_path() -> Result<PathBuf, StateError> {
    resolve_default_state_path(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}
fn resolve_default_state_path(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, StateError> {
    if let Some(value) = xdg_state_home {
        let path = PathBuf::from(value);
        if valid_state_root(&path) {
            return Ok(path.join("hule").join("projects.toml"));
        }
    }
    let home = PathBuf::from(home.ok_or(StateError::InvalidStateLocation)?);
    if !valid_state_root(&home) {
        return Err(StateError::InvalidStateLocation);
    }
    Ok(home.join(".local/state/hule/projects.toml"))
}
fn valid_state_root(path: &Path) -> bool {
    path.is_absolute()
        && !path.as_os_str().is_empty()
        && !path.components().any(|c| matches!(c, Component::ParentDir))
}

pub fn load(path: &Path) -> Result<ProjectRegistry, StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(StateError::UnsafeDestination);
        }
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectRegistry::default());
        }
        Err(_) => return Err(StateError::Io),
    }
    let contents = fs::read_to_string(path).map_err(|_| StateError::Io)?;
    ProjectRegistry::from_toml(&contents)
}

/// Replace the persisted registry with this complete snapshot.
///
/// Use [`update`] for read-modify-write operations so concurrent changes are
/// loaded and mutated while holding the advisory lock.
pub fn save(path: &Path, registry: &ProjectRegistry) -> Result<(), StateError> {
    let _guard = acquire_lock(path)?;
    write_registry(path, registry)
}
pub fn update<T>(
    path: &Path,
    mutate: impl FnOnce(&mut ProjectRegistry) -> Result<T, StateError>,
) -> Result<T, StateError> {
    let _guard = acquire_lock(path)?;
    let mut registry = load(path)?;
    let result = mutate(&mut registry)?;
    write_registry(path, &registry)?;
    Ok(result)
}
fn acquire_lock(path: &Path) -> Result<LockGuard, StateError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or(StateError::InvalidStateLocation)?;
    create_private_dir_all(parent).map_err(|_| StateError::Io)?;
    validate_lock_directory(parent)?;
    let lock_path = sibling_with_suffix(path, ".lock");
    let lock = open_lock_file(&lock_path)?;
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => StateError::LockExists,
        std::fs::TryLockError::Error(_) => StateError::Io,
    })?;
    validate_lock_path(&lock_path, &lock)?;
    Ok(LockGuard { _file: lock })
}
#[cfg(unix)]
fn validate_lock_directory(path: &Path) -> Result<(), StateError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).map_err(|_| StateError::Io)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
        return Err(StateError::UnsafeDestination);
    }
    Ok(())
}
#[cfg(not(unix))]
fn validate_lock_directory(path: &Path) -> Result<(), StateError> {
    if !fs::metadata(path).map_err(|_| StateError::Io)?.is_dir() {
        return Err(StateError::UnsafeDestination);
    }
    Ok(())
}
fn write_registry(path: &Path, registry: &ProjectRegistry) -> Result<(), StateError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or(StateError::InvalidStateLocation)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(StateError::UnsafeDestination);
        }
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err(StateError::Io),
    }
    let contents = registry.to_toml()?;
    let (temp_path, mut temp) = create_temp(parent, path)?;
    let result = (|| {
        temp.write_all(contents.as_bytes())
            .map_err(|_| StateError::Io)?;
        temp.sync_all().map_err(|_| StateError::Io)?;
        fs::rename(&temp_path, path).map_err(|_| StateError::Io)?;
        #[cfg(unix)]
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_private_dir_all(parent)?;
    }
    match create_private_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::metadata(path)?.is_dir() {
                Ok(())
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    set_private_directory_permissions(&mut builder);
    builder.create(path)
}
#[cfg(unix)]
fn set_private_directory_permissions(builder: &mut fs::DirBuilder) {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
}
#[cfg(not(unix))]
fn set_private_directory_permissions(_builder: &mut fs::DirBuilder) {}
fn open_lock_file(path: &Path) -> Result<File, StateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(StateError::UnsafeDestination);
        }
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err(StateError::Io),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    set_private_permissions(&mut options);
    let file = options.open(path).map_err(|_| StateError::Io)?;
    validate_lock_path(path, &file)?;
    Ok(file)
}
fn validate_lock_path(path: &Path, file: &File) -> Result<(), StateError> {
    let handle = file.metadata().map_err(|_| StateError::Io)?;
    let entry = fs::symlink_metadata(path).map_err(|_| StateError::Io)?;
    if entry.file_type().is_symlink() || !entry.is_file() || !same_file(&handle, &entry) {
        return Err(StateError::UnsafeDestination);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if handle.mode() & 0o077 != 0 {
            return Err(StateError::UnsafeDestination);
        }
    }
    Ok(())
}
#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(windows)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    a.volume_serial_number().is_some()
        && a.volume_serial_number() == b.volume_serial_number()
        && a.file_index().is_some()
        && a.file_index() == b.file_index()
}
#[cfg(not(any(unix, windows)))]
fn same_file(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    false
}
fn create_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_permissions(&mut options);
    options.open(path)
}
fn create_temp(parent: &Path, destination: &Path) -> Result<(PathBuf, File), StateError> {
    let name = destination
        .file_name()
        .ok_or(StateError::InvalidStateLocation)?
        .to_string_lossy();
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
        match create_new(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(StateError::Io),
        }
    }
    Err(StateError::Io)
}
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}
#[cfg(unix)]
fn set_private_permissions(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}
#[cfg(not(unix))]
fn set_private_permissions(_: &mut OpenOptions) {}
struct LockGuard {
    _file: File,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}
impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidToml => "project state has invalid syntax, fields or values",
            Self::UnsupportedVersion => "unsupported project state version",
            Self::InvalidProjectName => "invalid project name",
            Self::InvalidProjectPath => "invalid project path",
            Self::OverlappingProjectRoots => "registered project roots overlap",
            Self::ProjectAlreadyRegistered => "project name is already registered",
            Self::ProjectNotRegistered => "project name is not registered",
            Self::InvalidStateLocation => "invalid state location",
            Self::UnsafeDestination => "state destination is not a regular file",
            Self::LockExists => "project state is locked by another operation",
            Self::Io => "project state I/O failed",
        })
    }
}
impl Error for StateError {}

#[cfg(test)]
mod path_tests {
    use super::*;
    #[test]
    fn xdg_default_falls_back_for_empty_or_relative_values() {
        let home = std::ffi::OsStr::new("/home/test");
        assert_eq!(
            resolve_default_state_path(Some(std::ffi::OsStr::new("")), Some(home)).unwrap(),
            PathBuf::from("/home/test/.local/state/hule/projects.toml")
        );
        assert_eq!(
            resolve_default_state_path(Some(std::ffi::OsStr::new("relative")), Some(home)).unwrap(),
            PathBuf::from("/home/test/.local/state/hule/projects.toml")
        );
        assert_eq!(
            resolve_default_state_path(Some(std::ffi::OsStr::new("/state")), Some(home)).unwrap(),
            PathBuf::from("/state/hule/projects.toml")
        );
        assert_eq!(
            resolve_default_state_path(None, Some(home)).unwrap(),
            PathBuf::from("/home/test/.local/state/hule/projects.toml")
        );
        assert_eq!(
            resolve_default_state_path(None, Some(std::ffi::OsStr::new("relative"))),
            Err(StateError::InvalidStateLocation)
        );
        assert_eq!(
            resolve_default_state_path(None, None),
            Err(StateError::InvalidStateLocation)
        );
    }
}
