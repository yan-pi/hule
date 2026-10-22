use crate::{
    CleanupProfile, RetentionDecision,
    config::{Category, Config},
    decide_retention,
    state::ProjectRegistry,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    error::Error,
    ffi::{CStr, CString},
    fmt,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const DEFAULT_MAX_ENTRIES: usize = 50_000;
const DEFAULT_MAX_DEPTH: usize = 32;
const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(20);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 4 * 1024;
const FORMAT: &str = "hule-filesystem-inventory";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryKind {
    RustTarget,
    XcodeDerivedData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryState {
    Complete,
    Incomplete,
    #[serde(rename = "timeout-requested")]
    TimeoutRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryIssue {
    RootUnavailable,
    ReadFailed,
    EntryLimit,
    DepthLimit,
    DeadlineExceeded,
    OutputLimit,
    SizeOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryAuthorization {
    NotAuthorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct InventorySummary {
    entry_count: usize,
    file_entries: usize,
    directory_entries: usize,
    symlink_entries: usize,
    other_entries: usize,
    apparent_bytes: u64,
}

impl InventorySummary {
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }

    pub fn file_entries(&self) -> usize {
        self.file_entries
    }

    pub fn directory_entries(&self) -> usize {
        self.directory_entries
    }

    pub fn symlink_entries(&self) -> usize {
        self.symlink_entries
    }

    pub fn other_entries(&self) -> usize {
        self.other_entries
    }

    /// Sum of distinct regular-file inode lengths; not physical storage or reclaimable bytes.
    pub fn apparent_bytes(&self) -> u64 {
        self.apparent_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InventoryReport {
    format: &'static str,
    schema_version: u8,
    kind: InventoryKind,
    state: InventoryState,
    issue: Option<InventoryIssue>,
    summary: Option<InventorySummary>,
    authorization: InventoryAuthorization,
    limitation: &'static str,
}

impl InventoryReport {
    pub fn kind(&self) -> InventoryKind {
        self.kind
    }

    pub fn state(&self) -> InventoryState {
        self.state
    }

    pub fn issue(&self) -> Option<InventoryIssue> {
        self.issue
    }

    pub fn summary(&self) -> Option<&InventorySummary> {
        self.summary.as_ref()
    }

    pub fn authorization(&self) -> InventoryAuthorization {
        self.authorization
    }

    fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Serializes only if the entire report fits the caller's byte cap; no partial JSON is returned.
    pub fn to_json_limited(&self, max_output_bytes: usize) -> Option<String> {
        let report = self.with_output_limit(max_output_bytes);
        let json = report.to_json().ok()?;
        (json.len() <= max_output_bytes).then_some(json)
    }

    /// Replaces a complete result with an incomplete marker if its report exceeds the output cap.
    pub fn with_output_limit(&self, max_output_bytes: usize) -> Self {
        let over_limit = serde_json::to_string_pretty(self)
            .map(|json| json.len() > max_output_bytes)
            .unwrap_or(true);
        if over_limit {
            Self::incomplete(self.kind, InventoryIssue::OutputLimit)
        } else {
            self.clone()
        }
    }

    fn complete(kind: InventoryKind, summary: InventorySummary) -> Self {
        Self {
            format: FORMAT,
            schema_version: 1,
            kind,
            state: InventoryState::Complete,
            issue: None,
            summary: Some(summary),
            authorization: InventoryAuthorization::NotAuthorized,
            limitation: "metadata-only apparent-size inventory; no file contents were read; apparent bytes are not physical usage or a reclaim estimate; filesystem operations may block, and cancellation is best-effort",
        }
    }

    fn incomplete(kind: InventoryKind, issue: InventoryIssue) -> Self {
        Self {
            format: FORMAT,
            schema_version: 1,
            kind,
            state: InventoryState::Incomplete,
            issue: Some(issue),
            summary: None,
            authorization: InventoryAuthorization::NotAuthorized,
            limitation: "inventory did not complete; no partial totals are published; no cleanup is authorized",
        }
    }

    fn timeout_requested(kind: InventoryKind) -> Self {
        Self {
            format: FORMAT,
            schema_version: 1,
            kind,
            state: InventoryState::TimeoutRequested,
            issue: Some(InventoryIssue::DeadlineExceeded),
            summary: None,
            authorization: InventoryAuthorization::NotAuthorized,
            limitation: "inventory deadline expired; parent should request worker termination and discard all output; a filesystem syscall may remain blocked after the request",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_duration: Duration,
    pub max_output_bytes: usize,
}

impl Default for InventoryLimits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_depth: DEFAULT_MAX_DEPTH,
            max_duration: DEFAULT_MAX_DURATION,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

impl InventoryLimits {
    /// Clamps caller-provided limits to Hule's fixed resource ceilings.
    pub fn bounded(self) -> Self {
        Self {
            max_entries: self.max_entries.min(DEFAULT_MAX_ENTRIES),
            max_depth: self.max_depth.min(DEFAULT_MAX_DEPTH),
            max_duration: self.max_duration.min(DEFAULT_MAX_DURATION),
            max_output_bytes: self.max_output_bytes.min(DEFAULT_MAX_OUTPUT_BYTES),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct InventoryDeadline(Instant);

impl InventoryDeadline {
    fn new(limits: &InventoryLimits) -> Self {
        let now = Instant::now();
        Self(now.checked_add(limits.max_duration).unwrap_or(now))
    }

    fn check(self) -> Result<(), ScanFailure> {
        if Instant::now() >= self.0 {
            Err(ScanFailure::DeadlineExceeded)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryError {
    Unregistered,
    CategoryDisabled,
    Retained,
    Protected,
    UnsafeTarget,
    InvalidXcodeRoot,
}

impl fmt::Display for InventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unregistered => "Rust target inventory requires a registered project",
            Self::CategoryDisabled => "rust_target is not enabled for this profile",
            Self::Retained => "project retention rules keep this target",
            Self::Protected => "target is protected by configured keep paths",
            Self::UnsafeTarget => {
                "target is not a verified directory inside the registered project"
            }
            Self::InvalidXcodeRoot => "selected path is not a canonical Xcode DerivedData root",
        })
    }
}

impl Error for InventoryError {}

/// Inventory a target path discovered by an injected caller, never by running Cargo here.
///
/// This performs filesystem calls that can block. Invoke it only in an isolated worker process
/// supervised by a parent with a best-effort deadline. A timeout request may not interrupt a
/// kernel-blocked filesystem call; callers must discard all partial output and never authorize work.
pub fn inspect_rust_target(
    config: &Config,
    registry: &ProjectRegistry,
    project_name: &str,
    profile: CleanupProfile,
    discovered_target: &Path,
    limits: &InventoryLimits,
) -> Result<InventoryReport, InventoryError> {
    let kind = InventoryKind::RustTarget;
    let limits = (*limits).bounded();
    let limits = &limits;
    let deadline = InventoryDeadline::new(limits);
    if deadline.check().is_err() {
        return Ok(limited_report(
            InventoryReport::timeout_requested(kind),
            limits,
        ));
    }
    let project = registry
        .get(project_name)
        .ok_or(InventoryError::Unregistered)?;
    if !config.categories(profile).contains(&Category::RustTarget) {
        return Err(InventoryError::CategoryDisabled);
    }
    let protected = config.overlaps_resolved_keep_path(discovered_target);
    if deadline.check().is_err() {
        return Ok(limited_report(
            InventoryReport::timeout_requested(kind),
            limits,
        ));
    }
    match decide_retention(profile, Some(project.mode()), protected) {
        RetentionDecision::KeepProtected => return Err(InventoryError::Protected),
        RetentionDecision::KeepPersistent => return Err(InventoryError::Retained),
        RetentionDecision::SkipUnregistered => return Err(InventoryError::Unregistered),
        RetentionDecision::CategoryDisabled => return Err(InventoryError::CategoryDisabled),
        RetentionDecision::Eligible => {}
    }

    let project_root = match validate_canonical_directory(project.path(), deadline) {
        Ok(path) => path,
        Err(ScanFailure::UnsafePath) => return Err(InventoryError::UnsafeTarget),
        Err(failure) => return Ok(limited_report(report_failure(kind, failure), limits)),
    };
    let target = match validate_target(&project_root, discovered_target, deadline) {
        Ok(path) => path,
        Err(ScanFailure::UnsafePath) => return Err(InventoryError::UnsafeTarget),
        Err(failure) => return Ok(limited_report(report_failure(kind, failure), limits)),
    };
    if config.overlaps_resolved_keep_path(&target) {
        return Err(InventoryError::Protected);
    }
    if deadline.check().is_err() {
        return Ok(limited_report(
            InventoryReport::timeout_requested(kind),
            limits,
        ));
    }
    Ok(scan_root(kind, &target, limits, deadline))
}

/// Inventory only an explicitly selected `.../Library/Developer/Xcode/DerivedData` directory.
///
/// This performs filesystem calls that can block. Invoke it only in an isolated worker process
/// supervised by a parent with a best-effort deadline. A timeout request may not interrupt a
/// kernel-blocked filesystem call; callers must discard all partial output and never authorize work.
pub fn inspect_xcode_derived_data(
    selected_root: &Path,
    limits: &InventoryLimits,
) -> Result<InventoryReport, InventoryError> {
    let kind = InventoryKind::XcodeDerivedData;
    let limits = (*limits).bounded();
    let limits = &limits;
    let deadline = InventoryDeadline::new(limits);
    if !is_derived_data_path(selected_root) {
        return Err(InventoryError::InvalidXcodeRoot);
    }
    if deadline.check().is_err() {
        return Ok(limited_report(
            InventoryReport::timeout_requested(kind),
            limits,
        ));
    }
    let metadata_result = fs::symlink_metadata(selected_root);
    if deadline.check().is_err() {
        return Ok(limited_report(
            InventoryReport::timeout_requested(kind),
            limits,
        ));
    }
    let metadata = match metadata_result {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(limited_report(
                InventoryReport::incomplete(kind, InventoryIssue::RootUnavailable),
                limits,
            ));
        }
        Err(_) => {
            return Ok(limited_report(
                InventoryReport::incomplete(kind, InventoryIssue::ReadFailed),
                limits,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(InventoryError::InvalidXcodeRoot);
    }
    if deadline.check().is_err() {
        return Ok(limited_report(
            InventoryReport::timeout_requested(kind),
            limits,
        ));
    }
    let canonical_result = fs::canonicalize(selected_root);
    if deadline.check().is_err() {
        return Ok(limited_report(
            InventoryReport::timeout_requested(kind),
            limits,
        ));
    }
    let root = match canonical_result {
        Ok(root) if root == selected_root => root,
        Ok(_) => return Err(InventoryError::InvalidXcodeRoot),
        Err(_) => {
            return Ok(limited_report(
                InventoryReport::incomplete(kind, InventoryIssue::ReadFailed),
                limits,
            ));
        }
    };
    if deadline.check().is_err() {
        return Ok(limited_report(
            InventoryReport::timeout_requested(kind),
            limits,
        ));
    }
    Ok(scan_root(kind, &root, limits, deadline))
}

fn is_derived_data_path(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        && !path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        && path.ends_with("Library/Developer/Xcode/DerivedData")
}

fn validate_canonical_directory(
    path: &Path,
    deadline: InventoryDeadline,
) -> Result<PathBuf, ScanFailure> {
    deadline.check()?;
    let metadata_result = fs::symlink_metadata(path);
    deadline.check()?;
    let metadata = match metadata_result {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ScanFailure::RootUnavailable);
        }
        Err(_) => return Err(ScanFailure::ReadFailed),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ScanFailure::UnsafePath);
    }
    let canonical_result = fs::canonicalize(path);
    deadline.check()?;
    let canonical = canonical_result.map_err(|_| ScanFailure::ReadFailed)?;
    if canonical != path {
        return Err(ScanFailure::UnsafePath);
    }
    Ok(canonical)
}

fn validate_target(
    project_root: &Path,
    target: &Path,
    deadline: InventoryDeadline,
) -> Result<PathBuf, ScanFailure> {
    if !target.is_absolute()
        || target
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        || target == project_root
    {
        return Err(ScanFailure::UnsafePath);
    }
    let relative = target
        .strip_prefix(project_root)
        .map_err(|_| ScanFailure::UnsafePath)?;
    if relative.as_os_str().is_empty() {
        return Err(ScanFailure::UnsafePath);
    }
    let mut current = project_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(ScanFailure::UnsafePath);
        };
        current.push(name);
        deadline.check()?;
        let metadata_result = fs::symlink_metadata(&current);
        deadline.check()?;
        let metadata = match metadata_result {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ScanFailure::RootUnavailable);
            }
            Err(_) => return Err(ScanFailure::ReadFailed),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ScanFailure::UnsafePath);
        }
    }
    let canonical_result = fs::canonicalize(target);
    deadline.check()?;
    let canonical = canonical_result.map_err(|_| ScanFailure::ReadFailed)?;
    if canonical != target || !canonical.starts_with(project_root) {
        return Err(ScanFailure::UnsafePath);
    }
    Ok(canonical)
}

struct Scanner {
    deadline: InventoryDeadline,
    max_entries: usize,
    max_depth: usize,
    summary: InventorySummary,
    seen_files: BTreeSet<(libc::dev_t, libc::ino_t)>,
}

impl Scanner {
    fn new(limits: &InventoryLimits, deadline: InventoryDeadline) -> Self {
        Self {
            deadline,
            max_entries: limits.max_entries,
            max_depth: limits.max_depth,
            summary: InventorySummary {
                entry_count: 0,
                file_entries: 0,
                directory_entries: 0,
                symlink_entries: 0,
                other_entries: 0,
                apparent_bytes: 0,
            },
            seen_files: BTreeSet::new(),
        }
    }

    fn scan_directory(&mut self, directory: &File, depth: usize) -> Result<(), ScanFailure> {
        self.check_deadline()?;
        let stream_result = DirectoryStream::new(directory);
        self.check_deadline()?;
        let stream = stream_result.map_err(|_| ScanFailure::ReadFailed)?;
        loop {
            self.check_deadline()?;
            let name_result = stream.next_name();
            self.check_deadline()?;
            let name = name_result.map_err(|_| ScanFailure::ReadFailed)?;
            let Some(name) = name else {
                return Ok(());
            };
            if name.as_bytes() == b"." || name.as_bytes() == b".." {
                continue;
            }
            self.add_entry()?;
            self.check_deadline()?;
            let metadata_result = stat_at(directory.as_raw_fd(), &name);
            self.check_deadline()?;
            let metadata = metadata_result.map_err(|_| ScanFailure::ReadFailed)?;
            let file_type = metadata.st_mode & libc::S_IFMT;
            if file_type == libc::S_IFLNK {
                self.summary.symlink_entries += 1;
            } else if file_type == libc::S_IFREG {
                self.add_file(&metadata)?;
            } else if file_type == libc::S_IFDIR {
                self.summary.directory_entries += 1;
                if depth >= self.max_depth {
                    return Err(ScanFailure::DepthLimit);
                }
                self.check_deadline()?;
                let child =
                    open_directory_at(directory.as_raw_fd(), &name, &metadata, self.deadline)?;
                self.scan_directory(&child, depth + 1)?;
            } else {
                self.summary.other_entries += 1;
            }
        }
    }

    fn add_entry(&mut self) -> Result<(), ScanFailure> {
        self.summary.entry_count = self
            .summary
            .entry_count
            .checked_add(1)
            .ok_or(ScanFailure::EntryLimit)?;
        if self.summary.entry_count > self.max_entries {
            return Err(ScanFailure::EntryLimit);
        }
        Ok(())
    }

    fn add_file(&mut self, metadata: &libc::stat) -> Result<(), ScanFailure> {
        self.summary.file_entries += 1;
        let identity = (metadata.st_dev, metadata.st_ino);
        if self.seen_files.insert(identity) {
            let size = u64::try_from(metadata.st_size).map_err(|_| ScanFailure::SizeOverflow)?;
            self.summary.apparent_bytes = self
                .summary
                .apparent_bytes
                .checked_add(size)
                .ok_or(ScanFailure::SizeOverflow)?;
        }
        Ok(())
    }

    fn check_deadline(&self) -> Result<(), ScanFailure> {
        self.deadline.check()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanFailure {
    ReadFailed,
    RootUnavailable,
    UnsafePath,
    EntryLimit,
    DepthLimit,
    DeadlineExceeded,
    SizeOverflow,
}

fn report_failure(kind: InventoryKind, failure: ScanFailure) -> InventoryReport {
    match failure {
        ScanFailure::RootUnavailable => {
            InventoryReport::incomplete(kind, InventoryIssue::RootUnavailable)
        }
        ScanFailure::ReadFailed | ScanFailure::UnsafePath => {
            InventoryReport::incomplete(kind, InventoryIssue::ReadFailed)
        }
        ScanFailure::EntryLimit => InventoryReport::incomplete(kind, InventoryIssue::EntryLimit),
        ScanFailure::DepthLimit => InventoryReport::incomplete(kind, InventoryIssue::DepthLimit),
        ScanFailure::DeadlineExceeded => InventoryReport::timeout_requested(kind),
        ScanFailure::SizeOverflow => {
            InventoryReport::incomplete(kind, InventoryIssue::SizeOverflow)
        }
    }
}

fn limited_report(report: InventoryReport, limits: &InventoryLimits) -> InventoryReport {
    report.with_output_limit(limits.max_output_bytes)
}

fn scan_root(
    kind: InventoryKind,
    root: &Path,
    limits: &InventoryLimits,
    deadline: InventoryDeadline,
) -> InventoryReport {
    let root_fd = (|| {
        deadline.check()?;
        let metadata_result = fs::symlink_metadata(root);
        deadline.check()?;
        let root_metadata = match metadata_result {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => metadata,
            Ok(_) => return Err(ScanFailure::UnsafePath),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ScanFailure::RootUnavailable);
            }
            Err(_) => return Err(ScanFailure::ReadFailed),
        };
        let canonical_result = fs::canonicalize(root);
        deadline.check()?;
        let canonical = canonical_result.map_err(|_| ScanFailure::ReadFailed)?;
        if canonical != root {
            return Err(ScanFailure::UnsafePath);
        }
        open_directory_root(&canonical, &root_metadata, deadline)
    })();
    let report = match root_fd {
        Ok(root_fd) => {
            let mut scanner = Scanner::new(limits, deadline);
            scanner.scan_directory(&root_fd, 0).map_or_else(
                |failure| report_failure(kind, failure),
                |_| InventoryReport::complete(kind, scanner.summary),
            )
        }
        Err(failure) => report_failure(kind, failure),
    };
    limited_report(report, limits)
}

fn open_directory_root(
    path: &Path,
    expected: &fs::Metadata,
    deadline: InventoryDeadline,
) -> Result<File, ScanFailure> {
    deadline.check()?;
    let result = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path);
    deadline.check()?;
    let directory = result.map_err(|_| ScanFailure::ReadFailed)?;
    let actual_result = stat_fd(directory.as_raw_fd());
    deadline.check()?;
    let actual = actual_result.map_err(|_| ScanFailure::ReadFailed)?;
    if actual.st_dev as u64 != expected.dev() || actual.st_ino as u64 != expected.ino() {
        return Err(ScanFailure::ReadFailed);
    }
    Ok(directory)
}

fn open_directory_at(
    parent_fd: libc::c_int,
    name: &CString,
    expected: &libc::stat,
    deadline: InventoryDeadline,
) -> Result<File, ScanFailure> {
    deadline.check()?;
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    deadline.check()?;
    if fd < 0 {
        return Err(ScanFailure::ReadFailed);
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    let actual_result = stat_fd(directory.as_raw_fd());
    deadline.check()?;
    let actual = actual_result.map_err(|_| ScanFailure::ReadFailed)?;
    if actual.st_dev != expected.st_dev || actual.st_ino != expected.st_ino {
        return Err(ScanFailure::ReadFailed);
    }
    Ok(directory)
}

fn stat_fd(fd: libc::c_int) -> io::Result<libc::stat> {
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut metadata) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(metadata)
}

fn stat_at(parent_fd: libc::c_int, name: &CString) -> io::Result<libc::stat> {
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            &mut metadata,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(metadata)
}

struct DirectoryStream(*mut libc::DIR);

impl DirectoryStream {
    fn new(directory: &File) -> io::Result<Self> {
        let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::close(duplicate) };
            return Err(error);
        }
        Ok(Self(stream))
    }

    fn next_name(&self) -> io::Result<Option<CString>> {
        set_errno(0);
        let entry = unsafe { libc::readdir(self.0) };
        if entry.is_null() {
            let errno = get_errno();
            if errno == 0 {
                return Ok(None);
            }
            return Err(io::Error::from_raw_os_error(errno));
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        CString::new(name.to_bytes())
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid directory entry"))
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(target_os = "macos")]
fn set_errno(value: libc::c_int) {
    unsafe { *libc::__error() = value };
}

#[cfg(target_os = "macos")]
fn get_errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

#[cfg(target_os = "linux")]
fn set_errno(value: libc::c_int) {
    unsafe { *libc::__errno_location() = value };
}

#[cfg(target_os = "linux")]
fn get_errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_limits_cannot_raise_hule_resource_ceilings() {
        let limits = InventoryLimits {
            max_entries: usize::MAX,
            max_depth: usize::MAX,
            max_duration: Duration::MAX,
            max_output_bytes: usize::MAX,
        }
        .bounded();
        assert_eq!(limits.max_entries, DEFAULT_MAX_ENTRIES);
        assert_eq!(limits.max_depth, DEFAULT_MAX_DEPTH);
        assert_eq!(limits.max_duration, DEFAULT_MAX_DURATION);
        assert_eq!(limits.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn report_serialization_is_bounded_and_contains_no_input_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("Library/Developer/Xcode/DerivedData");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("private-name"), b"contents are not read").unwrap();
        let root = fs::canonicalize(root).unwrap();
        let report = inspect_xcode_derived_data(&root, &InventoryLimits::default()).unwrap();
        let capped = report.with_output_limit(1);
        assert_eq!(capped.state(), InventoryState::Incomplete);
        assert_eq!(capped.issue(), Some(InventoryIssue::OutputLimit));
        assert!(capped.summary().is_none());
        assert!(!report.to_json().unwrap().contains("private-name"));
        assert!(
            !report
                .to_json()
                .unwrap()
                .contains(&root.to_string_lossy().to_string())
        );
    }
}
