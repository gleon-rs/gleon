//! gleon CLI wrapper binary.

use std::{io::IsTerminal as _, path::Path};

use anyhow::Context as _;
use cli::{Cli, Commands};
use gleon_core::env::EnvProvider;
use gleon_model::platform::PlatformKey;
use tracing::debug;

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

    debug!("gleon {} starting", env!("CARGO_PKG_VERSION"));

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

    // The license check runs before every command.
    let license_status = gleon_core::license::LicenseGate::verify(&env);
    let strict = match gleon_core::env::flag(&env, STRICT_ENV) {
        Ok(on) => cli.strict || on,
        Err(e) => std::process::exit(commands::report_failure("Invalid environment", &e).into()),
    };
    let decision = gleon_core::license::enforce_policy(license_status, strict, &env);
    for line in &decision.message {
        eprintln!("{line}");
    }
    if let Some(annotation) = &decision.gha_annotation {
        eprintln!("{annotation}");
    }
    if decision.action == gleon_core::license::EnforcementAction::Block {
        std::process::exit(exit_code::ExitCode::LicenseBlocked.into());
    }

    // A command fails with the context it gave its error (or the context could not be resolved):
    // logged once, with its whole cause chain.
    let exit_code = match run(&cli, &current_dir, &env).await {
        Ok(code) => code,
        Err(e) => i32::from(commands::report(&e)),
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// The variable that does what `--require-storage` of `pull` and `push` does.
const REQUIRE_STORAGE_ENV: &str = "GLEON_REQUIRE_STORAGE";

/// The environment variable of `--strict`.
const STRICT_ENV: &str = "GLEON_STRICT";

/// The context of an error that keeps a command from starting.
const CONTEXT_FAILED: &str = "Context resolution failed";

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
    .context(CONTEXT_FAILED)
}

/// The platform the global `--platform` names, as the context resolved it: its canonical key
/// (lowercase, `macos-arm64` as `macos-aarch64`), the directory `gleon stage` writes, for the
/// commands that read the manifests of one platform (`lint`, `resolve`, `pull`, `push`); `None`
/// without the flag.
fn platform_filter(
    cli: &Cli,
    ctx: &gleon_core::context::ResolvedContext,
) -> anyhow::Result<Option<PlatformKey>> {
    if cli.platform.is_none() {
        return Ok(None);
    }
    Ok(Some(ctx.platform.key().context(CONTEXT_FAILED)?))
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
            commands::lint::run_lint(&ctx, platform_filter(cli, &ctx)?.as_ref())
        }
        Commands::Resolve { test_path, fetch } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            commands::resolve::run_resolve(
                &ctx,
                platform_filter(cli, &ctx)?.as_ref(),
                test_path.as_deref(),
                *fetch,
                get_storage_config(env),
                std::io::stdin().is_terminal(),
            )
            .await
        }
        Commands::Pull {
            all_platforms,
            require_storage,
        } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            let storage = get_storage_config(env);
            // The variable is checked even with the flag: an invalid value is always an error.
            let from_env =
                gleon_core::env::flag(env, REQUIRE_STORAGE_ENV).context("Invalid environment")?;
            commands::pull::run_pull(
                &ctx,
                storage.as_ref(),
                *all_platforms,
                platform_filter(cli, &ctx)?.as_ref(),
                *require_storage || from_env,
            )
            .await
            .map(|()| exit_code::ExitCode::Success)
        }
        Commands::Push {
            all_platforms,
            require_storage,
        } => {
            let ctx = resolve_context(cli, current_dir, env)?;
            let storage = get_storage_config(env);
            // The variable is checked even with the flag: an invalid value is always an error.
            let from_env =
                gleon_core::env::flag(env, REQUIRE_STORAGE_ENV).context("Invalid environment")?;
            commands::push::run_push(
                &ctx,
                storage.as_ref(),
                *all_platforms,
                platform_filter(cli, &ctx)?.as_ref(),
                *require_storage || from_env,
            )
            .await
            .map(|()| exit_code::ExitCode::Success)
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
            return commands::test::run_test(&ctx, env, command, current_dir).await;
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
    }?;

    Ok(code.into())
}
