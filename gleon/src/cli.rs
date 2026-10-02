//! CLI Argument parser definition for gleon.

use clap::{Parser, Subcommand, ValueEnum};

/// The formats of `gleon report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReportFormat {
    /// The PR comment (Markdown)
    Markdown,
    /// The HTML report of the failures
    Html,
    /// `JUnit` XML
    #[value(alias = "junit.xml", alias = "xml")]
    Junit,
    /// The case reports of the run (JSON)
    Json,
}

/// The main CLI structure for gleon.
#[derive(Parser, Debug)]
#[command(
    name = "gleon",
    version,
    about = "Universal visual regression testing CLI"
)]
pub struct Cli {
    /// Override the active git branch context
    #[arg(short = 'b', long = "branch", global = true)]
    pub branch: Option<String>,

    /// Override the OS component of the platform context (e.g. macos, linux, windows)
    #[arg(long = "os", global = true)]
    pub os: Option<String>,

    /// Override the CPU architecture component of the platform context (e.g. aarch64, `x86_64`)
    #[arg(long = "arch", global = true)]
    pub arch: Option<String>,

    /// Override the renderer identifier of the platform context (e.g. flutter-3.22, chrome-126)
    #[arg(long = "renderer", global = true)]
    pub renderer: Option<String>,

    /// Additional isolation labels (repeatable: --label key=val)
    #[arg(long = "label", global = true, value_parser = parse_label)]
    pub labels: Vec<(String, String)>,

    /// Override the active platform with an opaque custom string
    #[arg(short = 'p', long = "platform", global = true)]
    pub platform: Option<String>,

    /// Enable verbose logging (DEBUG level)
    #[arg(short = 'v', long = "verbose", global = true, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Suppress informational output (only show WARN/ERROR)
    #[arg(short = 'q', long = "quiet", global = true)]
    pub quiet: bool,

    /// Path to a custom configuration file
    #[arg(short = 'c', long = "config", global = true)]
    pub config: Option<std::path::PathBuf>,

    /// Enforce strict licensing compliance (hard fail with exit code 42 on violations)
    #[arg(
        long = "strict",
        global = true,
        env = "GLEON_STRICT",
        value_parser = clap::builder::FalseyValueParser::new()
    )]
    pub strict: bool,

    /// The target branch to compare against (defaults to 'main')
    #[arg(
        long = "target-branch",
        global = true,
        env = "GLEON_TARGET_BRANCH",
        default_value = "main"
    )]
    pub target_branch: String,

    /// The subcommand to execute
    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// Parses the process arguments like [`Parser::parse`], plus the checks clap cannot express
    /// between a global flag and the flags of a subcommand: `--all` excludes `--platform` even when
    /// the flag comes before the subcommand. Exits with clap's usage error otherwise.
    #[must_use]
    pub fn parse_checked() -> Self {
        let cli = Self::parse();
        if let Err(error) = cli.check() {
            error.exit();
        }
        cli
    }

    /// The cross-level checks of [`Self::parse_checked`].
    ///
    /// # Errors
    /// Returns an argument conflict when `--all` and `--platform` are both given.
    pub fn check(&self) -> Result<(), clap::Error> {
        let is_all = matches!(
            self.command,
            Commands::Pull {
                all_platforms: true
            } | Commands::Push {
                all_platforms: true
            }
        );
        if is_all && self.platform.is_some() {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                "the argument '--all' cannot be used with '--platform <PLATFORM>'\n",
            ));
        }
        Ok(())
    }
}

impl From<&Cli> for gleon_core::context::ContextOptions {
    fn from(cli: &Cli) -> Self {
        Self {
            config_path: cli.config.clone(),
            os: cli.os.clone(),
            arch: cli.arch.clone(),
            renderer: cli.renderer.clone(),
            labels: cli.labels.clone(),
            platform: cli.platform.clone(),
            branch: cli.branch.clone(),
            target_branch: cli.target_branch.clone(),
        }
    }
}

fn parse_artifacts_dir(s: &str) -> Result<gleon_core::config::ArtifactsDir, String> {
    gleon_core::config::ArtifactsDir::new(s).map_err(|e| e.to_string())
}

fn parse_label(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .ok_or_else(|| format!("invalid label: no '=' found in '{s}'"))
        .and_then(|(key, val)| {
            let key = key.trim().to_string();
            let val = val.trim().to_string();
            if key.is_empty() {
                return Err("invalid label: key cannot be empty".to_string());
            }
            if val.is_empty() {
                return Err("invalid label: value cannot be empty".to_string());
            }
            Ok((key, val))
        })
}

/// The available subcommands in gleon.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum Commands {
    /// Initialize gleon directory structure and default configuration
    Init,
    /// Print resolved configuration and active status
    Status {
        /// Format output as JSON
        #[arg(long = "json")]
        json: bool,
    },
    /// Stage actual screenshots as new baselines
    Stage {
        /// Optional path filters to stage
        #[arg(value_name = "PATHS")]
        paths: Vec<std::path::PathBuf>,
    },
    /// Run visual diff comparison against baseline images; writes a case report per screenshot
    Diff {
        /// Automatically pull the latest remote baselines before diffing
        #[arg(long = "auto-pull")]
        auto_pull: bool,
        /// Directory for the images of failed screenshots, relative to the workspace root:
        /// `.gleon/runs/latest/artifacts` (the default) or a directory under `.gleon/runs/`
        /// outside `latest/`; beats `GLEON_ARTIFACTS_DIR` and `artifacts:` of the config
        #[arg(long, value_name = "DIR", value_parser = parse_artifacts_dir)]
        artifacts: Option<gleon_core::config::ArtifactsDir>,
    },
    /// Lint baseline JSON manifests for schema validity and Git conflict markers (only the
    /// global `--platform`'s when it is given)
    #[command(alias = "lint")]
    LintManifests,
    /// Interactively resolve Git merge conflicts in baseline manifests
    Resolve {
        /// Optional specific test path filter
        #[arg(value_name = "TEST")]
        test_path: Option<String>,
        /// Download missing baseline blobs from remote storage during resolution
        #[arg(long)]
        fetch: bool,
    },
    /// Run a test command as one run: `gleon test -- flutter test`
    ///
    /// Gives the command a run id (`GLEON_RUN_ID`, kept when already set) and metrics
    /// (`GLEON_METRICS=1`), records the run in `.gleon/runs/latest/run.json` for `gleon report`,
    /// `gleon dashboard` and `gleon approve`, and exits with the command's exit code. Runs of one
    /// workspace go one after another: a concurrent run replaces the run file.
    Test {
        /// The test command and its arguments
        #[arg(
            value_name = "COMMAND",
            required = true,
            last = true,
            num_args = 1..
        )]
        command: Vec<String>,
    },
    /// Pull latest baselines from remote storage
    Pull {
        /// Pull blobs for all platforms under .gleon/manifests/ instead of only the active platform
        #[arg(short = 'a', long = "all", conflicts_with = "platform")]
        all_platforms: bool,
    },
    /// Push staged changes and report to remote storage
    Push {
        /// Push blobs for all platforms under .gleon/manifests/ instead of only the active platform
        #[arg(short = 'a', long = "all", conflicts_with = "platform")]
        all_platforms: bool,
    },
    /// Clean up unreferenced baseline blobs from remote storage
    Gc {
        /// Preview orphan blobs that would be deleted without actually deleting them
        #[arg(long)]
        dry_run: bool,

        /// Grace period in hours (blobs modified within this window are preserved, minimum 24 hours)
        #[arg(long, default_value = "24")]
        grace_period_hours: u32,

        /// Force execution bypassing safety checks (shallow clones, single ref, git traversal errors)
        #[arg(long)]
        force: bool,
    },
    /// Clean local screenshot files, untrack them from Git, and update .gitignore
    Clean {
        /// Preview changes without deleting files or modifying Git state
        #[arg(long)]
        dry_run: bool,

        /// Skip appending ignore rules to .gitignore
        #[arg(long)]
        skip_gitignore: bool,

        /// Skip deleting .gleon/runs and .gleon/diffs temporary directories
        #[arg(long)]
        keep_runs: bool,
    },
    /// Render the case reports of the latest run (a PR comment, HTML, `JUnit` or JSON)
    Report {
        /// Format of the report
        #[arg(value_name = "FORMAT", value_enum)]
        format: ReportFormat,
        /// A copy of `.gleon/runs/latest` (with `cases/`), e.g. a downloaded CI artifact, read as
        /// it is (default: the latest run of the workspace, picked with `GLEON_RUN_ID`)
        #[arg(long, value_name = "DIR")]
        from: Option<std::path::PathBuf>,
        /// Pull Request number
        #[arg(long)]
        pr_number: Option<u64>,
        /// Output file path
        #[arg(short = 'o', long)]
        out: Option<std::path::PathBuf>,
    },
    /// Approve the candidates of failed cases as new baselines
    Approve {
        /// Optional test names or golden paths to approve (filters by prefix)
        #[arg(value_name = "PATHS")]
        paths: Vec<std::path::PathBuf>,

        /// Copies of `.gleon/runs/latest` (`cases/` and `artifacts/`) to approve from, e.g. the
        /// downloaded artifacts of CI runs, each read as it is (repeatable; default: the latest
        /// run of the workspace, picked with `GLEON_RUN_ID`)
        #[arg(long = "from", value_name = "DIR")]
        from: Vec<std::path::PathBuf>,
    },
    /// Add the latest run to the history and compile the static dashboard
    Dashboard {
        /// A copy of `.gleon/runs/latest` (with `cases/`), read as it is (default: the latest run
        /// of the workspace, picked with `GLEON_RUN_ID`)
        #[arg(long, value_name = "DIR")]
        from: Option<std::path::PathBuf>,

        /// Output file path for compiled dashboard HTML
        #[arg(short = 'o', long)]
        out: Option<std::path::PathBuf>,

        /// Maximum number of runs kept in history.json (the oldest are dropped): the file is read
        /// and rewritten whole on every run, so it must not grow without bound
        #[arg(long = "truncate-history", value_name = "NUM", default_value = "200")]
        truncate_history: std::num::NonZeroUsize,

        /// Upload history.json and dashboard.html to remote storage
        #[arg(long)]
        push: bool,
    },
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
mod tests {
    use super::*;

    #[test]
    fn verify_cli() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn test_parse_label_with_equals_in_value() {
        let (k, v) = parse_label("url=http://host:8080").unwrap();
        assert_eq!(k, "url");
        assert_eq!(v, "http://host:8080");
    }

    #[test]
    fn test_parse_branch_flag() -> Result<(), clap::Error> {
        let args = ["gleon", "-b", "feature-test", "status"];
        let cli = Cli::try_parse_from(args)?;
        assert_eq!(cli.branch, Some("feature-test".to_string()));
        assert_eq!(cli.command, Commands::Status { json: false });
        Ok(())
    }

    #[test]
    fn test_parse_report_reads_the_latest_run_by_default() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["gleon", "report", "markdown"])?;
        assert_eq!(
            cli.command,
            Commands::Report {
                format: ReportFormat::Markdown,
                from: None,
                pr_number: None,
                out: None,
            }
        );

        let cli_override = Cli::try_parse_from(["gleon", "report", "html", "--from", "dl/latest"])?;
        assert_eq!(
            cli_override.command,
            Commands::Report {
                format: ReportFormat::Html,
                from: Some(std::path::PathBuf::from("dl/latest")),
                pr_number: None,
                out: None,
            }
        );
        Ok(())
    }

    #[test]
    fn test_parse_test_command() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from([
            "gleon",
            "test",
            "--",
            "flutter",
            "test",
            "--plain-name",
            "x",
        ])?;
        assert_eq!(
            cli.command,
            Commands::Test {
                command: ["flutter", "test", "--plain-name", "x"]
                    .map(String::from)
                    .to_vec(),
            }
        );
        assert!(Cli::try_parse_from(["gleon", "test"]).is_err());
        assert!(
            Cli::try_parse_from(["gleon", "test", "flutter"]).is_err(),
            "the command follows `--`"
        );
        Ok(())
    }

    #[test]
    fn test_parse_diff_artifacts() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["gleon", "diff", "--artifacts", ".gleon/runs/ci"])?;
        assert_eq!(
            cli.command,
            Commands::Diff {
                auto_pull: false,
                artifacts: Some(gleon_core::config::ArtifactsDir::new(".gleon/runs/ci").unwrap()),
            }
        );
        assert!(Cli::try_parse_from(["gleon", "diff", "--artifacts", "/tmp/out"]).is_err());
        Ok(())
    }

    #[test]
    fn test_parse_branch_flag_long() -> Result<(), clap::Error> {
        let args = ["gleon", "--branch", "another-branch", "diff"];
        let cli = Cli::try_parse_from(args)?;
        assert_eq!(cli.branch, Some("another-branch".to_string()));
        assert_eq!(
            cli.command,
            Commands::Diff {
                auto_pull: false,
                artifacts: None
            }
        );
        assert_eq!(cli.target_branch, "main"); // Default value
        Ok(())
    }

    #[test]
    fn test_parse_lint_and_resolve_commands() -> Result<(), clap::Error> {
        // `--platform` is one global flag, wherever it is written.
        for args_lint in [
            ["gleon", "lint-manifests", "--platform", "linux-x86_64"],
            ["gleon", "--platform", "linux-x86_64", "lint-manifests"],
        ] {
            let cli_lint = Cli::try_parse_from(args_lint)?;
            assert_eq!(cli_lint.command, Commands::LintManifests);
            assert_eq!(cli_lint.platform.as_deref(), Some("linux-x86_64"));
        }

        let args_resolve = ["gleon", "resolve", "--fetch", "auth/login"];
        let cli_resolve = Cli::try_parse_from(args_resolve)?;
        assert_eq!(
            cli_resolve.command,
            Commands::Resolve {
                test_path: Some("auth/login".to_string()),
                fetch: true,
            }
        );

        assert!(
            Cli::try_parse_from(["gleon", "diff", "--resolve"]).is_err(),
            "conflicts are resolved by `gleon resolve`"
        );
        Ok(())
    }

    #[test]
    fn test_parse_target_branch_flag() -> Result<(), clap::Error> {
        let args = ["gleon", "--target-branch", "develop", "diff"];
        let cli = Cli::try_parse_from(args)?;
        assert_eq!(cli.target_branch, "develop");
        Ok(())
    }

    #[test]
    fn test_parse_platform_flags() -> Result<(), clap::Error> {
        let args = [
            "gleon",
            "--os",
            "linux",
            "--arch",
            "x86_64",
            "--renderer",
            "chrome",
            "--label",
            "theme=dark",
            "--label",
            "locale=en",
            "stage",
        ];
        let cli = Cli::try_parse_from(args)?;
        assert_eq!(cli.os, Some("linux".to_string()));
        assert_eq!(cli.arch, Some("x86_64".to_string()));
        assert_eq!(cli.renderer, Some("chrome".to_string()));
        assert_eq!(
            cli.labels,
            vec![
                ("theme".to_string(), "dark".to_string()),
                ("locale".to_string(), "en".to_string())
            ]
        );
        assert_eq!(cli.command, Commands::Stage { paths: vec![] });
        Ok(())
    }

    #[test]
    fn test_parse_legacy_platform_flag() -> Result<(), clap::Error> {
        let args = ["gleon", "--platform", "custom-opaque", "stage"];
        let cli = Cli::try_parse_from(args)?;
        assert_eq!(cli.platform, Some("custom-opaque".to_string()));
        assert_eq!(cli.command, Commands::Stage { paths: vec![] });
        Ok(())
    }

    #[test]
    fn test_parse_verbose_flag() -> Result<(), clap::Error> {
        let args = ["gleon", "-v", "status"];
        let cli = Cli::try_parse_from(args)?;
        assert!(cli.verbose);
        assert!(!cli.quiet);

        let args_long = ["gleon", "--verbose", "status"];
        let cli_long = Cli::try_parse_from(args_long)?;
        assert!(cli_long.verbose);
        assert!(!cli_long.quiet);
        Ok(())
    }

    #[test]
    fn test_parse_quiet_flag() -> Result<(), clap::Error> {
        let args = ["gleon", "-q", "status"];
        let cli = Cli::try_parse_from(args)?;
        assert!(cli.quiet);
        assert!(!cli.verbose);

        let args_long = ["gleon", "--quiet", "status"];
        let cli_long = Cli::try_parse_from(args_long)?;
        assert!(cli_long.quiet);
        assert!(!cli_long.verbose);
        Ok(())
    }

    #[test]
    fn test_parse_invalid_flag() {
        let args = ["gleon", "--invalid-flag", "status"];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_label_errors() {
        assert!(parse_label("no_equals_sign").is_err());
        assert!(parse_label("=value").is_err());
        assert!(parse_label("key=").is_err());
        assert!(parse_label("  =  ").is_err());
    }

    #[test]
    fn test_parse_pull_push_conflicting_all_and_platform() {
        let pull_conflict =
            Cli::try_parse_from(["gleon", "pull", "--all", "--platform", "macos-aarch64"]);
        assert!(pull_conflict.is_err());

        let push_conflict = Cli::try_parse_from(["gleon", "push", "-a", "-p", "macos-aarch64"]);
        assert!(push_conflict.is_err());
        let global_conflict =
            Cli::try_parse_from(["gleon", "-p", "macos-aarch64", "pull", "--all"]).unwrap();
        assert_eq!(
            global_conflict.check().unwrap_err().kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "the global flag before the subcommand conflicts too"
        );
        assert!(
            Cli::try_parse_from(["gleon", "-p", "x", "pull"])
                .unwrap()
                .check()
                .is_ok()
        );

        let pull_ok = Cli::try_parse_from(["gleon", "pull", "--all"]);
        assert!(pull_ok.is_ok());

        let push_ok = Cli::try_parse_from(["gleon", "push", "--platform", "linux-x86_64"]);
        assert!(push_ok.is_ok());
    }

    #[test]
    fn test_parse_approve_command() -> Result<(), clap::Error> {
        let args = ["gleon", "approve", "--from", ".gleon/diffs", "auth/login"];
        let cli = Cli::try_parse_from(args)?;
        assert_eq!(
            cli.command,
            Commands::Approve {
                paths: vec![std::path::PathBuf::from("auth/login")],
                from: vec![std::path::PathBuf::from(".gleon/diffs")],
            }
        );
        let several = Cli::try_parse_from(["gleon", "approve", "--from", "a", "--from", "b"])?;
        assert!(matches!(several.command, Commands::Approve { from, .. } if from.len() == 2));

        let args_no_from = ["gleon", "approve"];
        let cli_no_from = Cli::try_parse_from(args_no_from)?;
        assert_eq!(
            cli_no_from.command,
            Commands::Approve {
                paths: vec![],
                from: vec![],
            }
        );
        Ok(())
    }

    #[test]
    fn test_parse_dashboard_command() -> Result<(), clap::Error> {
        let args = ["gleon", "dashboard", "--truncate-history", "5", "--push"];
        let cli = Cli::try_parse_from(args)?;
        assert_eq!(
            cli.command,
            Commands::Dashboard {
                from: None,
                out: None,
                truncate_history: std::num::NonZeroUsize::new(5).unwrap(),
                push: true,
            }
        );
        let default = Cli::try_parse_from(["gleon", "dashboard"])?;
        assert!(matches!(
            default.command,
            Commands::Dashboard { truncate_history, .. } if truncate_history.get() == 200
        ));
        Ok(())
    }
}
