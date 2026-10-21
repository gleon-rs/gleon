//! Implementation of the `gleon init` subcommand.

use std::path::Path;

use gleon_core::{context::ResolvedContext, ops::Template};
use tracing::{info, warn};

use crate::{commands::report_failure, exit_code::ExitCode};

/// Runs the `gleon init` subcommand in `dir` with the config of `template` (`None`: by the
/// `pubspec.yaml`).
///
/// Like `git init`, it initializes `dir` itself: the workspace `ctx` found above it (if any) is
/// only what a new workspace nests in, and a nested workspace is named as such.
pub fn run_init(ctx: &ResolvedContext, dir: &Path, template: Option<Template>) -> ExitCode {
    let target = gleon_core::paths::resolved(dir);
    let enclosing =
        (ctx.config.is_some() && ctx.base_dir != target && target.starts_with(&ctx.base_dir))
            .then(|| ctx.base_dir.clone());
    let mut ctx = ctx.clone();
    ctx.base_dir = target;
    let res = match gleon_core::ops::init_workspace(&ctx, template) {
        Ok(res) => res,
        Err(e) => return report_failure("Error initializing workspace", &e),
    };

    info!("Initialized gleon workspace at {}", res.gleon_dir.display());
    let config = res.config_path.display();
    match res.created {
        Some(Template::Flutter) => info!(
            "Created {config} for a Flutter package (pubspec.yaml found): the goldens under \
             every test/ are the baselines (`baselines: goldens`), compared exactly, their text \
             on this platform; `gleon init --generic` writes the generic template instead. \
             Next: `gleon test -- flutter test`, then `gleon report html --out \
             .gleon/runs/latest/report.html` and `gleon approve`"
        ),
        Some(Template::Generic) => info!("Created default configuration file at {config}"),
        // The flags choose the template of a new config only: say so rather than ignore them.
        None if template.is_some() => warn!(
            "{config} exists and was left as is: --flutter and --generic only choose the \
             template of a new one"
        ),
        None => info!("Kept the existing configuration file {config}"),
    }
    if let Some(enclosing) = enclosing.filter(|_| res.created.is_some()) {
        warn!(
            "This is a nested workspace inside the one at {}: its config, not that one's, now \
             applies to the goldens below {}",
            enclosing.display(),
            ctx.base_dir.display()
        );
    }
    ExitCode::Success
}
