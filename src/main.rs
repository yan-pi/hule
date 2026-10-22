use hule::{
    CleanupProfile,
    action_plan::{
        ActionPlan, DockerPurgeActionPlan, read_action_plan, validate_docker_purge_location,
        write_docker_purge_new, write_generation_new, write_new,
    },
    config::Config,
    docker_purge::{self, CommandDockerPurgeRunner, DockerPurgeError},
    doctor::DoctorReport,
    nix_generations::{self, CommandGenerationRunner, GenerationError, GenerationStatus},
    nix_store::{self, CommandNixRunner},
    preset::Preset,
    preview::{RustTargetSelection, preview_rust_target, preview_rust_target_plan},
    report::render_rust_target_preview,
    state,
};
use std::{
    error::Error,
    fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

const HELP: &str = "Usage:\n  hule --help\n  hule --version\n  hule doctor --config PATH [--json]\n  hule inspect nix-store --config PATH --store-path PATH... [--profile low|normal|aggressive] [--json]\n  hule inspect nix-generations --config PATH --nix-profile NAME [--generation ID...] [--profile low|normal|aggressive] [--json]\n  hule config preset NAME\n  hule plan nix-store --config PATH --store-path PATH... --output PLAN [--profile low|normal|aggressive]\n  hule plan nix-generations --config PATH --nix-profile NAME --generation ID... --output PLAN [--profile low|normal|aggressive]\n  hule plan docker --docker-purge --config PATH --docker-cli /ABS/PATH/docker --docker-endpoint unix:///PATH --output PLAN [--profile low|normal|aggressive]\n  hule apply --plan PATH --config PATH [--profile low|normal|aggressive]\n  hule project register --name NAME --path PATH --mode persistent|ephemeral [--state PATH]\n  hule project list [--state PATH]\n  hule project unregister --name NAME [--state PATH]\n  hule preview rust-target --config PATH (--project NAME | --all) [--state PATH] [--profile low|normal|aggressive] [--json]\n\nFor Rust-target preview Cargo metadata (`cargo metadata --no-deps --offline --locked`) may be invoked only when each project is registered, rust_target is enabled for the selected profile, and the anticipated target is not already protected. Cargo documentation does not formally guarantee absence of all side effects or code execution. Nix inspection queries only explicitly selected store objects or a configured profile and can open/create/migrate store metadata; it does not delete or authorize cleanup. Nix store planning queries requisites for explicitly named objects and can open/create/migrate store metadata. Apply requires an interactive confirmation and uses only the saved closure with nix-store --delete --store daemon; Nix performs native liveness checks and may refuse or partially complete. Generation planning accepts only exact numeric IDs from one configured user profile; apply removes only saved noncurrent profile links through nix-env --delete-generations. Generation apply does not activate generations, delete store objects, or run GC. Native profile locking protects the generation current at the command's locked check, but does not make the saved snapshot atomic against concurrent writers. No broad GC or direct filesystem removal is performed. Docker purge planning requires an explicit --docker-purge flag, the aggressive profile with docker_purge enabled, an explicitly selected trusted Docker CLI path, and a verified local Unix socket with protected parent directories; it records executable/socket/directory identities, Engine ID, and CLI/Engine/API versions and requires API 1.42 or newer. Apply requires a typed interactive confirmation and revalidates those identities immediately before running only docker system prune --all --volumes --force. The daemon dynamically selects stopped containers, unused networks, all unused images, build cache, and unused anonymous volumes at execution; no per-resource preview or rollback is possible. Errors may follow partial mutation; Hule makes no retry and reports unknown outcome. The doctor command parses only its explicit config and reports configured categories plus static capability limitations; it does not invoke provider commands, inspect host paths, contact daemons, or authorize cleanup. Invalid/unreadable configuration is reported as unsafe without echoing its path or contents.\n";

fn main() {
    if let Err(error) = run(std::env::args().skip(1).collect()) {
        let message: String = error
            .to_string()
            .chars()
            .map(|character| {
                if character.is_control() {
                    '?'
                } else {
                    character
                }
            })
            .collect();
        eprintln!("hule: {message}");
        std::process::exit(2);
    }
}

fn run(args: Vec<String>) -> Result<(), Box<dyn Error>> {
    match args.as_slice() {
        [flag] if flag == "--version" => println!("hule {}", env!("CARGO_PKG_VERSION")),
        [flag] if flag == "--help" => print!("{HELP}"),
        [command, ..] if command == "doctor" => run_doctor(&args[1..])?,
        [command, subcommand, ..] if command == "inspect" && subcommand == "nix-store" => {
            run_nix_store_inspection(&args[2..])?;
        }
        [command, subcommand, ..] if command == "inspect" && subcommand == "nix-generations" => {
            run_nix_generation_inspection(&args[2..])?;
        }
        [command, subcommand, ..] if command == "plan" && subcommand == "nix-store" => {
            run_nix_plan(&args[2..])?;
        }
        [command, subcommand, ..] if command == "plan" && subcommand == "nix-generations" => {
            run_nix_generation_plan(&args[2..])?;
        }
        [command, subcommand, ..] if command == "plan" && subcommand == "docker" => {
            run_docker_purge_plan(&args[2..])?;
        }
        [command, ..] if command == "apply" => run_apply(&args[1..])?,
        [command, subcommand, name] if command == "config" && subcommand == "preset" => {
            let preset = Preset::parse(name).ok_or("unknown preset name")?;
            print!("{}", preset.to_toml());
        }
        [command, subcommand, ..] if command == "project" => {
            run_project_command(subcommand, &args[2..])?;
        }
        [command, subcommand, ..] if command == "preview" && subcommand == "rust-target" => {
            let parsed = parse_preview_args(&args[2..])?;
            let config_contents = fs::read_to_string(parsed.config_path)?;
            let config = Config::from_toml(&config_contents)?;
            let state_path = match parsed.state_path {
                Some(path) => path,
                None => state::default_state_path()?,
            };
            let registry = state::load(state_path.as_path())?;
            let profile = parsed.profile.unwrap_or_else(|| config.default_profile());
            let selection = if parsed.all {
                RustTargetSelection::All
            } else {
                RustTargetSelection::Named(
                    parsed
                        .project_name
                        .clone()
                        .ok_or("exactly one of --project or --all is required")?,
                )
            };
            if parsed.json {
                let plan = preview_rust_target_plan(&config, &registry, selection, profile);
                let output = serde_json::to_string_pretty(&plan)
                    .map_err(|_| "could not serialize preview")?;
                println!("{output}");
            } else if parsed.all {
                if registry.list().is_empty() {
                    println!("no registered projects");
                } else {
                    for name in registry.list().keys() {
                        let preview = preview_rust_target(&config, &registry, name, profile);
                        print!("--- project ---\n{}", render_rust_target_preview(&preview));
                    }
                }
            } else {
                let name = parsed
                    .project_name
                    .as_deref()
                    .ok_or("exactly one of --project or --all is required")?;
                let preview = preview_rust_target(&config, &registry, name, profile);
                print!("{}", render_rust_target_preview(&preview));
            }
        }
        _ => return Err("invalid arguments; use --help for usage".into()),
    }
    Ok(())
}

struct ParsedDoctorArgs {
    config_path: PathBuf,
    json: bool,
}

fn run_doctor(args: &[String]) -> Result<(), Box<dyn Error>> {
    let parsed = parse_doctor_args(args)?;
    let report = match fs::read_to_string(parsed.config_path) {
        Ok(contents) => Config::from_toml(&contents)
            .map(|config| DoctorReport::from_config(&config))
            .unwrap_or_else(|_| DoctorReport::invalid_configuration()),
        Err(_) => DoctorReport::invalid_configuration(),
    };
    if parsed.json {
        println!(
            "{}",
            report
                .to_json()
                .map_err(|_| "could not serialize doctor report")?
        );
    } else {
        print!("{}", report.to_human());
    }
    Ok(())
}

fn parse_doctor_args(args: &[String]) -> Result<ParsedDoctorArgs, Box<dyn Error>> {
    let mut config_path = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" if config_path.is_none() => {
                let value = args.get(index + 1).ok_or("missing value for --config")?;
                config_path = Some(PathBuf::from(value));
                index += 2;
            }
            "--json" if !json => {
                json = true;
                index += 1;
            }
            _ => return Err("invalid arguments; use --help for usage".into()),
        }
    }
    Ok(ParsedDoctorArgs {
        config_path: config_path.ok_or("missing --config")?,
        json,
    })
}

struct ParsedNixStoreInspectionArgs {
    config_path: PathBuf,
    store_paths: Vec<PathBuf>,
    profile: Option<CleanupProfile>,
    json: bool,
}

fn run_nix_store_inspection(args: &[String]) -> Result<(), Box<dyn Error>> {
    let parsed = parse_nix_store_inspection_args(args)?;
    let config = Config::from_toml(&fs::read_to_string(parsed.config_path)?)?;
    let profile = parsed.profile.unwrap_or_else(|| config.default_profile());
    let report = nix_store::inspect(&mut CommandNixRunner, &config, profile, parsed.store_paths)?;
    if parsed.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|_| "could not serialize Nix store inspection")?
        );
    } else {
        print!("{}", report.to_human());
    }
    Ok(())
}

fn parse_nix_store_inspection_args(
    args: &[String],
) -> Result<ParsedNixStoreInspectionArgs, Box<dyn Error>> {
    let mut config_path = None;
    let mut store_paths = Vec::new();
    let mut profile = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" if config_path.is_none() => {
                config_path = Some(PathBuf::from(
                    args.get(index + 1).ok_or("missing value for --config")?,
                ));
                index += 2;
            }
            "--store-path" => {
                store_paths.push(PathBuf::from(
                    args.get(index + 1)
                        .ok_or("missing value for --store-path")?,
                ));
                index += 2;
            }
            "--profile" if profile.is_none() => {
                profile = Some(parse_profile(
                    args.get(index + 1).ok_or("missing value for --profile")?,
                )?);
                index += 2;
            }
            "--json" if !json => {
                json = true;
                index += 1;
            }
            "--config" | "--profile" | "--json" => {
                return Err("duplicate argument".into());
            }
            _ => return Err("invalid arguments; use --help for usage".into()),
        }
    }
    if store_paths.is_empty() {
        return Err("missing required argument: --store-path".into());
    }
    Ok(ParsedNixStoreInspectionArgs {
        config_path: config_path.ok_or("missing required argument: --config")?,
        store_paths,
        profile,
        json,
    })
}

struct ParsedNixGenerationInspectionArgs {
    config_path: PathBuf,
    nix_profile: String,
    generation_ids: Vec<u32>,
    profile: Option<CleanupProfile>,
    json: bool,
}

fn run_nix_generation_inspection(args: &[String]) -> Result<(), Box<dyn Error>> {
    let parsed = parse_nix_generation_inspection_args(args)?;
    let config = Config::from_toml(&fs::read_to_string(parsed.config_path)?)?;
    let profile = parsed.profile.unwrap_or_else(|| config.default_profile());
    let report = nix_generations::inspect_profile(
        &mut CommandGenerationRunner::default(),
        &config,
        profile,
        &parsed.nix_profile,
        &parsed.generation_ids,
    )?;
    if parsed.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|_| "could not serialize Nix generation inspection")?
        );
    } else {
        print!("{}", report.to_human());
    }
    Ok(())
}

fn parse_nix_generation_inspection_args(
    args: &[String],
) -> Result<ParsedNixGenerationInspectionArgs, Box<dyn Error>> {
    let mut config_path = None;
    let mut nix_profile = None;
    let mut generation_ids = Vec::new();
    let mut profile = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" if config_path.is_none() => {
                config_path = Some(PathBuf::from(
                    args.get(index + 1).ok_or("missing value for --config")?,
                ));
                index += 2;
            }
            "--nix-profile" if nix_profile.is_none() => {
                nix_profile = Some(
                    args.get(index + 1)
                        .ok_or("missing value for --nix-profile")?
                        .clone(),
                );
                index += 2;
            }
            "--generation" => {
                let id = parse_generation_id(
                    args.get(index + 1)
                        .ok_or("missing value for --generation")?,
                )?;
                if generation_ids.contains(&id) {
                    return Err("duplicate generation id".into());
                }
                generation_ids.push(id);
                index += 2;
            }
            "--profile" if profile.is_none() => {
                profile = Some(parse_profile(
                    args.get(index + 1).ok_or("missing value for --profile")?,
                )?);
                index += 2;
            }
            "--json" if !json => {
                json = true;
                index += 1;
            }
            "--config" | "--nix-profile" | "--profile" | "--json" => {
                return Err("duplicate argument".into());
            }
            _ => return Err("invalid arguments; use --help for usage".into()),
        }
    }
    Ok(ParsedNixGenerationInspectionArgs {
        config_path: config_path.ok_or("missing required argument: --config")?,
        nix_profile: nix_profile.ok_or("missing required argument: --nix-profile")?,
        generation_ids,
        profile,
        json,
    })
}

struct ParsedPreviewArgs {
    config_path: PathBuf,
    project_name: Option<String>,
    all: bool,
    state_path: Option<PathBuf>,
    profile: Option<CleanupProfile>,
    json: bool,
}

fn run_project_command(command: &str, args: &[String]) -> Result<(), Box<dyn Error>> {
    let parsed = parse_project_args(args, command == "list")?;
    let path = match parsed.state_path {
        Some(path) => path,
        None => state::default_state_path()?,
    };
    match command {
        "register" => {
            let name = parsed.name.ok_or("missing required argument: --name")?;
            let input_path = parsed.path.ok_or("missing required argument: --path")?;
            let mode = parsed.mode.ok_or("missing required argument: --mode")?;
            let canonical = fs::canonicalize(input_path)
                .map_err(|_| "project path must be an existing directory")?;
            if !canonical.is_dir() {
                return Err("project path must be an existing directory".into());
            }
            state::update(&path, |registry| registry.register(name, canonical, mode))?;
        }
        "unregister" => {
            if parsed.path.is_some() || parsed.mode.is_some() {
                return Err("invalid arguments; use --help for usage".into());
            }
            let name = parsed.name.ok_or("missing required argument: --name")?;
            state::update(&path, |registry| registry.unregister(&name).map(|_| ()))?;
        }
        "list" => {
            if parsed.name.is_some() || parsed.path.is_some() || parsed.mode.is_some() {
                return Err("invalid arguments; use --help for usage".into());
            }
            for (name, project) in state::load(&path)?.list() {
                println!(
                    "{}\t{}\t{}",
                    escape(name),
                    escape(&project.path().to_string_lossy()),
                    match project.mode() {
                        hule::ProjectMode::Persistent => "persistent",
                        hule::ProjectMode::Ephemeral => "ephemeral",
                    }
                );
            }
        }
        _ => return Err("invalid arguments; use --help for usage".into()),
    }
    Ok(())
}

struct ParsedProjectArgs {
    name: Option<String>,
    path: Option<PathBuf>,
    mode: Option<hule::ProjectMode>,
    state_path: Option<PathBuf>,
}

fn parse_project_args(args: &[String], list: bool) -> Result<ParsedProjectArgs, Box<dyn Error>> {
    let mut parsed = ParsedProjectArgs {
        name: None,
        path: None,
        mode: None,
        state_path: None,
    };
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if list && option != "--state" {
            return Err("invalid arguments; use --help for usage".into());
        }
        let value = args.get(index + 1).ok_or("missing value for argument")?;
        match option {
            "--name" if parsed.name.is_none() => parsed.name = Some(value.clone()),
            "--path" if parsed.path.is_none() => parsed.path = Some(PathBuf::from(value)),
            "--mode" if parsed.mode.is_none() => {
                parsed.mode = Some(match value.as_str() {
                    "persistent" => hule::ProjectMode::Persistent,
                    "ephemeral" => hule::ProjectMode::Ephemeral,
                    _ => return Err("invalid project mode".into()),
                })
            }
            "--state" if parsed.state_path.is_none() => {
                parsed.state_path = Some(PathBuf::from(value))
            }
            _ => return Err("invalid arguments; use --help for usage".into()),
        }
        index += 2;
    }
    if !list && ((parsed.name.is_none()) || (parsed.path.is_some() != parsed.mode.is_some())) {
        return Err("missing or invalid project arguments".into());
    }
    Ok(parsed)
}

struct ParsedNixPlan {
    config: PathBuf,
    output: PathBuf,
    paths: Vec<PathBuf>,
    profile: Option<CleanupProfile>,
}
fn run_nix_plan(args: &[String]) -> Result<(), Box<dyn Error>> {
    let p = parse_nix_plan(args)?;
    let config = Config::from_toml(&fs::read_to_string(&p.config)?)?;
    let profile = p.profile.unwrap_or_else(|| config.default_profile());
    let mut runner = CommandNixRunner;
    let plan = nix_store::create_plan(&mut runner, &config, profile, p.paths)?;
    write_new(&p.output, &plan)?;
    println!(
        "Planned {} explicit store objects and {} closure objects. Closure queries may open, create, or migrate Nix store metadata; no objects or roots are intentionally changed.",
        plan.inputs.len(),
        plan.closure.len()
    );
    for path in plan.closure {
        println!("{}", escape(&path.to_string_lossy()));
    }
    Ok(())
}
fn parse_nix_plan(args: &[String]) -> Result<ParsedNixPlan, Box<dyn Error>> {
    let (mut config, mut output, mut profile) = (None, None, None);
    let mut paths = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if flag == "--store-path" {
            let value = args.get(i + 1).ok_or("missing value")?;
            paths.push(PathBuf::from(value));
            i += 2;
            continue;
        }
        let value = args.get(i + 1).ok_or("missing value")?;
        match flag {
            "--config" if config.is_none() => config = Some(PathBuf::from(value)),
            "--output" if output.is_none() => output = Some(PathBuf::from(value)),
            "--profile" if profile.is_none() => profile = Some(parse_profile(value)?),
            _ => return Err("invalid arguments; use --help for usage".into()),
        }
        i += 2;
    }
    Ok(ParsedNixPlan {
        config: config.ok_or("missing --config")?,
        output: output.ok_or("missing --output")?,
        paths,
        profile,
    })
}
struct ParsedNixGenerationPlan {
    config: PathBuf,
    output: PathBuf,
    nix_profile: String,
    generation_ids: Vec<u32>,
    profile: Option<CleanupProfile>,
}

struct ParsedDockerPurgePlan {
    config: PathBuf,
    docker_cli: PathBuf,
    endpoint: String,
    output: PathBuf,
    profile: Option<CleanupProfile>,
}

fn run_docker_purge_plan(args: &[String]) -> Result<(), Box<dyn Error>> {
    let parsed = parse_docker_purge_plan(args)?;
    let config = Config::from_toml(&fs::read_to_string(&parsed.config)?)?;
    let profile = parsed.profile.unwrap_or_else(|| config.default_profile());
    let mut runner = CommandDockerPurgeRunner;
    let plan = docker_purge::create_plan(
        &mut runner,
        &config,
        profile,
        &parsed.endpoint,
        &parsed.docker_cli,
    )?;
    write_docker_purge_new(&parsed.output, &plan)?;
    println!(
        "Planned explicit Docker system purge for endpoint {}; Engine ID {}; Docker CLI {}; Engine {}; client API {}; server API {}.",
        escape(&plan.endpoint),
        escape(&plan.identity.engine_id),
        escape(&plan.identity.docker_cli_version),
        escape(&plan.identity.engine_version),
        escape(&plan.identity.client_api_version),
        escape(&plan.identity.server_api_version),
    );
    println!(
        "Saved dynamic scope: stopped containers, unused networks, all unused images, build cache, and unused anonymous volumes. No per-resource preview, rollback, or size estimate is claimed."
    );
    Ok(())
}

fn parse_docker_purge_plan(args: &[String]) -> Result<ParsedDockerPurgePlan, Box<dyn Error>> {
    let (mut config, mut docker_cli, mut endpoint, mut output, mut profile) =
        (None, None, None, None, None);
    let mut selected = false;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if flag == "--docker-purge" {
            if selected {
                return Err("duplicate argument: --docker-purge".into());
            }
            selected = true;
            index += 1;
            continue;
        }
        let value = args.get(index + 1).ok_or("missing value")?;
        match flag {
            "--config" if config.is_none() => config = Some(PathBuf::from(value)),
            "--docker-cli" if docker_cli.is_none() => docker_cli = Some(PathBuf::from(value)),
            "--docker-endpoint" if endpoint.is_none() => endpoint = Some(value.clone()),
            "--output" if output.is_none() => output = Some(PathBuf::from(value)),
            "--profile" if profile.is_none() => profile = Some(parse_profile(value)?),
            _ => return Err("invalid arguments; use --help for usage".into()),
        }
        index += 2;
    }
    if !selected {
        return Err("missing required flag: --docker-purge".into());
    }
    Ok(ParsedDockerPurgePlan {
        config: config.ok_or("missing --config")?,
        docker_cli: docker_cli.ok_or("missing --docker-cli")?,
        endpoint: endpoint.ok_or("missing --docker-endpoint")?,
        output: output.ok_or("missing --output")?,
        profile,
    })
}

fn run_nix_generation_plan(args: &[String]) -> Result<(), Box<dyn Error>> {
    let parsed = parse_nix_generation_plan(args)?;
    let config = Config::from_toml(&fs::read_to_string(&parsed.config)?)?;
    let cleanup_profile = parsed.profile.unwrap_or_else(|| config.default_profile());
    let mut runner = CommandGenerationRunner::default();
    let plan = nix_generations::create_plan(
        &mut runner,
        &config,
        cleanup_profile,
        &parsed.nix_profile,
        &parsed.generation_ids,
    )?;
    write_generation_new(&parsed.output, &plan)?;
    println!(
        "Planned {} explicit Nix generation IDs from profile {}; only rollback links are selected, store objects are retained, and no activation or GC is performed.",
        plan.selected_ids.len(),
        escape(&plan.nix_profile),
    );
    for id in &plan.selected_ids {
        if let Some(generation) = plan.history.iter().find(|item| item.id == *id) {
            println!(
                "generation {id}: link {} -> {}",
                escape(&generation.link.to_string_lossy()),
                escape(&generation.target.to_string_lossy())
            );
        }
    }
    Ok(())
}

fn parse_nix_generation_plan(args: &[String]) -> Result<ParsedNixGenerationPlan, Box<dyn Error>> {
    let (mut config, mut output, mut nix_profile, mut profile) = (None, None, None, None);
    let mut generation_ids = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args.get(index + 1).ok_or("missing value")?;
        match flag {
            "--config" if config.is_none() => config = Some(PathBuf::from(value)),
            "--output" if output.is_none() => output = Some(PathBuf::from(value)),
            "--nix-profile" if nix_profile.is_none() => nix_profile = Some(value.clone()),
            "--profile" if profile.is_none() => profile = Some(parse_profile(value)?),
            "--generation" => {
                let id = parse_generation_id(value)?;
                if generation_ids.contains(&id) {
                    return Err("duplicate generation id".into());
                }
                if generation_ids.len() >= nix_generations::MAX_GENERATION_IDS {
                    return Err("too many generations selected".into());
                }
                generation_ids.push(id);
            }
            _ => return Err("invalid arguments; use --help for usage".into()),
        }
        index += 2;
    }
    if generation_ids.is_empty() {
        return Err("at least one --generation is required".into());
    }
    generation_ids.sort_unstable();
    Ok(ParsedNixGenerationPlan {
        config: config.ok_or("missing --config")?,
        output: output.ok_or("missing --output")?,
        nix_profile: nix_profile.ok_or("missing --nix-profile")?,
        generation_ids,
        profile,
    })
}

fn parse_generation_id(value: &str) -> Result<u32, Box<dyn Error>> {
    if value.is_empty()
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("invalid generation id".into());
    }
    let id = value.parse::<u32>().map_err(|_| "invalid generation id")?;
    if id == 0 || id > i32::MAX as u32 {
        return Err("invalid generation id".into());
    }
    Ok(id)
}

fn parse_profile(value: &str) -> Result<CleanupProfile, Box<dyn Error>> {
    Ok(match value {
        "low" => CleanupProfile::Low,
        "normal" => CleanupProfile::Normal,
        "aggressive" => CleanupProfile::Aggressive,
        _ => return Err("invalid profile".into()),
    })
}
fn run_apply(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (mut plan_path, mut config_path, mut profile) = (None, None, None);
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = args.get(i + 1).ok_or("missing value")?;
        match flag {
            "--plan" if plan_path.is_none() => plan_path = Some(PathBuf::from(value)),
            "--config" if config_path.is_none() => config_path = Some(PathBuf::from(value)),
            "--profile" if profile.is_none() => profile = Some(parse_profile(value)?),
            _ => return Err("invalid arguments; use --help for usage".into()),
        }
        i += 2;
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(nix_store::NixError::NonInteractive.into());
    }
    let plan_path = plan_path.ok_or("missing --plan")?;
    let action = read_action_plan(&plan_path)?;
    if let ActionPlan::DockerPurge(plan) = &action {
        validate_docker_purge_location(&plan_path, plan)?;
    }
    let config_path = config_path.ok_or("missing --config")?;
    match action {
        ActionPlan::NixStore(plan) => run_apply_nix_store(&config_path, profile, plan),
        ActionPlan::NixGenerations(plan) => run_apply_nix_generations(&config_path, profile, plan),
        ActionPlan::DockerPurge(plan) => run_apply_docker_purge(&config_path, profile, plan),
    }
}

fn run_apply_docker_purge(
    config_path: &std::path::Path,
    profile: Option<CleanupProfile>,
    plan: DockerPurgeActionPlan,
) -> Result<(), Box<dyn Error>> {
    let load = || -> Result<(Config, CleanupProfile), DockerPurgeError> {
        let contents =
            fs::read_to_string(config_path).map_err(|_| DockerPurgeError::InvalidConfiguration)?;
        let config = Config::from_toml(&contents).map_err(docker_purge::config_load_error)?;
        let selected_profile = profile.unwrap_or_else(|| config.default_profile());
        Ok((config, selected_profile))
    };
    let mut runner = CommandDockerPurgeRunner;
    docker_purge::apply_with_confirmation(&mut runner, load, &plan, |plan| {
        let confirmation = docker_purge::confirmation_phrase(plan)?;
        println!(
            "Docker system prune will dynamically remove stopped containers, unused networks, all unused images, build cache, and unused anonymous volumes from the selected daemon. Named volumes are excluded by the required Engine API 1.42+ behavior. Resources can change after this summary; native prune may partially mutate before an error and cannot be rolled back."
        );
        println!(
            "Endpoint: {}; Engine ID: {}; CLI {}; Engine {}; client API {}; server API {}.",
            escape(&plan.endpoint),
            escape(&plan.identity.engine_id),
            escape(&plan.identity.docker_cli_version),
            escape(&plan.identity.engine_version),
            escape(&plan.identity.client_api_version),
            escape(&plan.identity.server_api_version),
        );
        print!("Type {confirmation} to continue: ");
        io::stdout()
            .flush()
            .map_err(|_| DockerPurgeError::Interactive)?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|_| DockerPurgeError::Interactive)?;
        Ok(answer.trim_end_matches(['\r', '\n']) == confirmation)
    })?;
    println!(
        "Docker system prune completed. The daemon selected resources dynamically; no per-resource removal or reclaimed-size guarantee is made."
    );
    Ok(())
}

fn run_apply_nix_store(
    config_path: &std::path::Path,
    profile: Option<CleanupProfile>,
    plan: hule::action_plan::NixStoreActionPlan,
) -> Result<(), Box<dyn Error>> {
    let load = || -> Result<Config, Box<dyn Error>> {
        Ok(Config::from_toml(&fs::read_to_string(config_path)?)?)
    };
    let config = load()?;
    let selected = profile.unwrap_or_else(|| config.default_profile());
    let mut runner = CommandNixRunner;
    nix_store::revalidate(&mut runner, &config, selected, &plan)?;
    println!(
        "Nix store delete will request these exact objects (Nix may reject live paths; GC bookkeeping may change; operation may be partial and is not atomic):"
    );
    for path in &plan.closure {
        println!("{}", escape(&path.to_string_lossy()));
    }
    let confirmation = nix_confirmation(&plan.closure);
    print!("Type {confirmation} to continue: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 || answer.trim() != confirmation {
        return Err(nix_store::NixError::Declined.into());
    }
    let fresh = load()?;
    let fresh_profile = profile.unwrap_or_else(|| fresh.default_profile());
    let result = nix_store::apply(&mut runner, &fresh, fresh_profile, &plan, |_| Ok(true))?;
    for item in &result.paths {
        let status = match item.status {
            nix_store::PathApplyStatus::Removed => "removed",
            nix_store::PathApplyStatus::RetainedOrNotDeleted => "retained_or_not_deleted",
            nix_store::PathApplyStatus::AbsentAfterFailure => "absent_after_failure",
            nix_store::PathApplyStatus::Unknown => "unknown",
        };
        println!("{}: {status}", escape(&item.path.to_string_lossy()));
    }
    if !result.command_succeeded {
        return Err(nix_store::NixError::Command.into());
    }
    if !result.is_complete() {
        return Err(nix_store::NixError::Incomplete.into());
    }
    println!("Nix delete command completed; no reclaimed-size or atomicity claim is made.");
    Ok(())
}

fn run_apply_nix_generations(
    config_path: &std::path::Path,
    profile: Option<CleanupProfile>,
    plan: hule::action_plan::NixGenerationActionPlan,
) -> Result<(), Box<dyn Error>> {
    let load = || -> Result<Config, Box<dyn Error>> {
        Ok(Config::from_toml(&fs::read_to_string(config_path)?)?)
    };
    let config = load()?;
    let selected = profile.unwrap_or_else(|| config.default_profile());
    let mut runner = CommandGenerationRunner::default();
    nix_generations::revalidate(&mut runner, &config, selected, &plan)?;
    println!(
        "Only these noncurrent rollback links are selected. Store objects remain; no Home Manager/nix-darwin activation, store-object deletion, or GC is performed. Concurrent profile/history writers or Home Manager activation are not serialized by Hule."
    );
    for id in &plan.selected_ids {
        if let Some(generation) = plan.history.iter().find(|item| item.id == *id) {
            println!(
                "generation {id}: {}",
                escape(&generation.link.to_string_lossy())
            );
        }
    }
    let confirmation = nix_generation_confirmation(&plan)?;
    print!("Type {confirmation} to continue: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 || answer.trim() != confirmation {
        return Err(GenerationError::Declined.into());
    }
    let fresh = load()?;
    let fresh_profile = profile.unwrap_or_else(|| fresh.default_profile());
    let result = nix_generations::apply(&mut runner, &fresh, fresh_profile, &plan, |_| Ok(true))?;
    for item in &result.statuses {
        let status = match item.status {
            GenerationStatus::Removed => "removed",
            GenerationStatus::RetainedOrNotDeleted => "retained_or_not_deleted",
            GenerationStatus::AbsentAfterFailure => "absent_after_failure",
            GenerationStatus::Unknown => "unknown",
        };
        println!("generation {}: {status}", item.id);
    }
    if !result.command_succeeded || !result.is_complete() {
        return Err(GenerationError::Incomplete.into());
    }
    println!(
        "Selected generation links were removed; store objects remain. No reclaimed-size or atomicity claim is made."
    );
    Ok(())
}

fn nix_generation_confirmation(
    plan: &hule::action_plan::NixGenerationActionPlan,
) -> Result<String, Box<dyn Error>> {
    let bytes = serde_json::to_vec(plan).map_err(|_| "could not bind confirmation")?;
    let hash = bytes
        .into_iter()
        .fold(0xcbf29ce484222325u64, |mut hash, byte| {
            hash ^= u64::from(byte);
            hash.wrapping_mul(0x100000001b3)
        });
    Ok(format!(
        "PRUNE-GENERATIONS {} {hash:016x}",
        plan.selected_ids.len()
    ))
}

fn nix_confirmation(paths: &[PathBuf]) -> String {
    // This is a confirmation binding, not a security signature or plan authorization.
    let mut hash = 0xcbf29ce484222325u64;
    for path in paths {
        for byte in path
            .to_string_lossy()
            .as_bytes()
            .iter()
            .copied()
            .chain([0xff])
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("DELETE {} {hash:016x}", paths.len())
}

fn escape(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

fn parse_preview_args(args: &[String]) -> Result<ParsedPreviewArgs, Box<dyn Error>> {
    let mut config = None;
    let mut project = None;
    let mut all = false;
    let mut state = None;
    let mut profile = None;
    let mut json = false;
    let mut index = 0;

    while index < args.len() {
        let option = args[index].as_str();
        if option == "--all" {
            if all {
                return Err("duplicate argument: --all".into());
            }
            all = true;
            index += 1;
            continue;
        }
        if option == "--json" {
            if json {
                return Err("duplicate argument: --json".into());
            }
            json = true;
            index += 1;
            continue;
        }
        let value = args.get(index + 1).ok_or("missing value for argument")?;
        match option {
            "--config" if config.is_none() => config = Some(PathBuf::from(value)),
            "--project" if project.is_none() => project = Some(value.clone()),
            "--state" if state.is_none() => state = Some(PathBuf::from(value)),
            "--profile" if profile.is_none() => {
                profile = Some(match value.as_str() {
                    "low" => CleanupProfile::Low,
                    "normal" => CleanupProfile::Normal,
                    "aggressive" => CleanupProfile::Aggressive,
                    _ => return Err("invalid profile".into()),
                });
            }
            "--config" | "--project" | "--state" | "--profile" | "--all" | "--json" => {
                return Err(format!("duplicate argument: {option}").into());
            }
            _ => return Err("unknown argument".into()),
        }
        index += 2;
    }

    if project.is_some() == all {
        return Err("exactly one of --project or --all is required".into());
    }

    Ok(ParsedPreviewArgs {
        config_path: config.ok_or("missing required argument: --config")?,
        project_name: project,
        all,
        state_path: state,
        profile,
        json,
    })
}
