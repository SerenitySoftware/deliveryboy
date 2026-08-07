//! `deliver` — the Delivery Boy CLI.
//!
//! Exit codes: 0 ok · 1 step/verify failure · 2 config or usage error.

mod config;
mod deployers;
mod detect;
mod exec;
mod notifications;
mod plan;
mod preflight;
mod secrets;
mod ui;
mod verify;
mod version;

use anyhow::Result;
use clap::{Parser, Subcommand};
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

    #[command(subcommand)]
    command: Commands,
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

#[derive(Subcommand)]
enum Commands {
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
    },
    /// Compile and print the plan — executes nothing
    Plan {
        #[arg(long)]
        service: Vec<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Execute the plan
    Deploy {
        #[arg(long)]
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
    },
    /// Run only the verify checks
    Verify {
        #[arg(long)]
        service: Vec<String>,
    },
    /// Roll back to the previous release (repoints the live symlink)
    Rollback {
        #[arg(long)]
        service: Vec<String>,
    },
    /// Run the preflight checks only (tools, input files, ssh reachability)
    Preflight {
        #[arg(long)]
        service: Vec<String>,
    },
    /// Show which secrets this config needs and whether they're in place
    Secrets {
        #[command(subcommand)]
        action: Option<SecretsAction>,
    },
    /// Remove this app's build artifacts from the system temp dir
    Clean,
    /// Schema-check .deliver.yml
    Validate,
}

fn main() {
    let cli = Cli::parse();
    let code = match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            2
        }
    };
    std::process::exit(code);
}

fn run(cli: &Cli) -> Result<i32> {
    match &cli.command {
        Commands::Init {
            path,
            host,
            dir,
            write,
            force,
        } => cmd_init(path, host.as_deref(), dir.as_deref(), *write, *force),
        Commands::Plan { service, json } => cmd_plan(cli.config.as_deref(), service, *json),
        Commands::Deploy {
            service,
            dry_run,
            yes,
            version,
        } => cmd_deploy(
            cli.config.as_deref(),
            service,
            *dry_run,
            false,
            *yes,
            version.as_deref(),
        ),
        Commands::Verify { service } => {
            cmd_deploy(cli.config.as_deref(), service, false, true, true, None)
        }
        Commands::Rollback { service } => cmd_rollback(cli.config.as_deref(), service),
        Commands::Preflight { service } => cmd_preflight(cli.config.as_deref(), service),
        Commands::Secrets { action } => cmd_secrets(cli.config.as_deref(), action),
        Commands::Clean => cmd_clean(cli.config.as_deref()),
        Commands::Validate => cmd_validate(cli.config.as_deref()),
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

    ui::phase("Detected deploy strategies");
    for f in &findings {
        let status = match f.deployer {
            Some(d) => format!("→ deployer: {d}"),
            None => "→ no deployer yet (informational)".to_string(),
        };
        println!("  • {}  {status}", f.evidence);
    }

    let host = host.unwrap_or("CHANGEME.example.com");
    let dir = dir
        .map(str::to_string)
        .unwrap_or_else(|| format!("/var/universal/{app}"));
    let yaml = detect::scaffold(&app, host, &dir, &findings);
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

    std::fs::write(&dest, &yaml)?;
    println!("\nWrote {}", dest.display());
    println!("Review it, then run: deliver plan");
    Ok(0)
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
/// version (no prefix) to tag, or None to cancel.
fn choose_version(previous: Option<&str>) -> Result<Option<String>> {
    use version::{Bump, SemVer};

    let parsed = previous.and_then(SemVer::parse);
    match (&parsed, previous) {
        (Some(_), Some(tag)) => ui::detail(format!("HEAD has no tag. Previous release: {tag}")),
        (None, Some(tag)) => ui::detail(format!(
            "HEAD has no tag. Previous tag {tag} isn't a version number."
        )),
        _ => ui::detail("HEAD has no tag, and there are no previous releases."),
    }

    // Offer patch/minor/major off the previous release; otherwise a sane first cut.
    let options: Vec<(&str, String)> = match &parsed {
        Some(v) => vec![
            ("patch", v.bump(Bump::Patch)),
            ("minor", v.bump(Bump::Minor)),
            ("major", v.bump(Bump::Major)),
        ],
        None => vec![("first release", "0.1.0".to_string())],
    };

    println!();
    for (i, (kind, next)) in options.iter().enumerate() {
        let prefix = parsed
            .as_ref()
            .map(|p| p.prefix.clone())
            .unwrap_or_else(|| "v".into());
        println!("  {}) {kind:<13} {prefix}{next}", i + 1);
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
    Ok(Some(options[choice - 1].1.clone()))
}

/// Resolve which release is being deployed.
///
/// A deploy ships a tagged commit: find the tag on HEAD and confirm it, or offer
/// to create one. Non-interactive runs never block — an existing tag is accepted,
/// and a missing one is an error rather than a silent untagged deploy.
fn resolve_release(
    root: &Path,
    v: version::DeployVersion,
    tag_cfg: Option<&config::TagConfig>,
    assume_yes: bool,
    version_arg: Option<&str>,
    dry_run: bool,
) -> Result<Option<version::DeployVersion>> {
    // A dry run is a preview: never prompt, never create a tag, never block.
    // With --version it shows the complete release plan; without one it shows
    // the untagged guards that a real deploy would hit.
    if dry_run {
        if let Some(wanted) = version_arg {
            let prefix = version::previous_tag(root)
                .as_deref()
                .and_then(version::SemVer::parse)
                .map(|parsed| parsed.prefix)
                .unwrap_or_else(|| "v".to_string());
            return Ok(Some(v.with_release(
                format!("{prefix}{}", wanted.trim_start_matches('v')),
                "argument",
            )));
        }
        return Ok(Some(v));
    }
    // `from: commit` / `commit-count` are explicit choices — nothing to resolve.
    if v.release_source != "tag" {
        return Ok(Some(v));
    }

    if let Some(tag) = v.release.clone() {
        if assume_yes {
            return Ok(Some(v));
        }
        return match prompt_yes_no(&format!("Deploy release {tag} ({})?", v.git.short_sha))? {
            Some(true) => Ok(Some(v)),
            Some(false) => {
                ui::note("canceled.");
                Ok(None)
            }
            // No stdin: the tag exists and was chosen deliberately by tagging it.
            None => Ok(Some(v)),
        };
    }

    // No tag on HEAD — offer the next versions based on the previous release.
    let previous = version::previous_tag(root);
    let wanted = match version_arg {
        Some(v) => Some(v.to_string()),
        None if assume_yes => None,
        None => choose_version(previous.as_deref())?,
    };
    let Some(wanted) = wanted else {
        ui::note(
            "no release to deploy — tag the commit (`git tag -a v0.4.1 -m v0.4.1`), \
             pass --version, or set versioning.from to commit/commit-count.",
        );
        return Ok(None);
    };

    if v.git.dirty {
        ui::note("working tree is dirty — commit before tagging a release.");
        return Ok(None);
    }

    // Carry the requested release through the deploy without mutating git.
    // If configured, the tag is created and pushed only after every deploy and
    // verify step succeeds, so failed releases do not leave false tags behind.
    let prefix = previous
        .as_deref()
        .and_then(version::SemVer::parse)
        .map(|parsed| parsed.prefix)
        .unwrap_or_else(|| "v".to_string());
    let _ = tag_cfg;
    Ok(Some(v.with_release(
        format!("{prefix}{}", wanted.trim_start_matches('v')),
        "argument",
    )))
}

fn cmd_secrets(explicit: Option<&Path>, action: &Option<SecretsAction>) -> Result<i32> {
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
        if reqs.is_empty() && declared.is_empty() {
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
        ui::phase(if missing == 0 {
            "Done"
        } else {
            "Missing secrets"
        });
        if missing == 0 {
            ui::ok("everything this config needs is in place");
            Ok(0)
        } else {
            ui::note(format!(
                "{missing} secret(s) missing — a deploy would fail partway."
            ));
            Ok(2)
        }
    }
}

/// Remove the local work directory. Deploys clean up after themselves on
/// success; this is for when one failed and left artifacts behind.
fn cmd_clean(explicit: Option<&Path>) -> Result<i32> {
    ui::banner();
    let path = config::resolve(explicit)?;
    let config = config::load(&path)?;
    let scratch = version::scratch_root(&config.app);
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

fn cmd_validate(explicit: Option<&Path>) -> Result<i32> {
    ui::banner();
    let (config, path) = load_announced(explicit)?;
    let v = version_announced(&config, &repo_root(&path));
    ui::phase("Validating");
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

/// Phase 0: find and load the config, reporting what was picked up.
fn load_announced(explicit: Option<&Path>) -> Result<(config::Config, PathBuf)> {
    ui::phase("Loading configuration");
    let path = config::resolve(explicit)?;
    ui::detail(format!("config: {}", path.display()));
    let config = config::load(&path)?;
    let target_names: Vec<&str> = config.targets.keys().map(|s| s.as_str()).collect();
    ui::detail(format!(
        "app: {} · {} service(s) · target(s): {}",
        config.app,
        config.services.len(),
        target_names.join(", ")
    ));
    Ok((config, path))
}

/// Resolve this deploy's identity: Delivery Boy's own deploy id, plus the app's
/// release number. Reported up front so you know what you're about to ship.
fn version_announced(config: &config::Config, root: &Path) -> version::DeployVersion {
    let release_cfg = config.versioning.as_ref();
    let v = version::resolve(root, release_cfg.and_then(|r| r.version_from.as_deref()));
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
    if reqs.is_empty() && declared.is_empty() {
        return true;
    }
    ui::phase("Secrets");
    let mut ok = true;
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

/// Phase: compile the plan, reporting its size.
fn compile_announced(
    config: &config::Config,
    only: &[String],
    root: &Path,
    version: &version::DeployVersion,
) -> Result<Vec<plan::ServicePlan>> {
    ui::phase("Compiling plan");
    let compiled = plan::build(config, only, root, version)?;
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
    Ok(compiled)
}

/// Phase: preflight. Returns false when it found problems.
fn preflight_announced(
    config: &config::Config,
    compiled: &[plan::ServicePlan],
    root: &Path,
    check_remote: bool,
) -> bool {
    ui::phase("Preflight");
    if !check_remote {
        ui::detail("(dry run — skipping remote reachability)");
    }
    let report = preflight::run(config, compiled, root, check_remote);
    for line in &report.checked {
        ui::ok(line);
    }
    for problem in &report.problems {
        ui::fail(problem);
    }

    report.ok()
}

fn cmd_plan(explicit: Option<&Path>, only: &[String], json: bool) -> Result<i32> {
    ui::banner();
    let (config, path) = load_announced(explicit)?;
    let v = version_announced(&config, &repo_root(&path));
    let compiled = compile_announced(&config, only, &repo_root(&path), &v)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&compiled)?);
    } else {
        ui::phase("Plan (nothing is executed)");
        println!("{}", plan::render(&compiled));
    }
    Ok(0)
}

fn cmd_deploy(
    explicit: Option<&Path>,
    only: &[String],
    dry_run: bool,
    verify_only: bool,
    assume_yes: bool,
    version_arg: Option<&str>,
) -> Result<i32> {
    ui::banner();
    let timer = ui::Timer::start();
    let (config, path) = load_announced(explicit)?;
    let root = repo_root(&path);

    // Before anything is tagged, built, or shipped.
    if !secrets_announced(&config, &root) && !dry_run {
        ui::phase("Aborted");
        ui::note("missing secrets — nothing was tagged, built, or changed.");
        return Ok(2);
    }

    if !dry_run && !verify_only {
        if let Err(reason) = version::release_gate(&root, config.versioning.as_ref()) {
            ui::phase("Aborted");
            ui::note(reason);
            return Ok(2);
        }
    }

    let v = version_announced(&config, &root);
    let tag_cfg = config.versioning.as_ref().and_then(|r| r.tag.as_ref());
    let Some(v) = resolve_release(
        &root,
        v,
        tag_cfg,
        assume_yes || verify_only,
        version_arg,
        dry_run,
    )?
    else {
        return Ok(0);
    };
    let mut plan = compile_announced(&config, only, &root, &v)?;

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
    if !preflight_announced(&config, &plan, &root, !dry_run) {
        ui::phase("Aborted");
        ui::note("preflight failed — nothing was built, shipped, or changed.");
        return Ok(2);
    }

    ui::phase(match (verify_only, dry_run) {
        (true, _) => "Verifying",
        (_, true) => "Dry run (nothing will be executed)",
        _ => "Executing",
    });
    if !verify_only && !dry_run {
        notifications::send(&config, &root, &v, "started", None);
    }
    let outcome = exec::execute(&plan, &config.targets, dry_run)?;

    if outcome.ok {
        ui::phase("Done");
        ui::ok(format!(
            "{} in {}",
            if dry_run {
                "dry run complete"
            } else {
                "deploy complete"
            },
            timer.elapsed()
        ));
        // The two outputs, side by side: our deploy id and the app's release.
        ui::detail(format!("deploy version: {}", v.id));
        ui::detail(format!("release:        {}", v.release_display()));

        // Tag what shipped, if configured. Only after success, and never on a
        // dry run — a tag should mean "this is live".
        if let Some(tag_cfg) = config.versioning.as_ref().and_then(|r| r.tag.as_ref()) {
            if tag_cfg.enabled && !dry_run {
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
                    }
                    // The deploy is done; failing to tag must not undo that.
                    Err(e) => ui::note(format!("not tagged: {e}")),
                }
            }
        }
        if !verify_only && !dry_run {
            notifications::send(&config, &root, &v, "succeeded", None);
        }
        Ok(0)
    } else {
        ui::phase("Failed");
        if outcome.rolled_back > 0 {
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

fn cmd_rollback(explicit: Option<&Path>, only: &[String]) -> Result<i32> {
    ui::banner();
    let timer = ui::Timer::start();
    let (config, path) = load_announced(explicit)?;
    let v = version_announced(&config, &repo_root(&path));
    let plan = compile_announced(&config, only, &repo_root(&path), &v)?;
    ui::phase("Rolling back");
    let ok = exec::rollback(&plan, &config.targets)?;
    if ok {
        ui::phase("Done");
        ui::ok(format!("rolled back in {}", timer.elapsed()));
        Ok(0)
    } else {
        ui::phase("Failed");
        Ok(1)
    }
}

fn cmd_preflight(explicit: Option<&Path>, only: &[String]) -> Result<i32> {
    ui::banner();
    let (config, path) = load_announced(explicit)?;
    let secrets_ok = secrets_announced(&config, &repo_root(&path));
    let v = version_announced(&config, &repo_root(&path));
    let compiled = compile_announced(&config, only, &repo_root(&path), &v)?;
    let ok = preflight_announced(&config, &compiled, &repo_root(&path), true) && secrets_ok;
    ui::phase(if ok { "Done" } else { "Failed" });
    if ok {
        ui::ok("ready to deploy");
        Ok(0)
    } else {
        ui::note("fix the above before deploying.");
        Ok(2)
    }
}
