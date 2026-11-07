//! gleon CLI wrapper binary.

use std::path::Path;

use cli::{Cli, Commands};
use gleon_core::env::EnvProvider;
use tracing::info;

mod cli;
mod exit_code;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse_checked();

    // Determine the log level based on CLI flags
    let log_level = if cli.quiet {
        tracing::Level::WARN
    } else if cli.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    // Initialize tracing subscriber for logging, directing log output to stderr
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(log_level)
        .init();

    info!("gleon CLI starting up...");

    let current_dir = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to determine current directory: {e}"))?;

    // Load environment configuration from .gleon/.env and .gleon/.env.local of the workspace
    // (that of `--config` when given; never of the home directory).
    let home = gleon_core::env::home(&gleon_core::env::OsEnv);
    let dotenv_dir =
        gleon_core::env::dotenv_dir(&current_dir, cli.config.as_deref(), home.as_deref());
    let dotenv = gleon_core::env::load_dotenv(&dotenv_dir, home.as_deref());
    if !dotenv.is_empty() {
        tracing::debug!(
            "Loaded {} environment variable(s) from .env files",
            dotenv.len()
        );
    }
    let env = gleon_core::env::MergedEnv::new(dotenv);

    // Run License/Compliance Check
    let license_status = gleon_core::license::LicenseGate::verify(&env);
    let decision = gleon_core::license::enforce_policy(license_status, cli.strict, &env);
    for line in &decision.message {
        eprintln!("{line}");
    }
    if let Some(annotation) = &decision.gha_annotation {
        eprintln!("{annotation}");
    }
    if decision.action == gleon_core::license::EnforcementAction::Block {
        std::process::exit(42);
    }

    // Any error still bubbling here means a command couldn't even start (e.g. context
    // resolution failed) — every command that *did* start reports its own failures via
    // `commands::report_failure`, so this is the one remaining spot that needs to log+exit
    // consistently with those.
    let exit_code = match run(&cli, &current_dir, &env).await {
        Ok(code) => code,
        Err(e) => i32::from(commands::report_failure("Context resolution failed", &*e)),
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn get_storage_config(env: &dyn EnvProvider) -> Option<gleon_core::storage::StorageConfig> {
    gleon_core::storage::StorageConfig::from_env(env)
}

mod commands;

/// Resolves the active [`gleon_core::context::ResolvedContext`] for the current invocation —
/// every subcommand needing workspace/platform/branch context builds it the same way, so this
/// is the one place that does.
fn resolve_context(
    cli: &Cli,
    current_dir: &Path,
    env: &dyn EnvProvider,
) -> anyhow::Result<gleon_core::context::ResolvedContext> {
    gleon_core::context::ResolvedContext::resolve(
        &gleon_core::context::ContextOptions::from(cli),
        current_dir,
        env,
    )
    .map_err(|e| anyhow::anyhow!(e))
}

/// The platform the global `--platform` names, as the context resolved it: its canonical key
/// (lowercase, `macos-arm64` as `macos-aarch64`), the directory `gleon stage` writes, for the
/// commands that read the manifests of one platform (`lint`, `resolve`, `pull`, `push`); `None`
/// without the flag.
fn platform_filter(
    cli: &Cli,
    ctx: &gleon_core::context::ResolvedContext,
) -> anyhow::Result<Option<String>> {
    if cli.platform.is_none() {
        return Ok(None);
    }
    Ok(Some(ctx.platform.key()?.as_str().to_owned()))
}

#[expect(
    clippy::too_many_lines,
    reason = "top-level subcommand dispatch: one match arm per command"
)]
async fn run(cli: &Cli, current_dir: &Path, env: &dyn EnvProvider) -> anyhow::Result<i32> {
    let code = match &cli.command {
        Commands::Init(args) => commands::init::run_init(
            &resolve_context(cli, current_dir, env)?,
            current_dir,
            args.template(),
        ),
        Commands::Status { json } => {
            commands::status::run_status(&resolve_context(cli, current_dir, env)?, *json)
        }
        Commands::Stage { paths } => {
            commands::stage::run_stage(&resolve_context(cli, current_dir, env)?, paths)
        }
        Commands::Diff {
            auto_pull,
            artifacts,
        } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            let storage = get_storage_config(env);
            commands::diff::run_diff(&ctx, env, *auto_pull, storage, artifacts.clone()).await
        }
        Commands::LintManifests => {
            let ctx = resolve_context(cli, current_dir, env)?;
            commands::lint::run_lint(&ctx, platform_filter(cli, &ctx)?.as_deref())
        }
        Commands::Resolve { test_path, fetch } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            commands::resolve::run_resolve(
                &ctx,
                platform_filter(cli, &ctx)?.as_deref(),
                test_path.as_deref(),
                *fetch,
                get_storage_config(env),
            )
            .await
        }
        Commands::Pull {
            all_platforms,
            require_storage,
        } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            let storage = get_storage_config(env);
            commands::pull::run_pull(
                &ctx,
                storage.as_ref(),
                *all_platforms,
                platform_filter(cli, &ctx)?.as_deref(),
                *require_storage,
            )
            .await
        }
        Commands::Push {
            all_platforms,
            require_storage,
        } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            let storage = get_storage_config(env);
            commands::push::run_push(
                &ctx,
                storage.as_ref(),
                *all_platforms,
                platform_filter(cli, &ctx)?.as_deref(),
                *require_storage,
            )
            .await
        }
        Commands::Report {
            format,
            from,
            pr_number,
            out,
        } => {
            let source = if let Some(dir) = from {
                commands::RunSource::Copy(dir.clone())
            } else {
                let ctx = resolve_context(cli, current_dir, env)?;
                commands::RunSource::Own(
                    gleon_core::paths::GleonPaths::new(&ctx.base_dir).runs_latest(),
                )
            };
            let storage = get_storage_config(env);
            commands::report::run_report(env, storage, *format, &source, *pr_number, out.as_deref())
                .await
        }
        Commands::Approve { paths, from } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            commands::approve::run_approve(&ctx, paths, from)
        }
        Commands::Dashboard {
            from,
            out,
            truncate_history,
            push,
        } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            commands::dashboard::run_dashboard(
                &ctx,
                get_storage_config(env),
                from.as_deref(),
                out.as_deref(),
                *truncate_history,
                *push,
            )
            .await
        }
        Commands::Clean {
            dry_run,
            screenshots,
            skip_gitignore,
            keep_runs,
        } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            let options = gleon_core::ops::clean::CleanOptions {
                dry_run: *dry_run,
                screenshots: *screenshots,
                skip_gitignore: *skip_gitignore,
                keep_runs: *keep_runs,
            };
            commands::clean::run_clean(&ctx, &options)
        }
        // The exit code is the test command's, not a gleon outcome.
        Commands::Test { command } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            return Ok(commands::test::run_test(&ctx, env, command, current_dir).await);
        }
        Commands::Gc {
            dry_run,
            grace_period_hours,
            force,
        } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            let storage = get_storage_config(env);
            commands::gc::run_gc(
                &ctx,
                storage.as_ref(),
                *dry_run,
                *grace_period_hours,
                *force,
            )
            .await
        }
    };

    Ok(code.into())
}
