# Hule

Hule is a Rust command-line cleanup tool with explicit retention policy, read-only Rust-target previews, and narrowly scoped Nix store and generation plan/apply providers. Nix generation support passed its native matrix on Nix 2.34.6 in an isolated OrbStack guest and passed independent safety review; other Nix versions remain unsupported. This is guest-only evidence, not validation or cleanup of host profiles. Generation-history removal is not transactional, concurrent Nix writers and Home Manager activation are not serialized, and no reclaimed-byte guarantee is made. Destructive Nix actions require a saved selection, TTY confirmation, and revalidation; other cleanup providers remain unimplemented.

## Current status

The CLI and configuration parser are implemented. A Nix flake defines the `hule` package for aarch64-darwin, x86_64-darwin, aarch64-linux, and x86_64-linux, plus a Rust development shell. It uses nixpkgs-unstable for aarch64-darwin/Linux and a separate nixpkgs 26.05-darwin input for x86_64-darwin, the last Nixpkgs release supporting that architecture. The Home Manager package list has a local-checkout wrapper. The package build and CLI `--help` run have passed from this checkout. Neither the flake nor wrapper is activated; no Home Manager activation or Nix-darwin switch has been run. You can also build locally with Rust/Cargo:

```sh
cargo build
```

This builds the local executable; it does not install Hule system-wide or activate the Home Manager wrapper. The configured wrapper runs `nix run "path:$HOME/www/personal-projects/hule" -- "$@"` after checking that the checkout contains `flake.nix`. No Home Manager activation or Nix-darwin switch has been run.

Available commands:

```text
hule --help
hule --version
hule config preset NAME
hule project register --name NAME --path PATH --mode persistent|ephemeral [--state PATH]
hule project list [--state PATH]
hule project unregister --name NAME [--state PATH]
hule preview rust-target --config PATH (--project NAME | --all) [--state PATH] [--profile low|normal|aggressive] [--json]
hule plan nix-store --config PATH --store-path PATH... --output PLAN [--profile low|normal|aggressive]
hule plan nix-generations --config PATH --nix-profile NAME --generation ID... --output PLAN [--profile low|normal|aggressive]
hule apply --plan PATH --config PATH [--profile low|normal|aggressive]
```

Exactly one of `--project NAME` and `--all` is required. `--all` previews only registered projects, in project-name order; each human report is delimited and includes that project's discovery and retention result. An empty human preview prints `no registered projects` and succeeds without invoking Cargo. `--profile` is optional and otherwise the configuration's `default_profile` is used. `--json` emits one pretty, read-only plan document for either selection, including schema version, profile, category, per-project discovery/decision entries, and `authorization: not_authorized`; it never writes a plan or authorizes cleanup. Cargo metadata (`cargo metadata --no-deps --format-version=1 --offline --locked --manifest-path <registered-root>/Cargo.toml`) may be invoked only for a registered project when `rust_target` is enabled for the selected profile and the anticipated target is not already protected. It may discover Cargo's `target_directory` only when it is an existing real directory contained within the registered project root. It does not enumerate target contents, request a build or clean, or delete anything. Cargo's documentation does not formally guarantee that metadata has no side effects or executes no code. The Rust preview performs no cleanup or deletion; unsupported metadata, paths, and errors are reported generically. `retention-eligible` means only that retention rules passed; it is not authorization to exclude or remove anything. Nix actions use separate strict, versioned action-plan JSON; the Rust informational JSON is never accepted by apply.

## Configuration

Schema version 2 is TOML policy only: it contains profiles, categories, and additional keep paths. Project registrations live separately in version-1 state TOML. Project names may contain ASCII letters, digits, `_`, and `-`; project paths must be absolute non-root paths without traversal, and registered roots may not overlap. Unknown fields, invalid profiles/categories and incomplete profile configuration are rejected without echoing parsed input.

```toml
schema_version = 2
default_profile = "normal"
additional_keep_paths = ["/Users/alex/important-data"]

[profiles.low]
categories = []

[profiles.normal]
categories = ["rust_target"]

[profiles.aggressive]
categories = ["rust_target"]

```

Every profile (`low`, `normal`, `aggressive`) must explicitly list categories; empty lists disable all categories. `rust_target`, `nix_store_gc`, `nix_user_generations`, and `nix_home_manager_generations` are implemented categories; all remain explicitly opt-in per profile. Presets leave Nix categories disabled. Category opt-in only enables the corresponding narrowly scoped provider; it does not replace saved-plan confirmation and revalidation. Generation profiles must be named explicitly under `nix_profiles`; a user profile path must be the current user's XDG `nix/profiles/profile` or Home Manager's `nix/profiles/home-manager` path. Home Manager profiles also require the supported `current-home` activation-root paths. `additional_keep_paths` may be empty; paths overlapping a project's `target` protect it under every profile. Project registration is explicit in the separate state registry; an unregistered project is skipped and no target path is constructed for it.

`hule config preset minimal` prints a complete schema-v2 policy with empty category lists. `hule config preset developer` enables `rust_target` in every profile. Presets only print TOML to stdout; they do not write or activate files. The optional `--state PATH` selects a separate registry file for project commands and preview; without it, state uses `XDG_STATE_HOME/hule/projects.toml`, falling back to `$HOME/.local/state/hule/projects.toml` if XDG_STATE_HOME is missing, empty, or relative. An absent state file is empty. Register requires an existing directory and stores its canonical path and retention mode only; it does not scan directory contents. Project roots may not overlap. List emits escaped tab-separated name, path, and mode fields. Unregister removes only the explicitly named registration. Explicit legacy migration remains deferred; inline registrations are not imported.

## Retention decisions

- **Protected:** configured keep paths take precedence for all profiles.
- **Persistent:** kept by `low` and `normal`; eligible for further checks under `aggressive`.
- **Ephemeral:** eligible for further checks under all profiles.
- **Unregistered:** skipped; no project target path is constructed.

Eligibility is not deletion authorization. Hule invokes `cargo metadata --no-deps --offline --locked` (also passing format-version and manifest-path arguments), does not request a build or clean, does not enumerate target contents, and executes no cleanup. Cargo documentation does not formally guarantee that the subprocess has no side effects or executes no code; Hule does not invoke `cargo clean` or `cargo build`.

## Policy decisions

- Presets generate explicit TOML; execution is to use saved lists rather than rediscovering targets from a preset.
- High-impact categories are opt-in.
- Cargo's shared cache is preserved by default; aggressive Cargo-cache cleanup is opt-in.
- M1 is limited to selected store objects and does not imply user-only store scope. M2 generation planning/apply is accepted after the isolated native matrix and independent safety review, but only for Nix 2.34.6. Concurrent Nix writers/Home Manager activation are not serialized; removal is not transactional and no reclaimed-byte guarantee is made.
- Docker volume cleanup is opt-in.
- There are no default age thresholds; age-based decisions require reliable provider-specific usage information.

These are policy decisions, not authorization beyond the provider-specific boundaries described here. Hule currently implements Rust-target metadata discovery/retention preview, the narrow Nix store plan/apply path, and a selected-generation provider accepted only for Nix 2.34.6 after isolated native acceptance and independent safety review. Nix store cleanup is limited to explicitly supplied top-level store objects and their saved forward-requisites closure; shared requisites are deduplicated deterministically and each path is strictly validated. Plan queries may open/create/migrate Nix store metadata; they do not intentionally change objects or roots. A plan output must have an existing parent and may not overlap the store, including through a canonicalized parent alias. Apply requires stdin and stdout TTYs and a confirmation bound to the saved plan (`DELETE N HASH` for M1; `PRUNE-GENERATIONS N HASH` for M2), rechecks policy and the saved identity, and invokes only the provider-specific selected Nix operation. M1 invokes `nix-store --delete --store daemon <saved paths>`. Each path receives a post-command status: `removed` requires successful command completion, a pre-delete presence validation, and post-delete absence; `retained_or_not_deleted` means it remains, without claiming why; `absent_after_failure` reports absence after a failed command; and unverifiable state is `unknown`. A failed delete remains a nonzero CLI result even if some paths were removed. Nix's liveness/referrer checks remain authoritative. This operation is not atomic: Nix bookkeeping can change and a failure may follow partial deletion; no size-reclaimed claim is made. Store version/path/daemon are only logical identity evidence, not a persistent physical-store identity guarantee. This is not a user-only store: a daemon can represent a shared/system store. No system generation selection/pruning, inventory, root query, build, broad GC, sudo, direct filesystem removal, or liveness override is used.

## Roadmap and limitations

The Rust-target provider discovers Cargo's target directory using `cargo metadata --no-deps --offline --locked` and validates its existence, symlink status and containment. Cargo documentation does not guarantee zero subprocess side effects or code execution; the command does not request build/clean, and Hule does not enumerate target contents. This Rust-target path does not check process/resource activity or execute cleanup. `cargo clean` is not an accepted apply mechanism: Cargo's default clean removes the whole target directory, and inspected Cargo 1.95.0 source skips the output lock for broad cleans and may also remove a separate build directory. These version-specific findings do not establish safe behavior on other versions; Rust-target apply remains unsupported until scope, ownership, quiescence, and revalidation are independently established. Migration, Rust apply, Docker, shared-cache, macOS, application, and browser providers are not implemented. Nix store planning/apply and selected generation planning/apply are implemented with the limitations above; M2 is accepted only for Nix 2.34.6, the sole version that passed the isolated native matrix and independent safety review. Generation deletion is not transactional: concurrent Nix history writers and Home Manager activation/current-home changes are not serialized by Hule, and detected drift cannot undo a prior link removal. No GC follows, and no reclaimed-byte guarantee is made. Project registration/list/unregister are implemented. The library provides a separately persisted project registry with strict parsing, atomic save, and exclusive locking; CLI updates serialize load/validate/mutate/write under that lock. `state::save` replaces the entire state with the supplied snapshot; use `state::update` for read-modify-write operations to avoid overwriting concurrent updates. State writes use a persistent `projects.toml.lock` sidecar (created with private permissions) and Rust's standard file-locking API; the OS releases the lock when its handle closes. The sidecar is intentionally not removed. On Unix, locking refuses a final state directory writable by group or others and does not change existing directory permissions. Sidecar identity is checked across opening to reduce replacement races, but neither advisory locking nor directory permissions protect against a malicious same-UID local attacker; ACLs are also not a security barrier guaranteed by these checks. Building requires Rust 1.89 or newer for `File::try_lock`/`File::unlock`. Rust-target reports remain informational; Nix store and generation commands also report their saved selections and per-object/per-generation apply outcomes.

M1 has passed independent review. `just fmt`, `just pre-push`, `git diff --check`, and the explicitly invoked native integration test (`cargo test --all-features --test nix_store_local -- --ignored --nocapture`) passed. The native fixture exercises Hule `create_plan` and `apply` with `CommandNixRunner` against an explicit `local?root=<canonical TempDir>` URI; its test-only API is feature-gated and never exposed through CLI arguments, environment, or saved plans. The fixture verifies a live rooted object is retained, a synthetic unrooted object is removed, and an unselected sentinel remains. Tests do not use `daemon`, access `/nix/store`, or run GC/build/sudo. The Nix package build and CLI `--help` run have been validated; Home Manager has not been activated. See [PLAN.md](PLAN.md) for the full safety requirements and phased checklist. No real machine-wide cleanup should be used for validation.
