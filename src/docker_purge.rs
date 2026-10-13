use crate::{
    CleanupProfile,
    action_plan::{
        DockerDirectoryIdentity, DockerFileIdentity, DockerPurgeActionPlan, DockerPurgeIdentity,
        DockerPurgeScope, valid_docker_socket_endpoint,
    },
    config::{Category, Config, ConfigError},
};
use std::{
    env,
    error::Error,
    fmt, fs,
    io::{self, Read},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const MINIMUM_ENGINE_API: (u32, u32) = (1, 42);
const MAX_METADATA_OUTPUT: usize = 16 * 1024;
const DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DOCKER_METADATA_TIMEOUT: Duration = Duration::from_secs(15);
const DOCKER_SYSTEM_PRUNE_ARGS: [&str; 5] = ["system", "prune", "--all", "--volumes", "--force"];
const DOCKER_ENV_OVERRIDES: [&str; 6] = [
    "DOCKER_API_VERSION",
    "DOCKER_CERT_PATH",
    "DOCKER_CONFIG",
    "DOCKER_CONTEXT",
    "DOCKER_HOST",
    "DOCKER_TLS_VERIFY",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerObservedIdentity {
    pub endpoint_identity: DockerFileIdentity,
    pub endpoint_parent_identities: Vec<DockerDirectoryIdentity>,
    pub identity: DockerPurgeIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerPurgeError {
    InvalidPlan,
    InvalidEndpoint,
    UnsupportedApi,
    EnvironmentOverride,
    IdentityChanged,
    CategoryDisabled,
    ProfileMismatch,
    InvalidConfiguration,
    Declined,
    Interactive,
    DockerCommand,
    PurgeNotStarted,
    PurgeUnknown,
}

impl fmt::Display for DockerPurgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPlan => "invalid Docker purge plan",
            Self::InvalidEndpoint => "Docker purge requires a verified local Unix socket",
            Self::UnsupportedApi => "Docker Engine API version 1.42 or newer is required",
            Self::EnvironmentOverride => {
                "Docker purge refuses inherited Docker endpoint, context, API, TLS or config overrides"
            }
            Self::IdentityChanged => "Docker endpoint or Engine identity changed; plan is stale",
            Self::CategoryDisabled => "docker_purge is not enabled for the selected profile",
            Self::ProfileMismatch => "Docker purge plans require the aggressive profile",
            Self::InvalidConfiguration => "could not load Docker purge configuration",
            Self::Declined => "Docker purge confirmation did not match",
            Self::Interactive => "could not read interactive Docker purge confirmation",
            Self::DockerCommand => "Docker CLI inspection failed; no purge was attempted",
            Self::PurgeNotStarted => "Docker prune command could not be started; no purge was attempted",
            Self::PurgeUnknown => {
                "Docker system prune failed; resources may have been partially removed, outcome is unknown, and no retry was attempted"
            }
        })
    }
}

impl Error for DockerPurgeError {}

pub trait DockerPurgeRunner {
    fn inspect(
        &mut self,
        endpoint: &str,
        docker_cli_path: &Path,
    ) -> Result<DockerObservedIdentity, DockerPurgeError>;

    fn system_prune(
        &mut self,
        endpoint: &str,
        expected: &DockerObservedIdentity,
    ) -> Result<(), DockerPurgeError>;
}

pub fn create_plan<R: DockerPurgeRunner>(
    runner: &mut R,
    config: &Config,
    profile: CleanupProfile,
    endpoint: &str,
    docker_cli_path: &Path,
) -> Result<DockerPurgeActionPlan, DockerPurgeError> {
    ensure_policy(config, profile)?;
    if !valid_docker_socket_endpoint(endpoint) {
        return Err(DockerPurgeError::InvalidEndpoint);
    }
    let observed = runner.inspect(endpoint, docker_cli_path)?;
    if !api_version_supported(&observed.identity.client_api_version)
        || !api_version_supported(&observed.identity.server_api_version)
    {
        return Err(DockerPurgeError::UnsupportedApi);
    }
    let plan = DockerPurgeActionPlan {
        format: "hule-docker-purge-action".into(),
        schema_version: 1,
        profile,
        category: Category::DockerPurge,
        endpoint: endpoint.into(),
        endpoint_identity: observed.endpoint_identity,
        endpoint_parent_identities: observed.endpoint_parent_identities,
        identity: observed.identity,
        scope: DockerPurgeScope::SystemPruneAllWithAnonymousVolumes,
    };
    plan.validate().map_err(|_| DockerPurgeError::InvalidPlan)?;
    Ok(plan)
}

pub fn revalidate<R: DockerPurgeRunner>(
    runner: &mut R,
    config: &Config,
    profile: CleanupProfile,
    plan: &DockerPurgeActionPlan,
) -> Result<DockerObservedIdentity, DockerPurgeError> {
    plan.validate().map_err(|_| DockerPurgeError::InvalidPlan)?;
    if profile != plan.profile || profile != CleanupProfile::Aggressive {
        return Err(DockerPurgeError::ProfileMismatch);
    }
    ensure_policy(config, profile)?;
    let observed = runner.inspect(&plan.endpoint, &plan.identity.docker_cli_path)?;
    if observed.endpoint_identity != plan.endpoint_identity
        || observed.endpoint_parent_identities != plan.endpoint_parent_identities
        || observed.identity != plan.identity
    {
        return Err(DockerPurgeError::IdentityChanged);
    }
    Ok(observed)
}

pub fn apply_with_confirmation<R, L, C>(
    runner: &mut R,
    mut load_config: L,
    plan: &DockerPurgeActionPlan,
    mut confirm: C,
) -> Result<(), DockerPurgeError>
where
    R: DockerPurgeRunner,
    L: FnMut() -> Result<(Config, CleanupProfile), DockerPurgeError>,
    C: FnMut(&DockerPurgeActionPlan) -> Result<bool, DockerPurgeError>,
{
    let (initial_config, initial_profile) = load_config()?;
    revalidate(runner, &initial_config, initial_profile, plan)?;
    if !confirm(plan)? {
        return Err(DockerPurgeError::Declined);
    }

    let (current_config, current_profile) = load_config()?;
    let observed = revalidate(runner, &current_config, current_profile, plan)?;
    runner.system_prune(&plan.endpoint, &observed)
}

fn ensure_policy(config: &Config, profile: CleanupProfile) -> Result<(), DockerPurgeError> {
    if profile != CleanupProfile::Aggressive {
        return Err(DockerPurgeError::ProfileMismatch);
    }
    if !config.categories(profile).contains(&Category::DockerPurge) {
        return Err(DockerPurgeError::CategoryDisabled);
    }
    Ok(())
}

pub fn confirmation_phrase(plan: &DockerPurgeActionPlan) -> Result<String, DockerPurgeError> {
    plan.validate().map_err(|_| DockerPurgeError::InvalidPlan)?;
    let bytes = serde_json::to_vec(plan).map_err(|_| DockerPurgeError::InvalidPlan)?;
    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    Ok(format!("PURGE-DOCKER-{hash:016x}"))
}

#[derive(Debug, Default)]
pub struct CommandDockerPurgeRunner;

impl DockerPurgeRunner for CommandDockerPurgeRunner {
    fn inspect(
        &mut self,
        endpoint: &str,
        selected_docker_cli_path: &Path,
    ) -> Result<DockerObservedIdentity, DockerPurgeError> {
        reject_environment_overrides(|name| env::var_os(name))?;
        let socket_before = inspect_local_socket(endpoint)?;
        let (docker_cli_path, docker_cli_file_identity, docker_cli_parent_identities) =
            inspect_docker_cli(selected_docker_cli_path)?;
        let client_config = tempfile::tempdir().map_err(|_| DockerPurgeError::DockerCommand)?;
        let mut version_command = docker_command(&docker_cli_path, endpoint, client_config.path());
        version_command.args([
            "version",
            "--format",
            "{{.Client.Version}}|{{.Client.APIVersion}}|{{.Server.Version}}|{{.Server.APIVersion}}",
        ]);
        verify_docker_cli(
            &docker_cli_path,
            &docker_cli_file_identity,
            &docker_cli_parent_identities,
        )?;
        let version_output = run_capture(
            version_command,
            DOCKER_METADATA_TIMEOUT,
            MAX_METADATA_OUTPUT,
        )?;
        verify_docker_cli(
            &docker_cli_path,
            &docker_cli_file_identity,
            &docker_cli_parent_identities,
        )?;
        let versions = parse_version_output(&version_output)?;
        if !api_version_supported(versions[1]) || !api_version_supported(versions[3]) {
            return Err(DockerPurgeError::UnsupportedApi);
        }
        let mut info_command = docker_command(&docker_cli_path, endpoint, client_config.path());
        info_command.args(["info", "--format", "{{.ID}}|{{.DockerRootDir}}"]);
        verify_docker_cli(
            &docker_cli_path,
            &docker_cli_file_identity,
            &docker_cli_parent_identities,
        )?;
        let info_output = run_capture(info_command, DOCKER_METADATA_TIMEOUT, MAX_METADATA_OUTPUT)?;
        verify_docker_cli(
            &docker_cli_path,
            &docker_cli_file_identity,
            &docker_cli_parent_identities,
        )?;
        let (engine_id, daemon_data_root) = parse_info_output(&info_output)?;
        let socket_after = inspect_local_socket(endpoint)?;
        if socket_before != socket_after {
            return Err(DockerPurgeError::IdentityChanged);
        }
        Ok(DockerObservedIdentity {
            endpoint_identity: socket_before.socket,
            endpoint_parent_identities: socket_before.parents,
            identity: DockerPurgeIdentity {
                docker_cli_path,
                docker_cli_file_identity,
                docker_cli_parent_identities,
                docker_cli_version: versions[0].to_owned(),
                engine_id,
                daemon_data_root,
                engine_version: versions[2].to_owned(),
                client_api_version: versions[1].to_owned(),
                server_api_version: versions[3].to_owned(),
            },
        })
    }

    fn system_prune(
        &mut self,
        endpoint: &str,
        expected: &DockerObservedIdentity,
    ) -> Result<(), DockerPurgeError> {
        reject_environment_overrides(|name| env::var_os(name))?;
        let client_config = tempfile::tempdir().map_err(|_| DockerPurgeError::PurgeNotStarted)?;
        let client_config_path = fs::canonicalize(client_config.path())
            .map_err(|_| DockerPurgeError::PurgeNotStarted)?;
        let current = self.inspect(endpoint, &expected.identity.docker_cli_path)?;
        if &current != expected {
            return Err(DockerPurgeError::IdentityChanged);
        }
        if paths_overlap(&client_config_path, &current.identity.daemon_data_root) {
            return Err(DockerPurgeError::PurgeNotStarted);
        }
        let socket = inspect_local_socket(endpoint)?;
        if socket.socket != current.endpoint_identity
            || socket.parents != current.endpoint_parent_identities
        {
            return Err(DockerPurgeError::IdentityChanged);
        }
        verify_docker_cli(
            &current.identity.docker_cli_path,
            &current.identity.docker_cli_file_identity,
            &current.identity.docker_cli_parent_identities,
        )?;
        let mut command = docker_command(
            &current.identity.docker_cli_path,
            endpoint,
            &client_config_path,
        );
        command
            .args(DOCKER_SYSTEM_PRUNE_ARGS)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        run_silent(&mut command, DOCKER_COMMAND_TIMEOUT)
    }
}

fn docker_command(cli_path: &Path, endpoint: &str, client_config: &Path) -> Command {
    let mut command = Command::new(cli_path);
    command
        .env_clear()
        .env("PATH", cli_path.parent().unwrap_or(Path::new("/")))
        .env("HOME", client_config)
        .env("LC_ALL", "C")
        .arg("--config")
        .arg(client_config)
        .arg("--host")
        .arg(endpoint);
    command
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SocketInspection {
    socket: DockerFileIdentity,
    parents: Vec<DockerDirectoryIdentity>,
}

fn inspect_docker_cli(
    path: &Path,
) -> Result<(PathBuf, DockerFileIdentity, Vec<DockerDirectoryIdentity>), DockerPurgeError> {
    if !path.is_absolute() {
        return Err(DockerPurgeError::DockerCommand);
    }
    let canonical = fs::canonicalize(path).map_err(|_| DockerPurgeError::DockerCommand)?;
    if canonical.file_name() != Some(std::ffi::OsStr::new("docker")) {
        return Err(DockerPurgeError::DockerCommand);
    }
    let metadata = fs::metadata(&canonical).map_err(|_| DockerPurgeError::DockerCommand)?;
    let uid = metadata.uid();
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
        || (uid != 0 && uid != unsafe { libc::geteuid() })
    {
        return Err(DockerPurgeError::DockerCommand);
    }
    let parents =
        inspect_trusted_parents(&canonical).map_err(|_| DockerPurgeError::DockerCommand)?;
    Ok((canonical, docker_file_identity(&metadata), parents))
}

fn verify_docker_cli(
    path: &Path,
    expected: &DockerFileIdentity,
    expected_parents: &[DockerDirectoryIdentity],
) -> Result<(), DockerPurgeError> {
    let (canonical, identity, parents) = inspect_docker_cli(path)?;
    if canonical != path || &identity != expected || parents != expected_parents {
        return Err(DockerPurgeError::IdentityChanged);
    }
    Ok(())
}

fn inspect_local_socket(endpoint: &str) -> Result<SocketInspection, DockerPurgeError> {
    if !valid_docker_socket_endpoint(endpoint) {
        return Err(DockerPurgeError::InvalidEndpoint);
    }
    let socket_path = Path::new(
        endpoint
            .strip_prefix("unix://")
            .ok_or(DockerPurgeError::InvalidEndpoint)?,
    );
    let metadata =
        fs::symlink_metadata(socket_path).map_err(|_| DockerPurgeError::InvalidEndpoint)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        return Err(DockerPurgeError::InvalidEndpoint);
    }
    if fs::canonicalize(socket_path).map_err(|_| DockerPurgeError::InvalidEndpoint)? != socket_path
    {
        return Err(DockerPurgeError::InvalidEndpoint);
    }
    let parents = inspect_trusted_parents(socket_path)?;
    if metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() } {
        return Err(DockerPurgeError::InvalidEndpoint);
    }
    Ok(SocketInspection {
        socket: docker_file_identity(&metadata),
        parents,
    })
}

fn inspect_trusted_parents(path: &Path) -> Result<Vec<DockerDirectoryIdentity>, DockerPurgeError> {
    let parent = path.parent().ok_or(DockerPurgeError::InvalidEndpoint)?;
    let mut paths = vec![PathBuf::from("/")];
    let mut current = PathBuf::from("/");
    for component in parent.components().skip(1) {
        let std::path::Component::Normal(name) = component else {
            return Err(DockerPurgeError::InvalidEndpoint);
        };
        current.push(name);
        paths.push(current.clone());
    }

    let euid = unsafe { libc::geteuid() };
    let mut identities = Vec::with_capacity(paths.len());
    for path in paths {
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| DockerPurgeError::InvalidEndpoint)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(DockerPurgeError::InvalidEndpoint);
        }
        let uid = metadata.uid();
        let mode = metadata.permissions().mode();
        let root_owned_sticky = uid == 0 && mode & 0o1000 != 0;
        if (uid != 0 && uid != euid) || (mode & 0o022 != 0 && !root_owned_sticky) {
            return Err(DockerPurgeError::InvalidEndpoint);
        }
        identities.push(DockerDirectoryIdentity {
            path,
            identity: crate::action_plan::FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: metadata.mode(),
            },
        });
    }
    Ok(identities)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn docker_file_identity(metadata: &fs::Metadata) -> DockerFileIdentity {
    DockerFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn parse_version_output(output: &[u8]) -> Result<[&str; 4], DockerPurgeError> {
    let text = std::str::from_utf8(output).map_err(|_| DockerPurgeError::DockerCommand)?;
    let line = text.trim_end_matches('\n');
    if line.contains('\n') || line.contains('\r') {
        return Err(DockerPurgeError::DockerCommand);
    }
    let fields = line.split('|').collect::<Vec<_>>();
    let [cli_version, client_api, engine_version, server_api] = fields.as_slice() else {
        return Err(DockerPurgeError::DockerCommand);
    };
    if [*cli_version, *client_api, *engine_version, *server_api]
        .iter()
        .any(|value| value.is_empty() || value.len() > 128 || !value.is_ascii())
    {
        return Err(DockerPurgeError::DockerCommand);
    }
    Ok([cli_version, client_api, engine_version, server_api])
}

fn parse_info_output(output: &[u8]) -> Result<(String, PathBuf), DockerPurgeError> {
    let text = std::str::from_utf8(output).map_err(|_| DockerPurgeError::DockerCommand)?;
    let line = text.trim_end_matches('\n');
    if line.contains(['\n', '\r']) {
        return Err(DockerPurgeError::DockerCommand);
    }
    let fields = line.split('|').collect::<Vec<_>>();
    let [engine_id, daemon_data_root] = fields.as_slice() else {
        return Err(DockerPurgeError::DockerCommand);
    };
    if engine_id.is_empty()
        || engine_id.len() > 128
        || !engine_id.is_ascii()
        || engine_id.bytes().any(|byte| byte.is_ascii_control())
        || daemon_data_root.is_empty()
    {
        return Err(DockerPurgeError::DockerCommand);
    }
    Ok(((*engine_id).to_owned(), PathBuf::from(daemon_data_root)))
}

fn api_version_supported(version: &str) -> bool {
    let mut components = version.split('.');
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
        && major == MINIMUM_ENGINE_API.0
        && minor >= MINIMUM_ENGINE_API.1
        && major_text == major.to_string()
        && minor_text == minor.to_string()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

fn reject_environment_overrides(
    mut get: impl FnMut(&str) -> Option<std::ffi::OsString>,
) -> Result<(), DockerPurgeError> {
    if DOCKER_ENV_OVERRIDES.iter().any(|name| get(name).is_some()) {
        return Err(DockerPurgeError::EnvironmentOverride);
    }
    Ok(())
}

fn run_capture(
    mut command: Command,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Vec<u8>, DockerPurgeError> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| DockerPurgeError::DockerCommand)?;
    let stdout = child.stdout.take().ok_or(DockerPurgeError::DockerCommand)?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take((max_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let status = wait_bounded(&mut child, timeout)?;
    let bytes = reader
        .join()
        .map_err(|_| DockerPurgeError::DockerCommand)?
        .map_err(|_| DockerPurgeError::DockerCommand)?;
    if !status.success() || bytes.len() > max_bytes {
        return Err(DockerPurgeError::DockerCommand);
    }
    Ok(bytes)
}

fn run_silent(command: &mut Command, timeout: Duration) -> Result<(), DockerPurgeError> {
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| DockerPurgeError::PurgeNotStarted)?;
    match wait_bounded(&mut child, timeout) {
        Ok(status) if status.success() => Ok(()),
        _ => Err(DockerPurgeError::PurgeUnknown),
    }
}

fn wait_bounded(child: &mut Child, timeout: Duration) -> Result<ExitStatus, DockerPurgeError> {
    wait_bounded_with(child, timeout, Child::try_wait)
}

fn wait_bounded_with(
    child: &mut Child,
    timeout: Duration,
    mut try_wait: impl FnMut(&mut Child) -> io::Result<Option<ExitStatus>>,
) -> Result<ExitStatus, DockerPurgeError> {
    let started = Instant::now();
    loop {
        match try_wait(child) {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(_) => {
                terminate_and_reap(child);
                return Err(DockerPurgeError::DockerCommand);
            }
        }
        if started.elapsed() >= timeout {
            terminate_and_reap(child);
            return Err(DockerPurgeError::DockerCommand);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub fn config_load_error(_: ConfigError) -> DockerPurgeError {
    DockerPurgeError::InvalidConfiguration
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_plan::{ActionPlan, read_action_plan, write_docker_purge_new};
    use std::{
        collections::VecDeque,
        fs,
        sync::{Arc, Mutex},
    };

    fn config(enabled: bool) -> Config {
        let categories = if enabled { "[\"docker_purge\"]" } else { "[]" };
        Config::from_toml(&format!(
            "schema_version = 2\ndefault_profile = \"normal\"\n\n[profiles.low]\ncategories = []\n[profiles.normal]\ncategories = []\n[profiles.aggressive]\ncategories = {categories}\n"
        ))
        .unwrap()
    }

    fn docker_file_identity_for_owner(mode: u32, uid: u32) -> DockerFileIdentity {
        DockerFileIdentity {
            device: 1,
            inode: 2,
            uid,
            gid: 20,
            mode,
            size: 100,
            modified_seconds: 1,
            modified_nanoseconds: 2,
            changed_seconds: 1,
            changed_nanoseconds: 2,
        }
    }

    fn directory_identity(mode: u32, uid: u32) -> crate::action_plan::FileIdentity {
        crate::action_plan::FileIdentity {
            device: 1,
            inode: 2,
            uid,
            gid: 20,
            mode,
        }
    }

    fn observed(api: &str) -> DockerObservedIdentity {
        DockerObservedIdentity {
            endpoint_identity: docker_file_identity_for_owner(0o140000 | 0o600, unsafe {
                libc::geteuid()
            }),
            endpoint_parent_identities: vec![
                DockerDirectoryIdentity {
                    path: PathBuf::from("/"),
                    identity: directory_identity(0o040755, 0),
                },
                DockerDirectoryIdentity {
                    path: PathBuf::from("/tmp"),
                    identity: directory_identity(0o040000 | 0o01777, 0),
                },
            ],
            identity: DockerPurgeIdentity {
                docker_cli_path: PathBuf::from("/usr/bin/docker"),
                docker_cli_file_identity: docker_file_identity_for_owner(0o100755, unsafe {
                    libc::geteuid()
                }),
                docker_cli_parent_identities: vec![
                    DockerDirectoryIdentity {
                        path: PathBuf::from("/"),
                        identity: directory_identity(0o040755, 0),
                    },
                    DockerDirectoryIdentity {
                        path: PathBuf::from("/usr"),
                        identity: directory_identity(0o040755, 0),
                    },
                    DockerDirectoryIdentity {
                        path: PathBuf::from("/usr/bin"),
                        identity: directory_identity(0o040755, 0),
                    },
                ],
                docker_cli_version: "29.0.0".into(),
                engine_id: "engine-fixture-123".into(),
                daemon_data_root: PathBuf::from("/var/lib/docker"),
                engine_version: "29.0.0".into(),
                client_api_version: api.into(),
                server_api_version: api.into(),
            },
        }
    }

    const ENDPOINT: &str = "unix:///tmp/hule-docker.sock";

    struct MockRunner {
        identities: VecDeque<Result<DockerObservedIdentity, DockerPurgeError>>,
        current: Option<DockerObservedIdentity>,
        inspections: usize,
        prunes: usize,
        prune_result: Result<(), DockerPurgeError>,
    }

    impl Default for MockRunner {
        fn default() -> Self {
            Self {
                identities: VecDeque::new(),
                current: None,
                inspections: 0,
                prunes: 0,
                prune_result: Ok(()),
            }
        }
    }

    impl DockerPurgeRunner for MockRunner {
        fn inspect(
            &mut self,
            _: &str,
            _: &Path,
        ) -> Result<DockerObservedIdentity, DockerPurgeError> {
            self.inspections += 1;
            if let Some(result) = self.identities.pop_front() {
                return result;
            }
            self.current.clone().ok_or(DockerPurgeError::DockerCommand)
        }

        fn system_prune(
            &mut self,
            _: &str,
            expected: &DockerObservedIdentity,
        ) -> Result<(), DockerPurgeError> {
            self.prunes += 1;
            if self.current.as_ref() != Some(expected) {
                return Err(DockerPurgeError::IdentityChanged);
            }
            self.prune_result
        }
    }

    fn plan(runner: &mut MockRunner) -> DockerPurgeActionPlan {
        create_plan(
            runner,
            &config(true),
            CleanupProfile::Aggressive,
            ENDPOINT,
            Path::new("/usr/bin/docker"),
        )
        .unwrap()
    }

    #[test]
    fn planning_requires_permission_and_engine_api_142_without_pruning() {
        let mut runner = MockRunner {
            current: Some(observed("1.42")),
            ..MockRunner::default()
        };
        assert_eq!(
            create_plan(
                &mut runner,
                &config(false),
                CleanupProfile::Aggressive,
                ENDPOINT,
                Path::new("/usr/bin/docker")
            ),
            Err(DockerPurgeError::CategoryDisabled)
        );
        assert_eq!(runner.inspections, 0);

        runner.current = Some(observed("1.41"));
        assert_eq!(
            create_plan(
                &mut runner,
                &config(true),
                CleanupProfile::Aggressive,
                ENDPOINT,
                Path::new("/usr/bin/docker")
            ),
            Err(DockerPurgeError::UnsupportedApi)
        );
        assert_eq!(runner.prunes, 0);
    }

    #[test]
    fn stale_identity_and_declined_confirmation_never_prune() {
        let mut runner = MockRunner {
            current: Some(observed("1.42")),
            ..MockRunner::default()
        };
        let plan = plan(&mut runner);
        runner.current.as_mut().unwrap().identity.engine_id = "different-engine".into();
        let mut confirmed = false;
        let result = apply_with_confirmation(
            &mut runner,
            || Ok((config(true), CleanupProfile::Aggressive)),
            &plan,
            |_| {
                confirmed = true;
                Ok(true)
            },
        );
        assert_eq!(result, Err(DockerPurgeError::IdentityChanged));
        assert!(!confirmed);
        assert_eq!(runner.prunes, 0);

        runner.current = Some(observed("1.42"));
        let result = apply_with_confirmation(
            &mut runner,
            || Ok((config(true), CleanupProfile::Aggressive)),
            &plan,
            |_| Ok(false),
        );
        assert_eq!(result, Err(DockerPurgeError::Declined));
        assert_eq!(runner.prunes, 0);
    }

    #[test]
    fn identity_or_policy_drift_after_confirmation_refuses_prune() {
        let mut runner = MockRunner {
            current: Some(observed("1.42")),
            ..MockRunner::default()
        };
        let plan = plan(&mut runner);
        runner.identities = VecDeque::from([
            Ok(observed("1.42")),
            Ok({
                let mut changed = observed("1.42");
                changed.identity.engine_id = "replacement-engine".into();
                changed
            }),
        ]);
        let result = apply_with_confirmation(
            &mut runner,
            || Ok((config(true), CleanupProfile::Aggressive)),
            &plan,
            |_| Ok(true),
        );
        assert_eq!(result, Err(DockerPurgeError::IdentityChanged));
        assert_eq!(runner.prunes, 0);

        let mut loads = 0;
        let result = apply_with_confirmation(
            &mut runner,
            || {
                loads += 1;
                Ok((config(loads == 1), CleanupProfile::Aggressive))
            },
            &plan,
            |_| Ok(true),
        );
        assert_eq!(result, Err(DockerPurgeError::CategoryDisabled));
        assert_eq!(runner.prunes, 0);
    }

    #[test]
    fn confirmation_reloads_policy_and_runs_one_explicit_prune() {
        let mut runner = MockRunner {
            current: Some(observed("1.42")),
            ..MockRunner::default()
        };
        let plan = plan(&mut runner);
        let loads = Arc::new(Mutex::new(0));
        let load_count = Arc::clone(&loads);
        let result = apply_with_confirmation(
            &mut runner,
            move || {
                let mut count = load_count.lock().unwrap();
                *count += 1;
                Ok((
                    config(*count == 1 || *count == 2),
                    CleanupProfile::Aggressive,
                ))
            },
            &plan,
            |plan| Ok(confirmation_phrase(plan).is_ok()),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(*loads.lock().unwrap(), 2);
        assert_eq!(runner.prunes, 1);
    }

    #[test]
    fn saved_docker_plan_is_private_strict_and_dispatches_without_fallback() {
        let fixture = tempfile::tempdir().unwrap();
        let mut runner = MockRunner {
            current: Some(observed("1.42")),
            ..MockRunner::default()
        };
        let plan = plan(&mut runner);
        let path = fixture.path().join("docker-plan.json");
        write_docker_purge_new(&path, &plan).unwrap();
        assert_eq!(
            read_action_plan(&path).unwrap(),
            ActionPlan::DockerPurge(plan.clone())
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let raw = fs::read_to_string(&path).unwrap();
        let duplicate = raw.replace(
            "\"engine_id\": \"engine-fixture-123\"",
            "\"engine_id\": \"other\", \"engine_id\": \"engine-fixture-123\"",
        );
        fs::write(&path, duplicate).unwrap();
        assert!(read_action_plan(&path).is_err());

        let data_root = fixture.path().join("daemon-data");
        fs::create_dir(&data_root).unwrap();
        let data_root = fs::canonicalize(data_root).unwrap();
        let mut overlapping = plan;
        overlapping.identity.daemon_data_root = data_root.clone();
        assert!(
            crate::action_plan::validate_docker_purge_location(
                &data_root.join("plan.json"),
                &overlapping
            )
            .is_err()
        );
    }

    #[test]
    fn native_prune_failure_is_unknown_and_is_never_retried() {
        let mut runner = MockRunner {
            current: Some(observed("1.42")),
            prune_result: Err(DockerPurgeError::PurgeUnknown),
            ..MockRunner::default()
        };
        let plan = plan(&mut runner);
        let result = apply_with_confirmation(
            &mut runner,
            || Ok((config(true), CleanupProfile::Aggressive)),
            &plan,
            |_| Ok(true),
        );
        assert_eq!(result, Err(DockerPurgeError::PurgeUnknown));
        assert_eq!(runner.prunes, 1);
    }

    #[test]
    fn docker_environment_overrides_are_rejected_without_spawning() {
        assert_eq!(
            reject_environment_overrides(|name| {
                (name == "DOCKER_CONTEXT").then(|| "fixture".into())
            }),
            Err(DockerPurgeError::EnvironmentOverride)
        );
        assert_eq!(reject_environment_overrides(|_| None), Ok(()));
    }

    #[test]
    fn system_prune_arguments_are_fixed_and_include_no_user_values() {
        assert_eq!(
            DOCKER_SYSTEM_PRUNE_ARGS,
            ["system", "prune", "--all", "--volumes", "--force"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_endpoint_requires_a_real_unaliased_unix_socket() {
        use std::os::unix::net::UnixListener;

        let fixture = tempfile::tempdir().unwrap();
        let fixture_root = fs::canonicalize(fixture.path()).unwrap();
        let socket_path = fixture_root.join("docker.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let endpoint = format!("unix://{}", socket_path.display());
        assert!(inspect_local_socket(&endpoint).is_ok());
        drop(listener);
        fs::remove_file(&socket_path).unwrap();
        assert_eq!(
            inspect_local_socket(&endpoint),
            Err(DockerPurgeError::InvalidEndpoint)
        );

        let regular_file = fixture_root.join("not-a-socket");
        fs::write(&regular_file, "fixture").unwrap();
        assert_eq!(
            inspect_local_socket(&format!("unix://{}", regular_file.display())),
            Err(DockerPurgeError::InvalidEndpoint)
        );
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_replacement_and_writable_parent_are_rejected() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};

        let fixture = tempfile::tempdir().unwrap();
        let fixture_root = fs::canonicalize(fixture.path()).unwrap();
        let socket_path = fixture_root.join("docker.sock");
        let endpoint = format!("unix://{}", socket_path.display());
        let first = UnixListener::bind(&socket_path).unwrap();
        let original = inspect_local_socket(&endpoint).unwrap();
        fs::remove_file(&socket_path).unwrap();
        let second = UnixListener::bind(&socket_path).unwrap();
        let replacement = inspect_local_socket(&endpoint).unwrap();
        assert_ne!(original, replacement);
        drop((first, second));

        let unsafe_parent = fixture.path().join("unsafe");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        let unsafe_socket = unsafe_parent.join("docker.sock");
        let listener = UnixListener::bind(&unsafe_socket).unwrap();
        assert_eq!(
            inspect_local_socket(&format!("unix://{}", unsafe_socket.display())),
            Err(DockerPurgeError::InvalidEndpoint)
        );
        drop(listener);
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn docker_cli_identity_is_pinned_and_parent_permissions_are_checked() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let cli_path = fixture.path().join("docker");
        fs::write(&cli_path, b"fixture docker executable").unwrap();
        fs::set_permissions(&cli_path, fs::Permissions::from_mode(0o700)).unwrap();
        let (canonical, identity, parents) = inspect_docker_cli(&cli_path).unwrap();
        assert_eq!(canonical, fs::canonicalize(&cli_path).unwrap());
        verify_docker_cli(&canonical, &identity, &parents).unwrap();

        fs::write(&cli_path, b"replacement docker executable").unwrap();
        assert_eq!(
            verify_docker_cli(&canonical, &identity, &parents),
            Err(DockerPurgeError::IdentityChanged)
        );

        let unsafe_parent = fixture.path().join("writable");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        let unsafe_cli = unsafe_parent.join("docker");
        fs::write(&unsafe_cli, b"fixture").unwrap();
        fs::set_permissions(&unsafe_cli, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            inspect_docker_cli(&unsafe_cli),
            Err(DockerPurgeError::DockerCommand)
        );
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn wait_error_terminates_and_reaps_child() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let result = wait_bounded_with(&mut child, Duration::from_secs(1), |_| {
            Err(io::Error::other("simulated wait error"))
        });
        assert_eq!(result, Err(DockerPurgeError::DockerCommand));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn local_endpoint_and_api_versions_are_strict() {
        assert!(valid_docker_socket_endpoint(ENDPOINT));
        assert!(!valid_docker_socket_endpoint("tcp://127.0.0.1:2375"));
        assert!(!valid_docker_socket_endpoint("unix://relative.sock"));
        assert!(!api_version_supported("1.41"));
        assert!(api_version_supported("1.42"));
        assert!(api_version_supported("1.99"));
        assert!(!api_version_supported("1.042"));
        assert!(!api_version_supported("2.0"));
    }
}
