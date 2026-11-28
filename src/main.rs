//! `deliver` — the Delivery Boy CLI.
//!
//! Exit codes: 0 ok · 1 step/verify failure · 2 config or usage error.

mod completions;
mod config;
mod configdiff;
mod deployers;
mod detect;
mod exec;
mod fleet;
mod interrupt;
mod lock;
mod logs;
mod notifications;
mod plan;
mod preflight;
mod readback;
mod release;
mod remote;
mod rollback;
mod secrets;
mod shipping;
mod source;
mod symlink_swap;
mod ui;
mod verify;
mod version;
mod workflow;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::ArgValueCompleter;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "deliver",
    version,
    about = "Delivery Boy — config-driven deploys"
)]
struct Cli {
    /// Path to the config file (default: search up from the current directory)
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,

    /// Deploy to this target instead of `defaults.target` (a service that
    /// names its own `target:` keeps it)
    #[arg(long, global = true, value_name = "NAME", add = ArgValueCompleter::new(completions::targets))]
    target: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

/// The two global flags every config-reading command shares: which config to
/// load, and which of its targets to aim at.
#[derive(Clone, Copy)]
struct ConfigArgs<'a> {
    path: Option<&'a Path>,
    target: Option<&'a str>,
}

#[derive(Subcommand)]
enum SecretsAction {
    /// Check every secret the config needs (default)
    Check,
    /// Store a keychain item, reading the value from stdin
    Set {
        /// Keychain service name, e.g. sample-sparkle-private
        service: String,
        /// Keychain account (default: release)
        #[arg(long, default_value = "release")]
        account: String,
    },
}

/// What `deliver fleet` runs in each repo. Three read/act commands, not the
/// whole surface: the fleet file answers "is everything ready", "ship it", and
/// "what is live" — the questions that stop making sense one repo at a time.
#[derive(Subcommand)]
enum FleetAction {
    /// Run preflight in every repo
    Preflight {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
    },
    /// Deploy every repo, in fleet-file order
    Deploy {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
        /// Walk every step without running it
        #[arg(long)]
        dry_run: bool,
        /// Don't prompt: accept the tag on HEAD (fails if there isn't one)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Carry on to the remaining repos after one fails
        #[arg(long)]
        keep_going: bool,
        /// Take each target's deploy lock even if another run holds it
        #[arg(long)]
        force: bool,
    },
    /// Show what is live across the fleet
    Status {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
    },
}

#[derive(Subcommand)]
enum Commands {
    /// Print a shell completion script (source it in your shell startup file)
    Completions {
        #[arg(value_enum)]
        shell: completions::Shell,
    },
    /// Prepare, check and deploy retained artifacts with the Azure release adapter
    Release {
        #[command(subcommand)]
        action: release::Action,
    },
    /// Inspect a repo, report how it's deployed, and scaffold a config
    Init {
        /// Repo to inspect (default: current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Deploy host for the scaffold
        #[arg(long)]
        host: Option<String>,
        /// Remote directory for the scaffold
        #[arg(long)]
        dir: Option<String>,
        /// Write .deliver.yml without prompting
        #[arg(long)]
        write: bool,
        /// Overwrite an existing .deliver.yml
        #[arg(long)]
        force: bool,
        /// Scaffold from a GitHub Actions workflow (e.g. .github/workflows/deploy.yml)
        #[arg(long, value_name = "PATH")]
        from_workflow: Option<PathBuf>,
    },
    /// Compile and print the plan — executes nothing
    Plan {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
        /// Preview this release version without prompting (e.g. 0.4.1)
        #[arg(long)]
        version: Option<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Execute the plan
    Deploy {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
        /// Walk every step without running it
        #[arg(long)]
        dry_run: bool,
        /// Don't prompt: accept the tag on HEAD (fails if there isn't one)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Create/use this release version without prompting (e.g. 0.4.1)
        #[arg(long)]
        version: Option<String>,
        /// Take the target's deploy lock even if another run holds it
        #[arg(long)]
        force: bool,
        /// Skip every service's `soak:` window (the checks still run once)
        #[arg(long)]
        no_soak: bool,
    },
    /// Run only the verify checks
    Verify {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
    },
    /// Show what is live on the target right now
    Status {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// List the deploys recorded on the target, newest first
    History {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
        /// How many deploys to show per service (0 for all)
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Tail the deployed logs from the target
    Logs {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
        /// Keep the stream open (one service at a time)
        #[arg(long, short = 'f')]
        follow: bool,
        /// How many existing lines to show first
        #[arg(long, short = 'n', default_value_t = 50)]
        tail: usize,
        /// Emit one JSON object per log line (NDJSON)
        #[arg(long)]
        json: bool,
    },
    /// Roll back to the previous release (repoints the live symlink)
    Rollback {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
        /// Roll back to this retained deploy id instead of one step back
        /// (`deliver history` lists the ids)
        #[arg(long, value_name = "DEPLOY_ID")]
        to: Option<String>,
        /// Take the target's deploy lock even if another run holds it
        #[arg(long)]
        force: bool,
        /// Don't prompt before rolling back
        #[arg(long, short = 'y')]
        yes: bool,
        /// Read the target and show what would be rolled back, changing nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Run the preflight checks only (tools, input files, ssh reachability)
    Preflight {
        #[arg(long, add = ArgValueCompleter::new(completions::services))]
        service: Vec<String>,
    },
    /// Show which secrets this config needs and whether they're in place
    Secrets {
        #[command(subcommand)]
        action: Option<SecretsAction>,
    },
    /// Run one command across every repo in deliver.fleet.yml
    Fleet {
        /// Path to the fleet file (default: search up from the current directory)
        #[arg(long)]
        fleet: Option<PathBuf>,
        /// Only these repos (by directory name or the path as written)
        #[arg(long)]
        repo: Vec<String>,
        #[command(subcommand)]
        action: FleetAction,
    },
    /// Remove this app's build artifacts from the system temp dir
    Clean,
    /// Schema-check .deliver.yml
    Validate,
    /// Print the JSON Schema for .deliver.yml (for editors)
    Schema,
}

fn main() {
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .var(completions::ENV)
        .complete();
    let cli = Cli::parse();
    let code = match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            // The last boundary: an error chain can carry a value picked up
            // from a subprocess or a context string.
            eprintln!("error: {}", secrets::redact::scrub_error(&err));
            2
        }
    };
    std::process::exit(code);
}

fn run(cli: &Cli) -> Result<i32> {
    let args = ConfigArgs {
        path: cli.config.as_deref(),
        target: cli.target.as_deref(),
    };
    match &cli.command {
        Commands::Completions { shell } => {
            completions::generate(*shell, &mut std::io::stdout().lock())?;
            Ok(0)
        }
        Commands::Release { action } => release::run(args.path, args.target, action),
        Commands::Init {
            path,
            host,
            dir,
            write,
            force,
            from_workflow: Some(workflow),
        } => cmd_init_from_workflow(
            path,
            workflow,
            host.as_deref(),
            dir.as_deref(),
            *write,
            *force,
        ),
        Commands::Init {
            path,
            host,
            dir,
            write,
            force,
            from_workflow: None,
        } => cmd_init(path, host.as_deref(), dir.as_deref(), *write, *force),
        Commands::Plan {
            service,
            version,
            json,
        } => cmd_plan(args, service, version.as_deref(), *json),
        Commands::Deploy {
            service,
            dry_run,
            yes,
            version,
            force,
            no_soak,
        } => cmd_deploy(
            args,
            service,
            *dry_run,
            false,
            *yes,
            version.as_deref(),
            *force,
            *no_soak,
        ),
        Commands::Verify { service } => {
            cmd_deploy(args, service, false, true, true, None, false, true)
        }
        Commands::Status { service, json } => cmd_readback(args, service, Readback::Status, *json),
        Commands::History {
            service,
            limit,
            json,
        } => cmd_readback(args, service, Readback::History { limit: *limit }, *json),
        Commands::Logs {
            service,
            follow,
            tail,
            json,
        } => cmd_logs(args, service, *follow, *tail, *json),
        Commands::Rollback {
            service,
            to,
            force,
            yes,
            dry_run,
        } => {
            let how = RollbackRun {
                force_lock: *force,
                assume_yes: *yes,
                dry_run: *dry_run,
            };
            match to {
                Some(deploy_id) => cmd_rollback_to(args, service, deploy_id, how),
                None => cmd_rollback(args, service, how),
            }
        }
        Commands::Preflight { service } => cmd_preflight(args, service),
        Commands::Fleet {
            fleet,
            repo,
            action,
        } => cmd_fleet(
            cli.config.as_deref(),
            cli.target.as_deref(),
            fleet.as_deref(),
            repo,
            action,
        ),
        Commands::Secrets { action } => cmd_secrets(args, action),
        Commands::Clean => cmd_clean(args),
        Commands::Validate => cmd_validate(args),
        Commands::Schema => {
            print!("{}", config::SCHEMA);
            Ok(0)
        }
    }
}

fn cmd_init(
    path: &Path,
    host: Option<&str>,
    dir: Option<&str>,
    write: bool,
    force: bool,
) -> Result<i32> {
    ui::banner();
    let root = path.canonicalize()?;
    let app = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "app".into());
    ui::phase("Inspecting repository");
    ui::detail(root.display().to_string());
    let findings = detect::detect(&root);

    if findings.is_empty() {
        println!("No known deploy strategy detected.");
        println!(
            "Supported today: {}",
            deployers::known_deployers().join(", ")
        );
        return Ok(0);
    }

    let host = host.unwrap_or("CHANGEME.example.com");
    let dir = dir
        .map(str::to_string)
        .unwrap_or_else(|| format!("/var/universal/{app}"));

    ui::phase("Detected deploy strategies");
    for f in &findings {
        let status = match f.deployer {
            Some(d) => format!("→ deployer: {d}"),
            None => "→ no deployer yet (informational)".to_string(),
        };
        println!("  • {}  {status}", f.evidence);
        // What the scaffold could not work out for itself. Saying it here, next
        // to the evidence, is the difference between a config that is ready and
        // one that only looks ready. The verify note rides along: a scaffolded
        // check the operator never looks at is how a check ends up failing a
        // good release.
        let verify_note = detect::verify_note(f, &app, &dir);
        for note in f.notes.iter().chain(verify_note.iter()) {
            println!(
                "      ! {}",
                note.replace("{app}", &app).replace("{host}", host)
            );
        }
    }
    let yaml = detect::scaffold(&app, host, &dir, &findings);
    offer_write(&root, &yaml, write, force)
}

/// Print the proposed config and write it — on `--write`, or on a yes at the
/// prompt. Never overwrites without `--force`.
fn offer_write(root: &Path, yaml: &str, write: bool, force: bool) -> Result<i32> {
    let dest = root.join(config::CONFIG_FILENAME);

    println!("\n--- {} (proposed) ---\n{yaml}", config::CONFIG_FILENAME);

    let exists = dest.exists();
    if exists && !force {
        println!(
            "{} already exists — pass --force to overwrite.",
            dest.display()
        );
        return Ok(0);
    }

    // --write skips the prompt; otherwise ask. EOF (no input, e.g. CI) means no,
    // so this never hangs, and `echo y | deliver init` works for scripting.
    let should_write = if write {
        true
    } else {
        match prompt_yes_no(&format!(
            "Write {}{}?",
            dest.display(),
            if exists {
                " (OVERWRITING existing file)"
            } else {
                ""
            }
        ))? {
            Some(answer) => answer,
            None => {
                println!("(no input — not writing; pass --write to save)");
                return Ok(0);
            }
        }
    };

    if !should_write {
        println!("Not written. Re-run with --write to save.");
        return Ok(0);
    }

    std::fs::write(&dest, yaml)?;
    println!("\nWrote {}", dest.display());
    println!("Review it, then run: deliver plan");
    Ok(0)
}

fn cmd_init_from_workflow(
    path: &Path,
    workflow_path: &Path,
    host: Option<&str>,
    dir: Option<&str>,
    write: bool,
    force: bool,
) -> Result<i32> {
    ui::banner();
    let root = path.canonicalize()?;
    let app = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "app".into());
    // As given (relative to where `deliver` runs), else relative to --path.
    let file = if workflow_path.exists() || workflow_path.is_absolute() {
        workflow_path.to_path_buf()
    } else {
        root.join(workflow_path)
    };
    ui::phase("Reading workflow");
    ui::detail(file.display().to_string());
    let text = std::fs::read_to_string(&file)
        .map_err(|e| anyhow::anyhow!("cannot read workflow {}: {e}", file.display()))?;
    let import =
        workflow::import(&text).map_err(|e| anyhow::anyhow!("{}: {e:#}", file.display()))?;

    // An image build is the docker-compose deployer's job, and plain `init`
    // already knows how to scaffold that from the compose file.
    let compose = import.image_build.as_ref().and_then(|_| {
        detect::detect(&root)
            .into_iter()
            .find(|f| f.deployer == Some("docker-compose"))
    });

    let host_given = host.is_some();
    let host = host
        .map(str::to_string)
        .or_else(|| import.host.clone())
        .unwrap_or_else(|| "CHANGEME.example.com".into());
    let user = import.user.clone().unwrap_or_else(|| "root".into());
    let port = import.port.unwrap_or(22);
    let dir = dir
        .map(str::to_string)
        .unwrap_or_else(|| format!("/var/universal/{app}"));

    ui::phase("Mapped");
    if import.mapped.is_empty() && compose.is_none() {
        println!("  (nothing)");
    }
    for step in &import.mapped {
        println!("  • {step}");
    }
    if let (Some(step), Some(f)) = (&import.image_build, &compose) {
        println!("  • {step}  → docker-compose service `{}`", f.service);
    }
    if !import.skipped.is_empty() {
        ui::phase("Skipped (CI runner setup — your machine already has it)");
        for step in &import.skipped {
            println!("  • {step}");
        }
    }

    let mut unmapped = import.unmapped.clone();
    if let (Some(step), None) = (&import.image_build, &compose) {
        unmapped.push(workflow::Unmapped {
            step: step.clone(),
            reason: "builds an image, and the repo has no compose file for the docker-compose \
                     deployer to build it from"
                .into(),
        });
    }
    let count = unmapped.len();
    ui::phase(&format!(
        "{count} step{} I could not map",
        if count == 1 { "" } else { "s" }
    ));
    for u in &unmapped {
        println!("  • {}\n      ! {}", u.step, u.reason);
    }

    let mut notes = import.notes.clone();
    if !host_given && import.host.is_none() {
        notes.push(
            "the workflow's ssh host was not a literal — pass --host, or edit targets.production.host"
                .into(),
        );
    }
    if let Some(f) = &compose {
        let restarts: Vec<&str> = import
            .jobs
            .iter()
            .filter(|j| {
                j.steps.iter().any(|s| {
                    matches!(s, workflow::Step::Ssh(cmd)
                        if cmd.contains("docker compose") || cmd.contains("docker-compose"))
                })
            })
            .map(|j| j.name.as_str())
            .collect();
        if !restarts.is_empty() {
            notes.push(format!(
                "`{}` still runs the workflow's own compose commands over ssh, and the `{}` \
                 service now does that job — keep one",
                restarts.join("`, `"),
                f.service
            ));
        }
    }
    if !import.connection_secrets.is_empty() {
        notes.push(format!(
            "not declared: {} — the workflow used them to reach the box or a registry; `deliver` \
             uses your own ssh config and docker login",
            import
                .connection_secrets
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !import.secrets.is_empty() {
        notes.push(
            "declared secrets are read from the environment — run `deliver secrets` to see which \
             are missing"
                .into(),
        );
    }
    if !notes.is_empty() {
        ui::phase("Notes");
        for note in &notes {
            println!("  ! {note}");
        }
    }

    let source = workflow_path.display().to_string();
    let yaml = workflow::scaffold(
        &source,
        &app,
        &host,
        &user,
        port,
        &dir,
        &import,
        compose.as_ref(),
    );
    offer_write(&root, &yaml, write, force)
}

/// The repo root is the directory holding the config (a `.deliveryboy/` config
/// lives one level down, so climb out of it).
fn repo_root(config_path: &Path) -> PathBuf {
    let dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    if dir.file_name().is_some_and(|n| n == ".deliveryboy") {
        return dir.parent().unwrap_or(Path::new(".")).to_path_buf();
    }
    dir
}

/// Ask a yes/no question. `Ok(None)` means stdin gave us nothing (EOF) — the
/// caller decides, rather than us blocking or guessing.
fn prompt_yes_no(question: &str) -> Result<Option<bool>> {
    use std::io::Write;
    print!("\n{question} [y/N] ");
    std::io::stdout().flush()?;

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        println!();
        return Ok(None);
    }
    Ok(Some(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    )))
}

/// Ask for a line of input. `Ok(None)` on EOF (non-interactive), so this never
/// blocks a scripted run.
fn prompt_line(question: &str) -> Result<Option<String>> {
    use std::io::Write;
    print!("{question} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        println!();
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

/// Offer the next version, based on the previous release. Returns the bare
/// version (no prefix) to tag, or None to cancel. `taken` says whether a bare
/// version's tag already exists (its name and commit), so a number another
/// commit already carries is skipped rather than offered.
fn choose_version(
    previous: Option<&str>,
    taken: &dyn Fn(&str) -> Option<(String, String)>,
) -> Result<Option<String>> {
    use version::SemVer;

    let parsed = previous.and_then(SemVer::parse);
    match (&parsed, previous) {
        (Some(_), Some(tag)) => ui::detail(format!("HEAD has no tag. Previous release: {tag}")),
        (None, Some(tag)) => ui::detail(format!(
            "HEAD has no tag. Previous tag {tag} isn't a version number."
        )),
        _ => ui::detail("HEAD has no tag, and there are no previous releases."),
    }

    // Offer patch/minor/major off the previous release, each past any number
    // whose tag is already taken; otherwise a sane first cut.
    let options: Vec<version::Offer> = match &parsed {
        Some(v) => version::offer_bumps(v, taken),
        None => vec![version::Offer {
            kind: "first release",
            version: "0.1.0".to_string(),
            note: None,
        }],
    };

    println!();
    for (i, offer) in options.iter().enumerate() {
        let prefix = parsed
            .as_ref()
            .map(|p| p.prefix.clone())
            .unwrap_or_else(|| "v".into());
        let why = offer
            .note
            .as_ref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        println!(
            "  {}) {:<13} {prefix}{}{why}",
            i + 1,
            offer.kind,
            offer.version
        );
    }
    println!("  {}) custom", options.len() + 1);
    println!("  (blank to cancel)");

    let answer = match prompt_line(&format!("Choose [1-{}]:", options.len() + 1))? {
        Some(a) => a,
        None => return Ok(None), // no stdin — caller explains
    };
    if answer.is_empty() {
        return Ok(None);
    }
    let choice: usize = match answer.parse() {
        Ok(n) if n >= 1 && n <= options.len() + 1 => n,
        // Typing a version directly is a reasonable thing to do.
        _ => {
            return Ok(Some(answer.trim_start_matches('v').to_string()));
        }
    };
    if choice == options.len() + 1 {
        return Ok(prompt_line("Version (e.g. 1.2.3):")?
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_start_matches('v').to_string()));
    }
    Ok(Some(options[choice - 1].version.clone()))
}

/// What `resolve_release` decided.
enum Resolved {
    /// Deploy this release.
    Release(version::DeployVersion),
    /// A person declined, or nobody was there to choose: not an error.
    Cancelled,
    /// A release guard refused the number: nothing ran, and the exit is 2.
    Refused,
}

/// The release a `--version` or a chosen number names, with the repo's tag
/// prefix (`v`) put back the way `previous_tag` shows it.
fn release_named(root: &Path, v: version::DeployVersion, wanted: &str) -> version::DeployVersion {
    let prefix = version::previous_tag(root)
        .as_deref()
        .and_then(version::SemVer::parse)
        .map(|parsed| parsed.prefix)
        .unwrap_or_else(|| "v".to_string());
    v.with_release(
        format!("{prefix}{}", wanted.trim_start_matches('v')),
        "argument",
    )
}

/// Say when the tag this release would get already names another commit. A
/// preview (`plan`, `--dry-run`) only warns. A deploy with `--version`
/// releases the commit a local tag names (see `release_commit`), and refuses
/// one only the remote has (see `resolve_release`).
fn warn_if_tag_taken(root: &Path, v: &version::DeployVersion, tag_cfg: Option<&config::TagConfig>) {
    let name = version::tag_name(v, tag_cfg);
    let check = version::check_tag(root, &name, &v.git.sha, tag_cfg);
    if let Some(warning) = check.warning {
        ui::warn(warning);
    }
    match check.clash {
        Some(version::TagClash::Local { commit }) => ui::warn(format!(
            "tag {name} already names {} — `deliver deploy --version` releases that commit, \
             not this plan's {}",
            &commit[..7.min(commit.len())],
            v.git.short_sha
        )),
        Some(clash) => ui::warn(format!(
            "{} — a deploy with this number would be refused",
            clash.refusal(&name)
        )),
        None => {}
    }
}

/// Resolve which release is being deployed.
///
/// A deploy ships a tagged commit: find the tag on HEAD (confirmed later, in
/// `shipping_approved`), or offer to create one. Non-interactive runs never block — an existing tag is accepted,
/// and a missing one is an error rather than a silent untagged deploy.
fn resolve_release(
    root: &Path,
    v: version::DeployVersion,
    tag_cfg: Option<&config::TagConfig>,
    assume_yes: bool,
    version_arg: Option<&str>,
    dry_run: bool,
) -> Result<Resolved> {
    // A dry run is a preview: never prompt, never create a tag, never block.
    // With --version it shows the complete release plan; without one it shows
    // the untagged guards that a real deploy would hit.
    if dry_run {
        if let Some(wanted) = version_arg {
            let v = release_named(root, v, wanted);
            warn_if_tag_taken(root, &v, tag_cfg);
            return Ok(Resolved::Release(v));
        }
        return Ok(Resolved::Release(v));
    }
    // `from: commit` / `commit-count` are explicit choices — nothing to resolve.
    if v.release_source != "tag" {
        return Ok(Resolved::Release(v));
    }

    // A tag on HEAD is confirmed later, by `shipping_approved`, once the
    // target has been read and the prompt can say what the release ships.
    // `--version` is the operator's answer already: the release is the number
    // given, built from the commit its tag names (see `release_commit`), even
    // when that commit carries other tags too.
    if v.release.is_some() && version_arg.is_none() {
        return Ok(Resolved::Release(v));
    }

    // No tag on HEAD — offer the next versions based on the previous release,
    // skipping any number whose tag another commit already carries.
    let previous = version::previous_tag(root);
    let taken = |bare: &str| {
        let candidate = release_named(root, v.clone(), bare);
        let name = version::tag_name(&candidate, tag_cfg);
        version::tag_commit(root, &name)
            .filter(|commit| *commit != v.git.sha)
            .map(|commit| (name, commit))
    };
    let wanted = match version_arg {
        Some(v) => Some(v.to_string()),
        None if assume_yes => None,
        None => choose_version(previous.as_deref(), &taken)?,
    };
    let Some(wanted) = wanted else {
        ui::note(
            "no release to deploy — tag the commit (`git tag -a v0.4.1 -m v0.4.1`), \
             pass --version, or set versioning.from to commit/commit-count.",
        );
        return Ok(Resolved::Cancelled);
    };

    if v.git.dirty {
        ui::note("working tree is dirty — commit before tagging a release.");
        return Ok(Resolved::Cancelled);
    }

    // Carry the requested release through the deploy without mutating git.
    // If configured, the tag is created and pushed only after every deploy and
    // verify step succeeds, so failed releases do not leave false tags behind.
    let v = release_named(root, v, &wanted);

    // The number must be free before anything ships: once a release is live
    // under it, it is in the target's history, in every `{version}`, and in
    // the artifacts it named — and `tag_release` can only say "not tagged".
    let name = version::tag_name(&v, tag_cfg);
    let check = version::check_tag(root, &name, &v.git.sha, tag_cfg);
    if let Some(warning) = check.warning {
        ui::warn(warning);
    }
    if let Some(clash) = check.clash {
        ui::phase("Aborted");
        ui::note(clash.refusal(&name));
        ui::note("nothing was built, shipped, or changed.");
        return Ok(Resolved::Refused);
    }
    Ok(Resolved::Release(v))
}

/// The release `deliver verify` checks: the one live on the target.
///
/// `verify` never ships, so a tag on HEAD is beside the point — it runs on a
/// feature branch, after pulling an untagged commit, or after a deploy that
/// used `--version` without tagging. `{version}` in its checks means what the
/// target records as live; when the target records nothing (a deployer with no
/// read-back) or services disagree, HEAD's own release stands, which is
/// `untagged` on an untagged commit.
fn live_release(
    config: &config::Config,
    only: &[String],
    root: &Path,
    v: version::DeployVersion,
) -> Result<version::DeployVersion> {
    let requests = readback::collect_releases(&plan::build(config, only, root, &v)?);
    let statuses = if requests.is_empty() {
        Vec::new()
    } else {
        readback::read(requests, &config.targets)
    };
    let mut live: Vec<String> = statuses
        .iter()
        .filter_map(readback::Status::live_deploy)
        .map(|d| d.release.clone())
        .filter(|r| !r.is_empty() && r != "untagged")
        .collect();
    live.sort();
    live.dedup();
    match live.as_slice() {
        [] => {
            ui::detail(format!(
                "live release:   none recorded on the target — checking as {}",
                v.release_display()
            ));
            Ok(v)
        }
        [one] => {
            ui::detail(format!("live release:   {one} (read from the target)"));
            let release = one.clone();
            Ok(v.with_release(release, "live"))
        }
        several => {
            ui::detail(format!(
                "live release:   services disagree ({}) — checking as {}",
                several.join(", "),
                v.release_display()
            ));
            Ok(v)
        }
    }
}

fn cmd_secrets(explicit: ConfigArgs<'_>, action: &Option<SecretsAction>) -> Result<i32> {
    ui::banner();
    let (config, _path) = load_announced(explicit)?;

    if let Some(SecretsAction::Set { service, account }) = action {
        ui::phase(&format!("Storing {service}"));
        ui::detail("reading the value from stdin (it is never echoed or logged)");
        let mut value = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut value)?;
        let value = value.trim_end_matches(['\n', '\r']);
        if value.is_empty() {
            ui::fail("no value on stdin");
            ui::note("usage:  deliver secrets set <service> < key.pem   (or pipe it in)");
            return Ok(2);
        }
        match secrets::set_keychain(service, account, value) {
            Ok(()) => {
                ui::ok(format!("stored {service} (account {account})"));
                Ok(0)
            }
            Err(e) => {
                ui::fail(e);
                Ok(2)
            }
        }
    } else {
        ui::phase("Secrets");
        let reqs = secrets::requirements(&config);
        let resolver = secrets::resolver(&config, &repo_root(&_path))?;
        let declared = secrets::declared_status(&resolver);
        let exposed = resolver.file_hygiene();
        if reqs.is_empty() && declared.is_empty() && exposed.is_empty() {
            ui::detail("this config needs no secrets");
            return Ok(0);
        }
        let mut missing = 0;
        if !declared.is_empty() {
            ui::detail(format!("providers: {}", resolver.describe()));
            for status in &declared {
                match (&status.provider, status.required) {
                    (Some(p), _) => ui::ok(format!("{} — found in {p}", status.name)),
                    (None, false) => ui::detail(format!("○ {} — not set (optional)", status.name)),
                    (None, true) => {
                        missing += 1;
                        ui::fail(format!("{} — not found in any provider", status.name));
                    }
                }
            }
        }
        for req in &reqs {
            let status = secrets::check(req);
            if status.ok {
                ui::ok(format!("{} — {}", req.label, status.detail));
            } else {
                missing += 1;
                ui::fail(format!("{} — {}", req.label, status.detail));
                ui::detail(format!("  used by {}", req.used_by));
                if let Some(remedy) = status.remedy {
                    ui::detail(format!("  fix: {remedy}"));
                }
            }
        }
        let tracked = report_exposure(&exposed);
        ui::phase(match (missing, tracked) {
            (0, 0) => "Done",
            (0, _) => "Secrets file in git",
            _ => "Missing secrets",
        });
        if missing == 0 && tracked == 0 {
            ui::ok("everything this config needs is in place");
            return Ok(0);
        }
        if missing > 0 {
            ui::note(format!(
                "{missing} secret(s) missing — a deploy would fail partway."
            ));
        }
        if tracked > 0 {
            ui::note("a committed secrets file — deploy refuses to run until it is untracked.");
        }
        Ok(2)
    }
}

/// Runs `git worktree prune` in the checkout when dropped, on every path out
/// of `cmd_clean`.
struct Prune(PathBuf);

impl Drop for Prune {
    fn drop(&mut self) {
        source::prune(&self.0);
    }
}

/// Remove the local work directory. Deploys clean up after themselves on
/// success; this is for when one failed and left artifacts behind.
fn cmd_clean(explicit: ConfigArgs<'_>) -> Result<i32> {
    ui::banner();
    let path = config::resolve(explicit.path)?;
    let config = config::load(&path)?;
    let scratch = version::scratch_root(&config.app);
    // A release killed mid-run leaves its clean copy registered as a git
    // worktree; once the directory below is gone, git can forget it.
    let _forget = Prune(repo_root(&path));
    ui::phase("Cleaning");
    ui::detail(format!("scratch: {}", scratch.display()));
    if !scratch.exists() {
        ui::detail("nothing to remove");
        return Ok(0);
    }
    let runs = std::fs::read_dir(&scratch).map(|d| d.count()).unwrap_or(0);
    std::fs::remove_dir_all(&scratch)?;
    ui::ok(format!(
        "removed {runs} run director{}",
        if runs == 1 { "y" } else { "ies" }
    ));
    Ok(0)
}

fn cmd_validate(explicit: ConfigArgs<'_>) -> Result<i32> {
    ui::banner();
    let (config, path) = load_announced(explicit)?;
    let root = repo_root(&path);
    let v = version_announced(&config, &root, None);
    ui::phase("Validating");

    // Compile every service, including `enabled: false` ones: passing every
    // service name as `only` overrides `enabled: false` so a broken disabled
    // service is caught here rather than when someone runs `--service`.
    let all: Vec<String> = config.services.keys().cloned().collect();
    let compiled = plan::compile(&config, &all, &root, &v)?;

    let mut failures = 0usize;
    for err in &compiled.errors {
        if err
            .error
            .chain()
            .any(|cause| cause.is::<secrets::Unresolved>())
        {
            ui::warn(format!("{err} — run `deliver secrets` to check providers"));
        } else if err
            .error
            .chain()
            .any(|cause| cause.is::<deployers::MissingInput>())
        {
            ui::warn(format!(
                "{err} — run `deliver preflight` to check local files"
            ));
        } else if err
            .error
            .chain()
            .any(|cause| cause.is::<deployers::UnsupportedHost>())
        {
            ui::warn(err.to_string());
        } else {
            failures += 1;
            ui::fail(err.to_string());
        }
    }

    if failures > 0 {
        ui::note("fix the above — deliver plan would refuse this config");
        return Ok(2);
    }

    ui::ok("schema and references are valid");
    ui::detail(format!(
        "would deploy as {} · {}",
        v.id,
        v.release_display()
    ));
    println!(
        "OK — {}: {} service(s), {} target(s) [{}]",
        config.app,
        config.services.len(),
        config.targets.len(),
        path.display()
    );
    Ok(0)
}

/// Phase 0: find and load the config, reporting what was picked up — and,
/// with `--target`, where it is aimed, before anything says what it will do.
fn load_announced(explicit: ConfigArgs<'_>) -> Result<(config::Config, PathBuf)> {
    ui::phase("Loading configuration");
    let path = config::resolve(explicit.path)?;
    ui::detail(format!("config: {}", path.display()));
    let mut config = config::load(&path)?;
    let pinned = match explicit.target {
        Some(name) => config.select_target(name)?,
        None => Vec::new(),
    };
    let target_names: Vec<&str> = config.targets.keys().map(|s| s.as_str()).collect();
    ui::detail(format!(
        "app: {} · {} service(s) · target(s): {}",
        config.app,
        config.services.len(),
        target_names.join(", ")
    ));
    if let Some(name) = explicit.target {
        ui::detail(format!("target: {name} (--target)"));
    }
    for (service, own) in &pinned {
        ui::note(format!(
            "service '{service}' pins target '{own}' — --target does not move it."
        ));
    }
    Ok((config, path))
}

/// Resolve this deploy's identity: Delivery Boy's own deploy id, plus the app's
/// release number. Reported up front so you know what you're about to ship.
/// `built_from` is the commit a release is built from (see `release_commit`);
/// `None` describes HEAD and the working tree.
fn version_announced(
    config: &config::Config,
    root: &Path,
    built_from: Option<&str>,
) -> version::DeployVersion {
    let from = config
        .versioning
        .as_ref()
        .and_then(|r| r.version_from.as_deref());
    let v = match built_from {
        Some(commit) => version::resolve_at(root, from, commit),
        None => version::resolve(root, from),
    };
    ui::phase("Versioning");
    ui::detail(format!("deploy version: {}", v.id));
    ui::detail(v.describe());
    if v.git.dirty {
        ui::detail("working tree is dirty — what ships won't match any commit");
    }
    v
}

/// Phase: secrets, immediately after the config is read.
///
/// Deliberately before versioning: resolving a release can *create a git tag*,
/// and tagging a commit for a deploy that can't succeed leaves a lie in your
/// history. Everything expensive (builds, notarization) comes later still.
fn secrets_announced(config: &config::Config, root: &Path) -> bool {
    let reqs = secrets::requirements(config);
    let resolver = match secrets::resolver(config, root) {
        Ok(r) => r,
        Err(e) => {
            ui::phase("Secrets");
            ui::fail(e.to_string());
            return false;
        }
    };
    let declared = secrets::declared_status(&resolver);
    let exposed = resolver.file_hygiene();
    if reqs.is_empty() && declared.is_empty() && exposed.is_empty() {
        return true;
    }
    ui::phase("Secrets");
    let mut ok = report_exposure(&exposed) == 0;
    if !declared.is_empty() {
        let found = declared.iter().filter(|d| d.provider.is_some()).count();
        ui::ok(format!(
            "{found}/{} declared secret(s) resolved",
            declared.len()
        ));
        for status in declared
            .iter()
            .filter(|d| d.provider.is_none() && d.required)
        {
            ok = false;
            ui::fail(format!("{} — not found in any provider", status.name));
        }
    }
    for req in &reqs {
        let status = secrets::check(req);
        if status.ok {
            ui::ok(status.detail);
        } else {
            ok = false;
            ui::fail(format!("{} — {}", req.label, status.detail));
            ui::detail(format!("  used by {}", req.used_by));
            if let Some(remedy) = status.remedy {
                ui::detail(format!("  fix: {remedy}"));
            }
        }
    }
    ok
}

/// Say which secrets files git can see. Returns how many are committed — the
/// kind that is already a leak, and stops a deploy.
fn report_exposure(exposed: &[secrets::Exposure]) -> usize {
    let mut tracked = 0;
    for exposure in exposed {
        match exposure {
            secrets::Exposure::Tracked(_) => {
                tracked += 1;
                ui::fail(exposure.message());
            }
            secrets::Exposure::Unignored(_) => ui::warn(exposure.message()),
        }
    }
    tracked
}

/// Phase: compile the plan, reporting its size. Files are read from `root`;
/// secrets from `secrets_root` — the checkout, when `root` is a release's
/// clean copy of its commit.
fn compile_announced(
    config: &config::Config,
    only: &[String],
    root: &Path,
    secrets_root: &Path,
    version: &version::DeployVersion,
) -> Result<Vec<plan::ServicePlan>> {
    ui::phase("Compiling plan");
    let compiled = plan::build_from(config, only, root, secrets_root, version)?;
    let steps: usize = compiled.iter().map(|sp| sp.steps.len()).sum();
    ui::detail(format!(
        "{} service(s), {steps} step(s): {}",
        compiled.len(),
        compiled
            .iter()
            .map(|sp| sp.service.as_str())
            .collect::<Vec<_>>()
            .join(" → ")
    ));
    // A failed verify check is what triggers the rollback `exec.rs` unwinds, so
    // a service with none can never roll itself back — it ships blind. That is
    // a legitimate choice for some services and an oversight for most, and the
    // difference is only visible if someone says it out loud.
    let unverified: Vec<&str> = compiled
        .iter()
        .filter(|sp| !sp.steps.iter().any(|s| s.label.starts_with("verify")))
        .map(|sp| sp.service.as_str())
        .collect();
    if !unverified.is_empty() {
        ui::note(format!(
            "no verification step: {} — a failed release there cannot roll itself back.",
            unverified.join(", ")
        ));
    }
    Ok(compiled)
}

/// What preflight should account for *beyond* the compiled plan.
///
/// Non-empty only for `deliver preflight`, which keeps going when the plan will
/// not compile so it can still report everything else wrong in the same pass.
#[derive(Default)]
struct Unplanned {
    /// Compile failures, reported as findings rather than as a fatal error.
    problems: Vec<String>,
    /// (target, host) pairs belonging to the services that failed.
    hosts: Vec<(String, String)>,
}

/// `compile_announced` for preflight: the same phase line, but a plan that will
/// not compile comes back as findings to report alongside the rest rather than
/// as an error that ends the run.
fn compile_reported(
    config: &config::Config,
    only: &[String],
    root: &Path,
    version: &version::DeployVersion,
) -> (Vec<plan::ServicePlan>, Unplanned) {
    ui::phase("Compiling plan");
    let compiled = match plan::compile(config, only, root, version) {
        Ok(c) => c,
        // Not a service's fault (an unreadable provider, a dependency cycle),
        // so there is no partial plan to go on — but the config's own checks
        // still run.
        Err(e) => {
            return (
                Vec::new(),
                Unplanned {
                    problems: vec![format!("plan will not compile — {e:#}")],
                    hosts: plan::unplanned_hosts(config, only, &[]),
                },
            )
        }
    };
    let steps: usize = compiled.plan.iter().map(|sp| sp.steps.len()).sum();
    ui::detail(format!(
        "{} service(s), {steps} step(s): {}",
        compiled.plan.len(),
        compiled
            .plan
            .iter()
            .map(|sp| sp.service.as_str())
            .collect::<Vec<_>>()
            .join(" → ")
    ));
    let unplanned = Unplanned {
        problems: compiled
            .errors
            .iter()
            .map(|e| format!("plan will not compile — {e}"))
            .collect(),
        hosts: plan::unplanned_hosts(config, only, &compiled.plan),
    };
    (compiled.plan, unplanned)
}

/// Phase: preflight. Returns false when it found problems.
///
/// `checkout` is set when `root` is a release's clean copy of its commit: a
/// file the config names that is missing from the commit but present in the
/// checkout is uncommitted or ignored by git, and the problem says so.
fn preflight_announced(
    config: &config::Config,
    compiled: &[plan::ServicePlan],
    root: &Path,
    check_remote: bool,
    unplanned: &Unplanned,
    checkout: Option<&Path>,
) -> bool {
    ui::phase("Preflight");
    if !check_remote {
        ui::detail("(dry run — skipping remote reachability)");
    }
    let mut report = preflight::run(config, compiled, root, check_remote, &unplanned.hosts);
    if let Some(checkout) = checkout {
        for problem in report.problems.iter_mut() {
            let only_here = problem
                .strip_prefix(preflight::MISSING_FILE)
                .is_some_and(|rel| checkout.join(rel).exists());
            if only_here {
                problem.push_str(
                    " — it is in this checkout but not in the release commit; a release ships \
                     only committed files, so commit it or have a step build it",
                );
            }
        }
    }
    // First, because everything after it is downstream of a plan that compiles.
    for problem in unplanned.problems.iter().rev() {
        report.problems.insert(0, problem.clone());
    }
    for line in &report.checked {
        ui::ok(line);
    }
    for line in &report.warnings {
        ui::warn(line);
    }
    for problem in &report.problems {
        ui::fail(problem);
    }

    report.ok()
}

fn cmd_plan(
    explicit: ConfigArgs<'_>,
    only: &[String],
    version_arg: Option<&str>,
    json: bool,
) -> Result<i32> {
    ui::banner();
    let (config, path) = load_announced(explicit)?;
    let root = repo_root(&path);
    let mut v = version_announced(&config, &root, None);
    if let Some(wanted) = version_arg {
        v = release_named(&root, v, wanted);
        ui::detail(format!("planned release: {}", v.release_display()));
        warn_if_tag_taken(
            &root,
            &v,
            config.versioning.as_ref().and_then(|r| r.tag.as_ref()),
        );
    }
    let compiled = compile_announced(&config, only, &root, &root, &v)?;
    if json {
        println!(
            "{}",
            secrets::redact::scrub(&serde_json::to_string_pretty(&compiled)?)
        );
    } else {
        ui::phase("Plan (nothing is executed)");
        println!("{}", plan::render(&compiled));
    }
    Ok(0)
}

/// Show what the release changes in the target's live infrastructure config,
/// and — when something does change — let the operator stop before it happens.
///
/// Returns false only when a human said no. Every failure mode here (an
/// unreadable file, an unreachable host, no stdin) prints and continues: this
/// is a window onto the deploy, not a new gate in front of it.
fn live_config_approved(
    config: &config::Config,
    plan: &[plan::ServicePlan],
    assume_yes: bool,
    dry_run: bool,
) -> bool {
    let pending = configdiff::collect(plan);
    if pending.is_empty() {
        return true;
    }
    ui::phase("Live config on the target");
    let changes = configdiff::read_live(pending, &config.targets);
    if let Some(report) = configdiff::render(&changes) {
        for line in report.lines() {
            ui::detail(line);
        }
    }
    if !configdiff::any_changes(&changes) {
        return true;
    }
    // A dry run never blocks (see `resolve_release`), and --yes already said so.
    if dry_run || assume_yes {
        return true;
    }
    match prompt_yes_no("Apply these changes to the live config?") {
        Ok(Some(answer)) => answer,
        // No stdin is not a refusal — it is CI, which chose this deploy already.
        Ok(None) | Err(_) => true,
    }
}

/// Show what the release ships — the commits between what each target has live
/// and HEAD — and, for a release found tagged on HEAD, ask before going on.
///
/// The range reads the same record `deliver status` does, so it covers the
/// services that keep one (files/hugo and docker-compose). Every failure mode
/// (an unreachable host, nothing recorded, a live commit this clone lacks)
/// prints a line and continues: this informs the confirmation, it is not a new
/// gate. Returns false only when a human said no.
fn shipping_approved(
    config: &config::Config,
    plan: &[plan::ServicePlan],
    root: &Path,
    v: &version::DeployVersion,
    selected_target: Option<&str>,
    assume_yes: bool,
    dry_run: bool,
) -> Result<bool> {
    let mut requests = readback::collect_releases(plan);
    // A dry run does not reach remote targets, for the same reason preflight
    // skips remote reachability there; a local target costs nothing to read.
    let before = requests.len();
    if dry_run {
        requests.retain(|r| config.targets.get(&r.target).is_some_and(|t| t.is_local()));
    }
    let skipped = before - requests.len();
    if before > 0 {
        ui::phase("Shipping");
    }
    if skipped > 0 {
        ui::detail("(dry run — not reading remote targets; `deliver status` reads them)");
    }
    let shipments = if requests.is_empty() {
        Vec::new()
    } else {
        let statuses = readback::read(requests, &config.targets);
        shipping::collect(root, &statuses, &v.git.sha)
    };
    for line in shipping::render(&shipments, v.git.dirty) {
        ui::detail(line);
    }

    // Only a tag found on HEAD is confirmed: a version picked at the prompt or
    // passed with --version is already the operator's answer, and --yes and a
    // dry run never block (see `resolve_release`).
    let tagged = v.release_source == "tag";
    let Some(tag) = v.release.as_deref().filter(|_| tagged) else {
        return Ok(true);
    };
    if assume_yes || dry_run {
        return Ok(true);
    }
    // With --target the operator chose *where*, so the question names it.
    let to = selected_target
        .map(|name| format!(" to {name}"))
        .unwrap_or_default();
    let question = match shipping::summary(&shipments) {
        Some(what) => format!("Deploy release {tag} ({}){to}, {what}?", v.git.short_sha),
        None => format!("Deploy release {tag} ({}){to}?", v.git.short_sha),
    };
    // No stdin: the tag exists and was chosen deliberately by tagging it.
    Ok(prompt_yes_no(&question)?.unwrap_or(true))
}

/// The commit a deploy builds and ships, or `None` to build in the working
/// tree.
///
/// A release numbered by a tag — the default `versioning.from` — is a commit:
/// the one `--version`'s tag already names (`deploy --version 1.2.3` after
/// tagging main), else HEAD's, which the release is tagged on once it is live.
/// `from: commit` and `commit-count` deploy the working tree as they always
/// have, as does a directory git does not track or a repo with no commits.
fn release_commit(
    config: &config::Config,
    root: &Path,
    version_arg: Option<&str>,
) -> Option<Commit> {
    let from = config
        .versioning
        .as_ref()
        .and_then(|r| r.version_from.as_deref());
    if matches!(from, Some("commit" | "commit-count")) {
        return None;
    }
    source::repo_prefix(root)?;
    let head = version::head_commit(root)?;
    let Some(wanted) = version_arg else {
        return Some(Commit {
            sha: head,
            tagged_as: None,
        });
    };
    let name = release_tag_name(config, root, &head, wanted);
    Some(match version::tag_commit(root, &name) {
        Some(sha) => Commit {
            sha,
            tagged_as: Some(name),
        },
        None => Commit {
            sha: head,
            tagged_as: None,
        },
    })
}

/// The commit a release is built from, and the tag that named it when
/// `--version` found one.
struct Commit {
    sha: String,
    tagged_as: Option<String>,
}

/// The tag `config` gives release `wanted` (its `tag.name` template).
fn release_tag_name(config: &config::Config, root: &Path, commit: &str, wanted: &str) -> String {
    let from = config
        .versioning
        .as_ref()
        .and_then(|r| r.version_from.as_deref());
    let named = release_named(root, version::resolve_at(root, from, commit), wanted);
    version::tag_name(
        &named,
        config.versioning.as_ref().and_then(|r| r.tag.as_ref()),
    )
}

/// Check out `commit` cleanly in Delivery Boy's scratch directory and move the
/// process into it, so every step builds and ships the commit and nothing
/// else. Returns the copy and the config as the commit has it, or `None` when
/// the run must stop (having said why).
///
/// The commit's own config, because how a release is built is part of the
/// release: an uncommitted edit to `.deliver.yml` in the checkout is no more
/// part of it than an uncommitted edit to the code. Secrets and relative ssh
/// keys are still read from the checkout (`anchor_local_paths`), where a
/// gitignored `.env.deploy` lives.
fn enter_release_copy(
    config: &config::Config,
    config_path: &Path,
    root: &Path,
    built_from: &Commit,
    version_arg: Option<&str>,
    target: Option<&str>,
    dry_run: bool,
) -> Result<Option<(source::Export, config::Config)>> {
    ui::phase("Release source");
    let commit = built_from.sha.as_str();
    let short = &commit[..7.min(commit.len())];
    if let Some(tag) = &built_from.tagged_as {
        ui::detail(format!("{tag} names commit {short}"));
    }
    let dir =
        version::scratch_root(&config.app).join(format!("source-{short}-{}", std::process::id()));
    let mut copy = source::Export::create(root, commit, &dir)?;
    ui::detail(format!(
        "commit {short}, checked out clean: {}",
        copy.root.display()
    ));
    let here = version::git_info(root);
    if here.sha != commit {
        let at = match here.branch.as_deref() {
            None | Some("HEAD") => "a detached HEAD".to_string(),
            Some(branch) => branch.to_string(),
        };
        ui::detail(format!(
            "this checkout is at {at} ({}) — that is not what ships",
            here.short_sha
        ));
    }
    if here.dirty {
        ui::detail("this checkout's uncommitted and untracked files are not part of the release");
    }

    let rel = config_path.strip_prefix(root).unwrap_or(config_path);
    let committed = copy.root.join(rel);
    if !committed.is_file() {
        ui::phase("Aborted");
        ui::note(format!(
            "{} is not in commit {short} — a release is built from its commit, config included; \
             commit it first.",
            rel.display()
        ));
        ui::note("nothing was built, shipped, or changed.");
        return Ok(None);
    }
    let mut release_config = config::load(&committed)?;
    if let Some(name) = target {
        release_config.select_target(name)?;
    }
    release_config.anchor_local_paths(root);
    // The commit was found by the tag this checkout's `tag.name` gives the
    // version; the release is tagged by the commit's own. Two names for one
    // release would leave the second pointing nowhere anyone looks.
    if let (Some(found), Some(wanted)) = (&built_from.tagged_as, version_arg) {
        let own = release_tag_name(&release_config, root, commit, wanted);
        if own != *found {
            ui::phase("Aborted");
            ui::note(format!(
                "{found} names commit {short}, but that commit's own config tags this release \
                 {own} — make the two `tag.name`s agree, or tag {short} as {own}."
            ));
            ui::note("nothing was built, shipped, or changed.");
            return Ok(None);
        }
    }
    if std::fs::read(config_path).ok() != std::fs::read(&committed).ok() {
        ui::note(format!(
            "using {} as commit {short} has it — this checkout's copy differs",
            rel.display()
        ));
        // Its secrets were checked; these are the ones the release will use.
        if !secrets_announced(&release_config, root) && !dry_run {
            ui::phase("Aborted");
            ui::note("secrets not ready — nothing was tagged, built, or changed.");
            return Ok(None);
        }
    }
    copy.enter()?;
    Ok(Some((copy, release_config)))
}

#[allow(clippy::too_many_arguments)]
fn cmd_deploy(
    explicit: ConfigArgs<'_>,
    only: &[String],
    dry_run: bool,
    verify_only: bool,
    assume_yes: bool,
    version_arg: Option<&str>,
    force_lock: bool,
    no_soak: bool,
) -> Result<i32> {
    ui::banner();
    let timer = ui::Timer::start();
    let (config, path) = load_announced(explicit)?;
    // Absolute before anything moves the working directory (`source::Export`).
    let path = std::path::absolute(&path)?;
    let root = repo_root(&path);

    // Before anything is tagged, built, or shipped.
    if !secrets_announced(&config, &root) && !dry_run {
        ui::phase("Aborted");
        ui::note("secrets not ready — nothing was tagged, built, or changed.");
        return Ok(2);
    }

    // A tagged release is built from a clean copy of its commit, whatever state
    // this checkout is in; from here on `config` is the commit's own, files are
    // read from `build_root`, and secrets and git from `root`. The copy lives
    // until this function returns, after the deploy lock is given back.
    let built_from = if verify_only {
        None
    } else {
        release_commit(&config, &root, version_arg)
    };
    let mut copy = None;
    let config = match &built_from {
        None => config,
        Some(commit) => {
            match enter_release_copy(
                &config,
                &path,
                &root,
                commit,
                version_arg,
                explicit.target,
                dry_run,
            )? {
                Some((entered, config)) => {
                    copy = Some(entered);
                    config
                }
                None => return Ok(2),
            }
        }
    };
    let build_root = copy
        .as_ref()
        .map(|c: &source::Export| c.root.clone())
        .unwrap_or_else(|| root.clone());

    if !dry_run && !verify_only {
        let commit = built_from.as_ref().map_or("", |c| c.sha.as_str());
        if let Err(reason) = version::release_gate(
            &root,
            config.versioning.as_ref(),
            commit,
            built_from.is_none(),
        ) {
            ui::phase("Aborted");
            ui::note(reason);
            ui::note("nothing was built, shipped, or changed.");
            return Ok(2);
        }
    }

    let v = version_announced(&config, &root, built_from.as_ref().map(|c| c.sha.as_str()));
    let tag_cfg = config.versioning.as_ref().and_then(|r| r.tag.as_ref());
    // `verify` ships nothing, so it needs no release of its own: it checks
    // whatever is live, on any commit, tagged or not.
    let resolved = if verify_only {
        Resolved::Release(live_release(&config, only, &root, v)?)
    } else {
        resolve_release(&root, v, tag_cfg, assume_yes, version_arg, dry_run)?
    };
    let v = match resolved {
        Resolved::Release(v) => v,
        Resolved::Refused => return Ok(2),
        // Without a prompt there is nobody to have declined: `--yes` stopping
        // here is a failed release guard, not a cancel, and a CI job must not
        // go green having shipped nothing.
        Resolved::Cancelled if assume_yes => {
            ui::phase("Aborted");
            ui::note("nothing was built, shipped, or changed.");
            return Ok(2);
        }
        Resolved::Cancelled => return Ok(0),
    };
    let mut plan = compile_announced(&config, only, &build_root, &root, &v)?;
    // `--no-soak` for a scripted run; and `verify` always, since it checks
    // what is live once rather than watching a release it just shipped.
    if no_soak {
        for sp in plan.iter_mut() {
            sp.soak = None;
        }
    }

    if verify_only {
        for sp in plan.iter_mut() {
            sp.steps.retain(|s| s.label.starts_with("verify"));
        }
        plan.retain(|sp| !sp.steps.is_empty());
        if plan.is_empty() {
            println!("no verify checks configured");
            return Ok(0);
        }
    }

    // Preflight before anything is built or shipped.
    if !preflight_announced(
        &config,
        &plan,
        &build_root,
        !dry_run,
        &Unplanned::default(),
        copy.as_ref().map(|_| root.as_path()),
    ) {
        ui::phase("Aborted");
        ui::note("preflight failed — nothing was built, shipped, or changed.");
        return Ok(2);
    }

    // What this release ships: the commits between what is live and HEAD.
    if !verify_only
        && !shipping_approved(
            &config,
            &plan,
            &root,
            &v,
            explicit.target,
            assume_yes,
            dry_run,
        )?
    {
        ui::phase("Aborted");
        ui::note("canceled — nothing was built, shipped, or changed.");
        return Ok(0);
    }

    // What this release changes in the config already running on the target.
    // Read-only, so a dry run gets it too — "what would this do to the box?" is
    // exactly the question a preview is for.
    if !verify_only && !live_config_approved(&config, &plan, assume_yes, dry_run) {
        ui::phase("Aborted");
        ui::note("canceled — nothing was built, shipped, or changed.");
        return Ok(0);
    }

    // Serialize against every other run touching these targets. Taken here,
    // after the last human window and immediately before the first mutating
    // step, so the lock is never held while someone reads a diff — and released
    // by `held`'s `Drop` on every path out of this function, including the
    // rollback unwind inside `exec::execute`.
    //
    // A dry run and `verify` mutate nothing, so neither takes the lock: gating
    // a read-only preview on a colleague's release would answer "what would
    // this do?" with "wait".
    let held = if verify_only || dry_run {
        lock::Guard::empty()
    } else {
        ui::phase("Deploy lock");
        match lock::acquire(
            &plan,
            &config.targets,
            &lock::Identity::new(&plan, &v),
            force_lock,
        ) {
            lock::Taken::Held(guard) => guard,
            lock::Taken::Busy(blocked) => {
                ui::phase("Aborted");
                for line in blocked.render() {
                    ui::fail(line);
                }
                ui::note(
                    "wait for that run to finish, or re-run with --force to take the lock anyway.",
                );
                ui::note("nothing was built, shipped, or changed.");
                return Ok(2);
            }
            lock::Taken::Failed(unlockable) => {
                ui::phase("Aborted");
                for line in unlockable.render() {
                    ui::fail(line);
                }
                ui::note("nothing was built, shipped, or changed.");
                return Ok(2);
            }
        }
    };

    ui::phase(match (verify_only, dry_run) {
        (true, _) => "Verifying",
        (_, true) => "Dry run (nothing will be executed)",
        _ => "Executing",
    });
    if !verify_only && !dry_run {
        notifications::send(&config, &root, &v, "started", None);
    }
    let (release_plan, after_tag_plan): (Vec<_>, Vec<_>) =
        plan.into_iter().partition(|service| !service.after_tag);
    let outcome = exec::execute(&release_plan, &config.targets, dry_run, Some(&held))?;

    if outcome.ok {
        // Tag what shipped, if configured. Only after success, and never on a
        // dry run — a tag should mean "this is live".
        // `verify` shipped nothing, so it has nothing to tag — and the release
        // it carries is whatever is live, which HEAD may not be.
        let mut tag_ready = dry_run || verify_only;
        if let Some(tag_cfg) = config.versioning.as_ref().and_then(|r| r.tag.as_ref()) {
            if tag_cfg.enabled && !dry_run && !verify_only {
                match version::tag_release(&root, &v, tag_cfg) {
                    Ok(tag) => {
                        let state = match (tag.created, tag.pushed) {
                            (true, true) => "created and pushed",
                            (true, false) => "created",
                            _ => "already present",
                        };
                        ui::detail(format!("tag:            {} ({state})", tag.name));
                        if let Some(note) = tag.note {
                            ui::note(note);
                        }
                        tag_ready = !tag_cfg.push || tag.pushed;
                    }
                    // The deploy is done; failing to tag must not undo that.
                    Err(e) => ui::note(format!("not tagged: {e}")),
                }
            }
        }

        if !after_tag_plan.is_empty() {
            if !tag_ready {
                ui::phase("Failed");
                ui::note("after-tag steps did not run because the release tag was not pushed.");
                ui::note("fix the tag push, then run the same release again.");
                if !dry_run {
                    notifications::send(&config, &root, &v, "failed", Some("release tag"));
                }
                return Ok(1);
            }
            ui::phase(if dry_run {
                "After tag (dry run)"
            } else {
                "After tag"
            });
            let finalized = exec::execute(&after_tag_plan, &config.targets, dry_run, Some(&held))?;
            if !finalized.ok {
                ui::phase("Failed");
                ui::note(if finalized.interrupted {
                    "the release tag exists, but the after-tag steps were interrupted (Ctrl-C)."
                } else {
                    "the release tag exists, but an after-tag step failed."
                });
                ui::note("after-tag commands should support a safe retry of the same release.");
                if !dry_run {
                    notifications::send(
                        &config,
                        &root,
                        &v,
                        "failed",
                        finalized.failed_step.as_deref(),
                    );
                }
                return Ok(1);
            }
        }

        ui::phase("Done");
        ui::ok(format!(
            "{} in {}",
            match (verify_only, dry_run) {
                (true, _) => "verify complete",
                (_, true) => "dry run complete",
                _ => "deploy complete",
            },
            timer.elapsed()
        ));
        // The two outputs, side by side: our deploy id and the app's release.
        ui::detail(format!("deploy version: {}", v.id));
        ui::detail(format!("release:        {}", v.release_display()));
        if !verify_only && !dry_run {
            notifications::send(&config, &root, &v, "succeeded", None);
        }
        Ok(0)
    } else {
        ui::phase("Failed");
        if outcome.interrupted {
            ui::note("interrupted (Ctrl-C) — the release was stopped where it stood.");
        }
        if outcome.abandoned {
            ui::note(format!(
                "rollback abandoned after {} step(s) — `deliver status` says where each service stands.",
                outcome.rolled_back
            ));
        } else if outcome.rollback_failed > 0 {
            // An undo that ran and failed is not "nothing to undo": the
            // target may still be on the release that just failed.
            if outcome.rolled_back > 0 {
                ui::note(format!(
                    "rolled back {} step(s), but rollback FAILED for {} step(s) — resolve by hand; `deliver status` says where each service stands.",
                    outcome.rolled_back, outcome.rollback_failed
                ));
            } else {
                ui::note(format!(
                    "rollback FAILED for {} step(s) — resolve by hand; `deliver status` says where each service stands.",
                    outcome.rollback_failed
                ));
            }
        } else if outcome.rolled_back > 0 {
            ui::note(format!(
                "rolled back {} step(s) — the target is back on its previous release.",
                outcome.rolled_back
            ));
        } else {
            ui::note("no reversible step had run, so nothing needed rolling back.");
        }
        ui::note(format!("elapsed {}", timer.elapsed()));
        if !verify_only && !dry_run {
            notifications::send(&config, &root, &v, "failed", outcome.failed_step.as_deref());
        }
        Ok(1)
    }
}

/// Which read-back the operator asked for.
enum Readback {
    Status,
    History { limit: usize },
}

/// `deliver status` / `deliver history` — read the target's own record of what
/// was deployed, and print it.
///
/// Both commands compile the plan for the same reason `rollback` does: the plan
/// is where the deployers declare the paths they write, so the read looks
/// exactly where the write went. Nothing on the target is modified, and no
/// release is resolved — there is no deploy here to version.
fn cmd_readback(
    explicit: ConfigArgs<'_>,
    only: &[String],
    what: Readback,
    json: bool,
) -> Result<i32> {
    cmd_readback_with_summary(explicit, only, what, json, &mut None)
}

fn cmd_readback_with_summary(
    explicit: ConfigArgs<'_>,
    only: &[String],
    what: Readback,
    json: bool,
    summary: &mut Option<String>,
) -> Result<i32> {
    ui::banner();
    let (config, path) = load_announced(explicit)?;
    let root = repo_root(&path);
    // Quietly: announcing a "deploy version" here would invite reading it as
    // the thing that is live, which is the one question this command answers.
    let v = version::resolve(
        &root,
        config
            .versioning
            .as_ref()
            .and_then(|r| r.version_from.as_deref()),
    );
    let plan = plan::build(&config, only, &root, &v)?;
    let requests = readback::collect(&plan);
    if requests.is_empty() {
        ui::phase("Nothing to read back");
        ui::note(
            "no service in this config records deploy state on the target — \
             supported deployers are files/hugo, docker-compose and managed nginx-vhost.",
        );
        return Ok(0);
    }

    ui::phase("Reading the target");
    ui::detail(format!(
        "{} service(s): {}",
        requests.len(),
        requests
            .iter()
            .map(|r| r.service.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let statuses = readback::read(requests, &config.targets);

    *summary = readback::certificate_summary(&statuses);
    if json {
        println!(
            "{}",
            secrets::redact::scrub(&serde_json::to_string_pretty(&statuses)?)
        );
    } else {
        ui::phase(match what {
            Readback::Status => "Live on the target",
            Readback::History { .. } => "Deploy history",
        });
        println!(
            "{}",
            match what {
                Readback::Status => readback::render_status(&statuses),
                Readback::History { limit } => readback::render_history(&statuses, limit),
            }
        );
    }

    // A target that could not be read is a failed answer, not an empty one —
    // scripts calling this must be able to tell the difference.
    if statuses.iter().any(|s| !s.reachable) {
        return Ok(1);
    }
    Ok(0)
}

fn cmd_logs(
    explicit: ConfigArgs<'_>,
    only: &[String],
    follow: bool,
    tail: usize,
    json: bool,
) -> Result<i32> {
    ui::banner();
    let (config, path) = load_announced(explicit)?;
    let root = repo_root(&path);
    // Quietly, like the read-back commands: this run deploys nothing, and
    // announcing a "deploy version" would invite reading it as what is live.
    let v = version::resolve(
        &root,
        config
            .versioning
            .as_ref()
            .and_then(|r| r.version_from.as_deref()),
    );
    let plan = plan::build(&config, only, &root, &v)?;
    let (requests, unknown) = logs::collect(&config, &plan);

    if requests.is_empty() {
        ui::phase("Nowhere to look");
        for service in &unknown {
            ui::detail(format!(
                "{} ({}) does not say where its logs are",
                service.service, service.deployer
            ));
        }
        ui::note(
            "only `docker-compose` services can be tailed without being told where to look. \
             Give the others a `logs:` block — `unit:` for a systemd unit, `files:` for log \
             files, or `command:` to run something else on the target.",
        );
        return Ok(2);
    }

    // Two attached streams would interleave into something neither of them
    // said. Narrowing is the operator's call, so name the choices and stop.
    if follow && requests.len() > 1 {
        ui::phase("More than one service");
        for request in &requests {
            ui::detail(logs::describe(request));
        }
        ui::note("--follow tails one service at a time — narrow the run with --service NAME.");
        return Ok(2);
    }

    // Snapshot the target's recorded live release, never the local checkout's
    // version. Old log lines can predate this snapshot.
    let statuses = if json {
        let probes = readback::collect(&plan)
            .into_iter()
            .filter(|p| {
                requests
                    .iter()
                    .any(|r| r.service == p.service && r.target == p.target && r.host == p.host)
            })
            .collect();
        readback::read(probes, &config.targets)
    } else {
        Vec::new()
    };
    let mut failed = false;
    for request in &requests {
        ui::phase(&format!(
            "{} on {}",
            request.service,
            config
                .targets
                .get(&request.target)
                .map(|t| t.describe(&request.host))
                .unwrap_or_else(|| request.host.clone())
        ));
        let ok = if json {
            let deployed = statuses
                .iter()
                .find(|s| {
                    s.service == request.service
                        && s.target == request.target
                        && s.host == request.host
                })
                .and_then(|s| s.live_deploy());
            logs::run_json(request, &config, follow, tail, deployed)
        } else {
            logs::run(request, &config, follow, tail)
        };
        if !ok {
            failed = true;
        }
    }
    for service in &unknown {
        ui::note(format!(
            "{} ({}) has no `logs:` block — skipped",
            service.service, service.deployer
        ));
    }
    Ok(if failed { 1 } else { 0 })
}

/// Take the deploy lock for a rollback, or report who has it.
///
/// `Err(code)` is the exit code to return: the two rollback commands both stop
/// with the target untouched rather than joining a release already in flight.
fn take_deploy_lock(
    plan: &[plan::ServicePlan],
    config: &config::Config,
    v: &version::DeployVersion,
    force_lock: bool,
) -> std::result::Result<lock::Guard, i32> {
    ui::phase("Deploy lock");
    match lock::acquire(
        plan,
        &config.targets,
        &lock::Identity::new(plan, v),
        force_lock,
    ) {
        lock::Taken::Held(guard) => Ok(guard),
        lock::Taken::Busy(blocked) => {
            ui::phase("Aborted");
            for line in blocked.render() {
                ui::fail(line);
            }
            ui::note(
                "wait for that run to finish, or re-run with --force to take the lock anyway.",
            );
            ui::note("nothing on the target was changed.");
            Err(2)
        }
        lock::Taken::Failed(unlockable) => {
            ui::phase("Aborted");
            for line in unlockable.render() {
                ui::fail(line);
            }
            ui::note("nothing on the target was changed.");
            Err(2)
        }
    }
}

/// How a rollback was asked to run: the flags `deploy` has, honoured the same
/// way.
#[derive(Clone, Copy)]
struct RollbackRun {
    force_lock: bool,
    assume_yes: bool,
    dry_run: bool,
}

/// The last window before a rollback acts. `--yes` and a dry run never block,
/// and no stdin is not a refusal — it is a script, which chose this already.
/// Returns false only when a human said no.
fn rollback_confirmed(how: RollbackRun, services: usize) -> Result<bool> {
    if how.assume_yes || how.dry_run {
        return Ok(true);
    }
    Ok(prompt_yes_no(&format!("Roll back {services} service(s)?"))?.unwrap_or(true))
}

/// After the swap: record each restored release in the target's history,
/// run the restored services' verify and health checks, and send the notice.
///
/// `restored` is (service, the one-line description, where it now stands).
/// Returns the exit code.
fn finish_rollback(
    config: &config::Config,
    root: &Path,
    v: &version::DeployVersion,
    plan: &[plan::ServicePlan],
    restored: &[(String, String, Option<rollback::StepBack>)],
    timer: &ui::Timer,
) -> Result<i32> {
    let records: Vec<&rollback::StepBack> = restored
        .iter()
        .filter_map(|(_, _, back)| back.as_ref())
        .filter(|back| back.to.is_some())
        .collect();
    if !records.is_empty() {
        ui::phase("Recording");
        for back in records {
            let target = &config.targets[&back.target];
            let sudo = if target.uses_sudo() { "sudo " } else { "" };
            let id = back.to.as_deref().unwrap_or_default();
            let command = rollback::record_command(
                &back.history_path,
                id,
                back.to_release.as_deref().unwrap_or("unknown"),
                back.to_sha.as_deref().unwrap_or("unknown"),
                sudo,
            );
            // The release is already back; a history row that did not land
            // is worth a warning, not a failed rollback.
            if !matches!(exec::run_ssh(target, &back.host, &command), Ok(true)) {
                ui::note(format!(
                    "{}: could not append the rollback to {}",
                    back.service, back.history_path
                ));
            }
        }
    }

    let services: Vec<&str> = restored.iter().map(|(s, _, _)| s.as_str()).collect();
    let checks: Vec<plan::ServicePlan> = plan
        .iter()
        .filter(|sp| services.contains(&sp.service.as_str()))
        .map(|sp| plan::ServicePlan {
            service: sp.service.clone(),
            target: sp.target.clone(),
            host: sp.host.clone(),
            steps: sp
                .steps
                .iter()
                .filter(|s| s.label.starts_with("verify") || s.label.starts_with("health check"))
                .cloned()
                .collect(),
            after_tag: false,
            soak: None,
        })
        .filter(|sp| !sp.steps.is_empty())
        .collect();
    ui::phase("Verifying the restored release");
    let failed = if checks.is_empty() {
        ui::detail("no verify or health checks configured — nothing proves the restored release");
        None
    } else {
        exec::check(&checks, &config.targets)?
    };

    let summary = restored
        .iter()
        .map(|(_, line, _)| line.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    match failed {
        None => {
            notifications::send(config, root, v, "rolled_back", Some(&summary));
            ui::phase("Done");
            ui::ok(format!(
                "rolled back {} service(s) in {}",
                restored.len(),
                timer.elapsed()
            ));
            Ok(0)
        }
        Some(step) => {
            notifications::send(
                config,
                root,
                v,
                "failed",
                Some(&format!("rollback check {step}")),
            );
            ui::phase("Failed");
            ui::fail(format!("the restored release did not pass: {step}"));
            ui::note("the rollback itself landed — `deliver status` says what is live now.");
            Ok(1)
        }
    }
}

fn cmd_rollback(explicit: ConfigArgs<'_>, only: &[String], how: RollbackRun) -> Result<i32> {
    ui::banner();
    let timer = ui::Timer::start();
    let (config, path) = load_announced(explicit)?;
    let root = repo_root(&path);
    // Quietly, as `--to` does: a rollback restores what is already on the
    // target, so announcing a "deploy version" from this checkout would name
    // something nothing is going to deploy.
    let v = version::resolve(
        &root,
        config
            .versioning
            .as_ref()
            .and_then(|r| r.version_from.as_deref()),
    );
    let plan = compile_announced(&config, only, &root, &root, &v)?;

    let reversible: Vec<&plan::ServicePlan> = plan
        .iter()
        .filter(|sp| sp.steps.iter().any(|s| s.rollback.is_some()))
        .collect();
    if reversible.is_empty() {
        // Lets `exec::rollback` say so in its own words.
        ui::phase("Failed");
        exec::rollback(&plan, &config.targets)?;
        return Ok(1);
    }

    // Read the target first, so both ends of every swap are on screen before
    // anything is touched. Read-only, so a dry run reads it too: the preview
    // is the whole point of one.
    let names: Vec<&str> = reversible.iter().map(|sp| sp.service.as_str()).collect();
    let requests: Vec<readback::Request> = readback::collect_releases(&plan)
        .into_iter()
        .filter(|r| names.contains(&r.service.as_str()))
        .collect();
    ui::phase("Reading the target");
    let statuses = if requests.is_empty() {
        Vec::new()
    } else {
        readback::read(requests, &config.targets)
    };
    let backs: Vec<rollback::StepBack> = statuses.iter().map(rollback::step_back).collect();
    let mut restored: Vec<(String, String, Option<rollback::StepBack>)> = Vec::new();
    for sp in &reversible {
        match backs.iter().find(|b| b.service == sp.service) {
            Some(back) => {
                let line = rollback::describe_step_back(back);
                ui::detail(&line);
                restored.push((sp.service.clone(), line, Some(back.clone())));
            }
            // No record on the target (a deployer with an undo but no
            // history): name the undo itself.
            None => {
                for step in sp.steps.iter().filter(|s| s.rollback.is_some()) {
                    let line =
                        secrets::redact::scrub(&format!("{}: undo \"{}\"", sp.service, step.label));
                    ui::detail(&line);
                    restored.push((sp.service.clone(), line, None));
                }
            }
        }
    }
    restored.dedup_by(|a, b| a.0 == b.0);

    // One service with nowhere to go refuses the whole run, dry or not, while
    // the target is untouched — as `--to` does, so a rollback never half-lands.
    let refusals: Vec<(String, String)> = statuses
        .iter()
        .filter_map(|status| {
            rollback::nothing_to_roll_back(status).map(|why| (status.service.clone(), why))
        })
        .collect();
    if !refusals.is_empty() {
        ui::phase("Failed");
        for (service, why) in &refusals {
            ui::fail(format!("{service}: {why}"));
        }
        ui::note("nothing was changed on the target.");
        return Ok(1);
    }

    if how.dry_run {
        ui::phase("Dry run (nothing will be executed)");
        ui::detail(format!(
            "would roll back {} service(s), record each in the target's history, and run their checks",
            reversible.len()
        ));
        ui::note("nothing was changed on the target.");
        return Ok(0);
    }
    if !rollback_confirmed(how, reversible.len())? {
        ui::phase("Aborted");
        ui::note("canceled — nothing was changed on the target.");
        return Ok(0);
    }

    // A rollback swaps the same symlink a deploy does, so it contends for the
    // same lock — the `rollback` fired at a target mid-deploy is one of the
    // races this exists to stop.
    let _lock = match take_deploy_lock(&plan, &config, &v, how.force_lock) {
        Ok(guard) => guard,
        Err(code) => return Ok(code),
    };
    ui::phase("Rolling back");
    if !exec::rollback(&plan, &config.targets)? {
        notifications::send(&config, &root, &v, "failed", Some("rollback"));
        ui::phase("Failed");
        ui::note("some undo did not complete — `deliver status` says where each service stands.");
        return Ok(1);
    }
    finish_rollback(&config, &root, &v, &plan, &restored, &timer)
}

/// `deliver rollback --to <deploy-id>` — restore any *retained* release, not
/// just the one step back `.deliver-previous` records.
///
/// Reads the target first (the same read `deliver status` does), resolves
/// every selected service against the requested id, and only then swaps. A
/// service that cannot be satisfied refuses the whole run while the target is
/// still untouched, so a multi-service rollback never half-lands.
fn cmd_rollback_to(
    explicit: ConfigArgs<'_>,
    only: &[String],
    deploy_id: &str,
    how: RollbackRun,
) -> Result<i32> {
    ui::banner();
    let timer = ui::Timer::start();
    if !rollback::id_is_addressable(deploy_id) {
        ui::phase("Failed");
        ui::fail(format!("{deploy_id} is not a deploy id"));
        ui::note("a deploy id is one directory name — `deliver history` lists them.");
        return Ok(2);
    }
    let (config, path) = load_announced(explicit)?;
    let root = repo_root(&path);
    // Quietly, as the read-back does: the release being restored is already on
    // the target, so announcing a "deploy version" resolved from this checkout
    // would name something this command has no intention of shipping.
    let v = version::resolve(
        &root,
        config
            .versioning
            .as_ref()
            .and_then(|r| r.version_from.as_deref()),
    );
    let plan = plan::build(&config, only, &root, &v)?;
    let requests = readback::collect_releases(&plan);
    if requests.is_empty() {
        ui::phase("Failed");
        ui::fail("no service in this config keeps releases on the target");
        ui::note("`--to` addresses a retained release directory, which the files and hugo deployers write.");
        return Ok(2);
    }

    ui::phase("Reading the target");
    ui::detail(format!(
        "{} service(s): {}",
        requests.len(),
        requests
            .iter()
            .map(|r| r.service.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let statuses = readback::read(requests, &config.targets);
    let resolved = rollback::resolve(&statuses, &config.targets, deploy_id);

    // Every refusal is reported, not just the first: an operator fixing a
    // multi-service run wants the whole list in one pass.
    let refusals: Vec<&rollback::Refusal> = resolved
        .iter()
        .filter_map(|r| match r {
            rollback::Resolution::Refused(refusal) => Some(refusal),
            _ => None,
        })
        .collect();
    if !refusals.is_empty() {
        ui::phase("Failed");
        for refusal in &refusals {
            ui::fail(format!("{}: {}", refusal.service, refusal.reason));
        }
        ui::note("nothing was changed on the target.");
        return Ok(refusals.iter().map(|r| r.exit_code).max().unwrap_or(2));
    }

    let swaps: Vec<&rollback::Swap> = resolved
        .iter()
        .filter_map(|r| match r {
            rollback::Resolution::Ready(swap) => Some(swap),
            _ => None,
        })
        .collect();
    if swaps.is_empty() {
        ui::phase("Done");
        ui::ok(format!("{deploy_id} is already live everywhere selected"));
        return Ok(0);
    }

    ui::phase(&format!("Rolling back to {deploy_id}"));
    for resolution in &resolved {
        if let rollback::Resolution::AlreadyLive { service, .. } = resolution {
            ui::detail(format!("{service}: already live, left alone"));
        }
    }
    for swap in &swaps {
        ui::detail(rollback::describe(swap));
    }
    if how.dry_run {
        ui::phase("Dry run (nothing will be executed)");
        ui::note("nothing was changed on the target.");
        return Ok(0);
    }
    if !rollback_confirmed(how, swaps.len())? {
        ui::phase("Aborted");
        ui::note("canceled — nothing was changed on the target.");
        return Ok(0);
    }

    let _lock = match take_deploy_lock(&plan, &config, &v, how.force_lock) {
        Ok(guard) => guard,
        Err(code) => return Ok(code),
    };

    let mut all_ok = true;
    for swap in &swaps {
        let target = &config.targets[&swap.target];
        match exec::run_ssh(target, &swap.host, &swap.command) {
            Ok(true) => {}
            _ => {
                all_ok = false;
                ui::fail(format!("{}: rollback failed", swap.service));
            }
        }
    }
    if all_ok {
        let restored: Vec<(String, String, Option<rollback::StepBack>)> = swaps
            .iter()
            .filter_map(|swap| {
                let status = statuses.iter().find(|s| s.service == swap.service)?;
                Some((
                    swap.service.clone(),
                    rollback::describe(swap),
                    Some(rollback::landed(swap, status)),
                ))
            })
            .collect();
        finish_rollback(&config, &root, &v, &plan, &restored, &timer)
    } else {
        notifications::send(&config, &root, &v, "failed", Some("rollback"));
        ui::phase("Failed");
        ui::note("some services did not swap — `deliver status` says where each one stands.");
        Ok(1)
    }
}

/// `deliver fleet …` — the same per-repo command, once per repo.
///
/// The whole command is the loop: each repo is entered and the existing
/// `cmd_preflight` / `cmd_deploy` / `cmd_readback` runs there with no config
/// path, so it discovers the repo's own `.deliver.yml` exactly as it would if
/// you had typed the command in that directory. Nothing about a repo's release
/// is decided here; this only decides *which* repos run and what happens when
/// one of them fails.
fn cmd_fleet(
    config: Option<&Path>,
    target: Option<&str>,
    fleet_path: Option<&Path>,
    only_repos: &[String],
    action: &FleetAction,
) -> Result<i32> {
    ui::banner();
    // One config cannot describe several repos, and silently ignoring it would
    // hide that from whoever typed it.
    if config.is_some() {
        ui::fail("--config names one repo's config, so it cannot be used with `fleet`");
        ui::note("pass --fleet PATH to choose the fleet file, or --repo NAME to narrow the run.");
        return Ok(2);
    }
    // Target names are per repo, so one name cannot mean the same thing
    // across the fleet.
    if target.is_some() {
        ui::fail("--target names one config's target, so it cannot be used with `fleet`");
        ui::note("run `deliver --target NAME deploy` in the repo instead.");
        return Ok(2);
    }

    ui::phase("Loading fleet");
    let path = fleet::resolve(fleet_path)?;
    ui::detail(format!("fleet: {}", path.display()));
    let fleet = fleet::load(&path)?;
    let (selected, unmatched) = fleet.select(only_repos);
    // A selector that matched nothing is a typo, and running the fleet minus a
    // repo the operator thinks is in it is the worst possible answer.
    if !unmatched.is_empty() {
        ui::fail(format!("no repo named {}", unmatched.join(", ")));
        ui::detail(format!(
            "{} lists: {}",
            fleet.path.display(),
            fleet
                .repos
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return Ok(2);
    }
    ui::detail(format!(
        "{} repo(s): {}",
        selected.len(),
        selected
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    // A deploy stops at the first failure: the repo that failed has already
    // unwound itself, and carrying on would put more of the box in flight while
    // an operator is still working out what happened. Reads never stop — the
    // point of asking the fleet is the whole answer.
    let stop_on_failure = match action {
        FleetAction::Deploy { keep_going, .. } => !keep_going,
        _ => false,
    };

    // Each repo finds its own config and uses its own default target.
    let each = ConfigArgs {
        path: None,
        target: None,
    };
    let mut outcomes: Vec<(&fleet::Repo, fleet::RepoOutcome)> = Vec::new();
    let mut certificate_summaries = std::collections::BTreeMap::new();
    let mut stopped = false;
    for repo in &selected {
        if stopped {
            outcomes.push((repo, fleet::RepoOutcome::NotAttempted));
            continue;
        }
        ui::phase(&format!("{} — {}", repo.name, repo.path.display()));
        let mut certificate_summary = None;
        let outcome = fleet::run_in(repo, || match action {
            FleetAction::Preflight { service } => cmd_preflight(each, service),
            FleetAction::Deploy {
                service,
                dry_run,
                yes,
                force,
                ..
            } => cmd_deploy(each, service, *dry_run, false, *yes, None, *force, false),
            FleetAction::Status { service } => cmd_readback_with_summary(
                each,
                service,
                Readback::Status,
                false,
                &mut certificate_summary,
            ),
        });
        if let Some(summary) = certificate_summary {
            certificate_summaries.insert(repo.name.clone(), summary);
        }
        // Ctrl-C ends the fleet run even with --keep-going: the operator
        // stopped one repo and did not ask for the next to start.
        if !outcome.ok() && (stop_on_failure || interrupt::seen()) {
            stopped = true;
        }
        outcomes.push((repo, outcome));
    }

    ui::phase("Fleet summary");
    let width = outcomes
        .iter()
        .map(|(repo, _)| repo.name.len())
        .max()
        .unwrap_or(0);
    for (repo, outcome) in &outcomes {
        let suffix = certificate_summaries
            .get(&repo.name)
            .map(|s| format!(" · {s}"))
            .unwrap_or_default();
        let line = format!("{:width$}  {}{suffix}", repo.name, outcome.describe());
        match outcome {
            fleet::RepoOutcome::Ran(0) => ui::ok(line),
            fleet::RepoOutcome::NotAttempted => ui::detail(format!("- {line}")),
            _ => ui::fail(line),
        }
    }
    if stopped {
        ui::note("stopped at the first failure — pass --keep-going to run the rest anyway.");
    }

    // The worst per-repo code, so a script sees the same thing it would have
    // seen running the repos one at a time and taking the worst answer.
    Ok(outcomes
        .iter()
        .map(|(_, outcome)| outcome.exit_code())
        .max()
        .unwrap_or(0))
}

fn cmd_preflight(explicit: ConfigArgs<'_>, only: &[String]) -> Result<i32> {
    ui::banner();
    let (config, path) = load_announced(explicit)?;
    let secrets_ok = secrets_announced(&config, &repo_root(&path));
    let v = version_announced(&config, &repo_root(&path), None);
    // A plan that will not compile is a *finding* here, not a fatal error:
    // preflight is the command whose whole job is to list everything wrong at
    // once, and an operator who is also missing `hugo` or cannot reach the box
    // should learn that now rather than on the next run.
    let (compiled, unplanned) = compile_reported(&config, only, &repo_root(&path), &v);
    let ok = preflight_announced(
        &config,
        &compiled,
        &repo_root(&path),
        true,
        &unplanned,
        None,
    ) && secrets_ok;
    ui::phase(if ok { "Done" } else { "Failed" });
    if ok {
        ui::ok("ready to deploy");
        Ok(0)
    } else {
        ui::note("fix the above before deploying.");
        Ok(2)
    }
}
