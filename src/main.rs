use hule::{
    CleanupProfile,
    action_plan::{read_plan, write_new},
    config::Config,
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

const HELP: &str = "Usage:\n  hule --help\n  hule --version\n  hule config preset NAME\n  hule plan nix-store --config PATH --store-path PATH... --output PLAN [--profile low|normal|aggressive]\n  hule apply --plan PATH --config PATH [--profile low|normal|aggressive]\n  hule project register --name NAME --path PATH --mode persistent|ephemeral [--state PATH]\n  hule project list [--state PATH]\n  hule project unregister --name NAME [--state PATH]\n  hule preview rust-target --config PATH (--project NAME | --all) [--state PATH] [--profile low|normal|aggressive] [--json]\n\nFor Rust-target preview Cargo metadata (`cargo metadata --no-deps --offline --locked`) may be invoked only when each project is registered, rust_target is enabled for the selected profile, and the anticipated target is not already protected. Cargo documentation does not formally guarantee absence of all side effects or code execution. Nix store planning queries requisites for explicitly named objects and can open/create/migrate store metadata. Apply requires an interactive confirmation and uses only the saved closure with nix-store --delete --store daemon; Nix performs native liveness checks and may refuse or partially complete. No broad GC or direct filesystem removal is performed.\n";

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
        [command, subcommand, ..] if command == "plan" && subcommand == "nix-store" => {
            run_nix_plan(&args[2..])?;
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
    let plan = read_plan(&plan_path.ok_or("missing --plan")?)?;
    let config_path = config_path.ok_or("missing --config")?;
    let load = || -> Result<Config, Box<dyn Error>> {
        Ok(Config::from_toml(&fs::read_to_string(&config_path)?)?)
    };
    let config = load()?;
    let selected = profile.unwrap_or_else(|| config.default_profile());
    let mut runner = CommandNixRunner;
    nix_store::revalidate(&mut runner, &config, selected, &plan)?;
    println!(
        "Nix store delete will request these exact objects (Nix may reject live paths; GC bookkeeping may change; operation may be partial and is not atomic):"
    );
    for p in &plan.closure {
        println!("{}", escape(&p.to_string_lossy()));
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
