//! Per-process state of an integration: the workspaces of its goldens, their compiled configs
//! (compiled again when a file changes) and the inputs the integration passes in.

#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use gleon_engine::config::Zone;
use gleon_model::{
    case::{RUN_ID_ENV, RunId},
    config::{ArtifactsDir, GITIGNORE_LINES, GleonConfig, MetricsConfig},
    rules::{RuleMatch, RuleSet},
    tolerance::Tolerance,
};

use crate::{error::Failure, text};

/// Inputs of a session, as passed by the integration.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    /// Whether goldens belong to workspaces: the nearest directory above a golden with
    /// `.gleon/gleon.yaml`. Without, every golden compares exactly and nothing is recorded.
    pub finds_workspaces: bool,
    /// Raw `GLEON_METRICS` value, `None` when unset.
    pub metrics_env: Option<String>,
    /// Raw `GLEON_ARTIFACTS_DIR` value, `None` when unset.
    pub artifacts_env: Option<String>,
    /// Raw `GLEON_RUN_ID` value, `None` when unset.
    pub run_id_env: Option<String>,
    /// Who calls.
    pub integration: Integration,
}

/// The integration calling the engine: recorded in case reports, and naming its failure
/// artifacts in its own convention.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Integration {
    /// Name, e.g. `gleon_flutter`.
    pub tool: String,
    /// Version.
    pub tool_version: String,
    /// Renderer identifier (`flutter-3.47.5`), when known.
    pub renderer: Option<String>,
    /// File names of failure artifacts.
    pub artifacts: ArtifactNames,
}

/// File name patterns of the failure artifacts; `{name}` is the golden's file name without its
/// extension. Flutter uses `{name}_masterImage.png`, `{name}_testImage.png`, `{name}_gleonDiff.png`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactNames {
    /// The golden, copied as is.
    pub golden: String,
    /// The candidate of this run.
    pub candidate: String,
    /// The diff visualization (mismatches only).
    pub diff: String,
}

impl ArtifactNames {
    /// The placeholder replaced by the golden's name.
    pub const PLACEHOLDER: &str = "{name}";

    /// `pattern` for the golden `name`.
    #[must_use]
    pub fn file(pattern: &str, name: &str) -> String {
        pattern.replace(Self::PLACEHOLDER, name)
    }

    /// Checks that each pattern names one file of the failures directory per golden, distinct
    /// from the others even on case-insensitive file systems (macOS, Windows).
    ///
    /// # Errors
    /// Returns why a pattern is invalid.
    pub fn validate(&self) -> Result<(), String> {
        let patterns = [&self.golden, &self.candidate, &self.diff];
        if patterns
            .iter()
            .any(|pattern| !pattern.contains(Self::PLACEHOLDER))
        {
            return Err(format!(
                "every failure artifact pattern must contain `{}`",
                Self::PLACEHOLDER
            ));
        }
        if patterns.iter().any(|pattern| pattern.contains(['/', '\\'])) {
            return Err("failure artifact patterns are file names, without `/` or `\\`".to_owned());
        }
        let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        if same(&self.golden, &self.candidate)
            || same(&self.golden, &self.diff)
            || same(&self.candidate, &self.diff)
        {
            return Err("the failure artifact patterns must differ (ignoring case)".to_owned());
        }
        Ok(())
    }
}

/// A workspace: the directory containing `.gleon/gleon.yaml`.
#[derive(Debug)]
pub struct Workspace {
    /// The root, with symbolic links resolved.
    pub root: PathBuf,
    config: Mutex<Option<Cached>>,
    has_gitignore: AtomicBool,
}

/// A parsed, validated config with its globs compiled.
#[derive(Debug)]
struct Compiled {
    metrics: MetricsConfig,
    rules: RuleSet,
    /// The artifacts directory, `GLEON_ARTIFACTS_DIR` applied.
    artifacts: Arc<ArtifactsDir>,
}

/// The config compiled from one version of the file (or why it is invalid).
#[derive(Debug)]
struct Cached {
    text: String,
    compiled: Result<Arc<Compiled>, String>,
}

impl Workspace {
    const fn new(root: PathBuf) -> Self {
        Self {
            root,
            config: Mutex::new(None),
            has_gitignore: AtomicBool::new(false),
        }
    }

    /// `<root>/.gleon`.
    pub fn gleon_dir(&self) -> PathBuf {
        self.root.join(".gleon")
    }

    /// `.gleon/gleon.yaml` as shown in messages, with the separators of the platform.
    pub fn config_display(&self) -> String {
        self.root
            .join(".gleon")
            .join("gleon.yaml")
            .display()
            .to_string()
    }

    /// Creates `.gleon/.gitignore` (the lines of `gleon init`, which ignore `runs/`) unless it
    /// exists; tried until it succeeds once per session.
    ///
    /// # Errors
    /// Returns the I/O error of creating it.
    pub fn ensure_gitignore(&self) -> io::Result<()> {
        if self.has_gitignore.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut lines = GITIGNORE_LINES.join("\n");
        lines.push('\n');
        gleon_model::fs::create_new(&self.gleon_dir().join(".gitignore"), lines.as_bytes())?;
        self.has_gitignore.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// The config, compiled again only when the file's content changed (long-lived processes such
    /// as watch modes see every edit). Reading the small file per golden costs microseconds next
    /// to the golden itself; compiling happens outside the lock. `artifacts_env` is the session's
    /// `GLEON_ARTIFACTS_DIR`, the same for every call.
    fn compiled(&self, artifacts_env: Option<&ArtifactsDir>) -> Result<Arc<Compiled>, String> {
        let path = self.gleon_dir().join("gleon.yaml");
        let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read it: {e}"))?;
        let cached = self
            .cache()
            .as_ref()
            .filter(|cached| cached.text == text)
            .map(|cached| cached.compiled.clone());
        if let Some(compiled) = cached {
            return compiled;
        }
        let compiled = compile(&text, artifacts_env);
        *self.cache() = Some(Cached {
            text,
            compiled: compiled.clone(),
        });
        compiled
    }

    fn cache(&self) -> MutexGuard<'_, Option<Cached>> {
        // Nothing panics while the lock is held; a poisoned cache is still consistent.
        self.config.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The path of `file` relative to the root, `/`-separated with the case as on disk, or `None`
    /// when it lies outside. The file and its directories may be missing (a new golden). Windows
    /// paths compare case-insensitively.
    pub fn relative_path(&self, file: &Path) -> Option<String> {
        let file = canonical_file(file).ok()?;
        let mut inside = file.components();
        for root_part in self.root.components() {
            let part = inside.next()?;
            if !same_component(root_part, part) {
                return None;
            }
        }
        let mut relative = String::new();
        for part in inside {
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(&part.as_os_str().to_string_lossy());
        }
        (!relative.is_empty()).then_some(relative)
    }
}

fn compile(text: &str, artifacts_env: Option<&ArtifactsDir>) -> Result<Arc<Compiled>, String> {
    let config = GleonConfig::from_yaml_str(text).map_err(|e| e.to_string())?;
    let rules = RuleSet::new(&config).map_err(|e| format!("invalid glob set: {e}"))?;
    Ok(Arc::new(Compiled {
        metrics: config.metrics,
        artifacts: Arc::new(config.artifacts_dir(artifacts_env, None)),
        rules,
    }))
}

#[cfg(not(windows))]
fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    a == b
}

/// Windows paths are case-insensitive (the CLI folds case for names too). Both sides come from
/// `canonicalize`, so they almost always match as they are; only a mismatch pays for folding.
#[cfg(windows)]
fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    a == b
        || a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
}

/// `path` absolute with symbolic links resolved.
#[cfg(not(windows))]
fn canonical(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// `path` absolute with symbolic links resolved, without the `\\?\` verbatim prefix of
/// `canonicalize` (which Dart never shows).
#[cfg(windows)]
fn canonical(path: &Path) -> io::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)?;
    let text = resolved.to_string_lossy();
    Ok(text.strip_prefix(r"\\?\UNC\").map_or_else(
        || {
            text.strip_prefix(r"\\?\")
                .map_or_else(|| resolved.clone(), PathBuf::from)
        },
        |share| PathBuf::from(format!(r"\\{share}")),
    ))
}

/// [`canonical`] of a file that may not exist yet: its nearest existing ancestor resolved, the
/// missing components kept as they are.
fn canonical_file(path: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        match canonical(existing) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(existing.file_name().ok_or(e)?);
                existing = existing
                    .parent()
                    .filter(|dir| !dir.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
            }
            resolved => {
                return Ok(missing
                    .into_iter()
                    .rev()
                    .fold(resolved?, |dir, name| dir.join(name)));
            }
        }
    }
}

/// Walks up from `dir` (inclusive) to the first directory with a `.gleon/gleon.yaml` file.
fn find_root(mut dir: PathBuf) -> Option<PathBuf> {
    loop {
        dir.push(".gleon");
        dir.push("gleon.yaml");
        let found = dir.is_file();
        dir.pop();
        dir.pop();
        if found {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The workspaces found so far: by directory of a golden, and by root so that every golden of a
/// workspace shares one compiled config.
#[derive(Debug, Default)]
struct Workspaces {
    by_dir: HashMap<PathBuf, Option<Arc<Workspace>>>,
    by_root: HashMap<PathBuf, Arc<Workspace>>,
}

/// Per-process state; tests create their own with injected inputs.
#[derive(Debug)]
pub struct Session {
    /// Why every call fails (invalid inputs, an invalid `GLEON_METRICS`), if it does.
    failure: Option<Failure>,
    finds_workspaces: bool,
    workspaces: Mutex<Workspaces>,
    /// `GLEON_METRICS`: `None` when unset, else whether it turns metrics on.
    metrics_override: Option<bool>,
    /// `GLEON_ARTIFACTS_DIR`, when set.
    artifacts_override: Option<ArtifactsDir>,
    /// `GLEON_RUN_ID`, when set.
    pub run_id: Option<RunId>,
    /// Who calls.
    pub integration: Integration,
    warned: AtomicBool,
}

/// What one comparison uses and whether it records a case report.
#[derive(Debug)]
pub struct Plan {
    /// The effective tolerance: the call's, else the rule's, else exact.
    pub tolerance: Tolerance,
    /// The call's masks followed by the rule's.
    pub masks: Vec<Zone>,
    /// Whether the golden belongs to a workspace (failure messages point at `.gleon/gleon.yaml`
    /// otherwise).
    pub has_workspace: bool,
    /// The golden inside its workspace, when a rule of the workspace matches it.
    pub in_workspace: Option<InWorkspace>,
}

impl Plan {
    /// The golden and how to record its case report, when metrics are on.
    pub fn recorded(&self) -> Option<(&InWorkspace, Record)> {
        self.in_workspace
            .as_ref()
            .and_then(|golden| golden.record.map(|record| (golden, record)))
    }
}

/// A golden matched by a rule of its workspace: where its images and case report go.
#[derive(Debug)]
pub struct InWorkspace {
    /// The workspace.
    pub workspace: Arc<Workspace>,
    /// Canonical test name (the name of its case report and artifacts folder).
    pub name: String,
    /// Golden path relative to the root, `/`-separated.
    pub golden_path: String,
    /// The artifacts directory, relative to the root.
    pub artifacts: Arc<ArtifactsDir>,
    /// How to record the case report, when metrics are on.
    pub record: Option<Record>,
}

/// How to record a case report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Whether to print the console line.
    pub console: bool,
}

/// The rule matching a golden inside its workspace.
struct Rule {
    tolerance: Tolerance,
    masks: Vec<Zone>,
    golden: InWorkspace,
}

impl Session {
    /// Creates a session; an invalid environment override fails every call.
    #[must_use]
    pub fn new(options: SessionOptions) -> Self {
        let config_error = |message: String| Failure::config(format!("gleon: {message}"));
        let metrics_override = MetricsConfig::env_override(options.metrics_env.as_deref())
            .map_err(|e| config_error(e.to_string()));
        let artifacts_override = ArtifactsDir::from_env(options.artifacts_env.as_deref())
            .map_err(|e| config_error(e.to_string()));
        let run_id = RunId::from_env(options.run_id_env.as_deref())
            .map_err(|e| config_error(format!("{RUN_ID_ENV}: {e}")));
        let failure = metrics_override
            .as_ref()
            .err()
            .or_else(|| artifacts_override.as_ref().err())
            .or_else(|| run_id.as_ref().err())
            .cloned();
        Self {
            failure,
            finds_workspaces: options.finds_workspaces,
            workspaces: Mutex::default(),
            metrics_override: metrics_override.unwrap_or_default(),
            artifacts_override: artifacts_override.unwrap_or_default(),
            run_id: run_id.unwrap_or_default(),
            integration: options.integration,
            warned: AtomicBool::new(false),
        }
    }

    /// A session whose every call fails with `failure`.
    #[must_use]
    pub fn failed(failure: Failure) -> Self {
        Self {
            failure: Some(failure),
            ..Self::new(SessionOptions::default())
        }
    }

    /// Why every call of this session fails.
    pub const fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }

    /// The warning, returned once per session, when `GLEON_METRICS` asks for metrics but a golden
    /// of `plan` has no workspace to record them in.
    pub fn missing_workspace_warning(&self, plan: &Plan) -> Option<&'static str> {
        (self.metrics_override == Some(true)
            && !plan.has_workspace
            && !self.warned.swap(true, Ordering::Relaxed))
        .then_some(text::MISSING_WORKSPACE_WARNING)
    }

    /// The workspace of `golden` (which may be missing): the nearest directory above it with
    /// `.gleon/gleon.yaml`, looked up once per directory.
    fn workspace_of(&self, golden: &Path) -> Option<Arc<Workspace>> {
        if !self.finds_workspaces {
            return None;
        }
        let dir = canonical_file(golden).ok()?.parent()?.to_path_buf();
        if let Some(known) = self.lock().by_dir.get(&dir) {
            return known.clone();
        }
        // Walked outside the lock; a concurrent lookup of the same directory finds the same.
        let root = find_root(dir.clone());
        let mut workspaces = self.lock();
        let workspace = root.map(|root| {
            workspaces
                .by_root
                .entry(root.clone())
                .or_insert_with(|| Arc::new(Workspace::new(root)))
                .clone()
        });
        workspaces.by_dir.insert(dir, workspace.clone());
        workspace
    }

    fn lock(&self) -> MutexGuard<'_, Workspaces> {
        // Nothing panics while the lock is held; a poisoned map is still consistent.
        self.workspaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Plans the comparison of `golden` (which may be missing): the call's `tolerance` beats the
    /// `.gleon/gleon.yaml` rule of its workspace, which beats exact; the call's `masks` come
    /// first.
    ///
    /// # Errors
    /// Returns the failure of the session, or an invalid config or golden name.
    pub fn plan(
        &self,
        golden: &Path,
        tolerance: Option<Tolerance>,
        mut masks: Vec<Zone>,
    ) -> Result<Plan, Failure> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let workspace = self.workspace_of(golden);
        let has_workspace = workspace.is_some();
        let rule = match workspace {
            Some(workspace) => self.rule(workspace, golden)?,
            None => None,
        };
        let (rule_tolerance, in_workspace) = match rule {
            Some(Rule {
                tolerance,
                masks: rule_masks,
                golden,
            }) => {
                masks.extend(rule_masks);
                (Some(tolerance), Some(golden))
            }
            None => (None, None),
        };
        Ok(Plan {
            tolerance: tolerance.or(rule_tolerance).unwrap_or(Tolerance::Exact {}),
            masks,
            has_workspace,
            in_workspace,
        })
    }

    /// The rule of `golden` in `workspace`; `None` outside the workspace or when no rule applies.
    fn rule(&self, workspace: Arc<Workspace>, golden: &Path) -> Result<Option<Rule>, Failure> {
        let Some(golden_path) = workspace.relative_path(golden) else {
            return Ok(None);
        };
        let config_error = |message: String| {
            Failure::config(text::config_error(&workspace.config_display(), &message))
        };
        let compiled = workspace
            .compiled(self.artifacts_override.as_ref())
            .map_err(config_error)?;
        let RuleMatch::Matched {
            name,
            tolerance,
            masks,
            ..
        } = compiled
            .rules
            .resolve(&golden_path)
            .map_err(|e| config_error(e.to_string()))?
        else {
            return Ok(None);
        };
        let is_recorded = self.metrics_override.unwrap_or(compiled.metrics.enabled);
        Ok(Some(Rule {
            tolerance,
            masks,
            golden: InWorkspace {
                workspace,
                name,
                golden_path,
                artifacts: Arc::clone(&compiled.artifacts),
                record: is_recorded.then_some(Record {
                    console: compiled.metrics.console,
                }),
            },
        }))
    }
}

#[cfg(all(test, not(miri)))]
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
    use std::fs;

    use gleon_engine::config::Dimension;

    use super::*;
    use crate::error::ErrorKind;

    const YAML: &str = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "test/goldens/**/*.png"
    mode: ssim
    diff: { min_similarity: 0.6, color_tolerance: 64 }
    masks:
      - path: "**/clock.png"
        zones: [{ x: 0, y: 0, width: "25%", height: 10 }]
metrics:
  enabled: true
  console: false
"#;

    /// A workspace with `yaml` and an empty `test/goldens/<golden>`; returns its canonical root.
    fn workspace(yaml: &str, golden: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path()).unwrap();
        fs::create_dir_all(root.join(".gleon")).unwrap();
        fs::write(root.join(".gleon/gleon.yaml"), yaml).unwrap();
        let golden = root.join("test/goldens").join(golden);
        fs::create_dir_all(golden.parent().unwrap()).unwrap();
        fs::write(&golden, b"").unwrap();
        (dir, root)
    }

    fn session(metrics_env: Option<&str>) -> Session {
        Session::new(SessionOptions {
            finds_workspaces: true,
            metrics_env: metrics_env.map(str::to_owned),
            ..SessionOptions::default()
        })
    }

    fn pixel_mask(x: u32) -> Zone {
        Zone {
            x,
            y: 0,
            width: Dimension::Pixels(1),
            height: Dimension::Pixels(1),
        }
    }

    #[test]
    fn test_finds_the_workspace_above_the_golden_and_plans_the_rule() {
        let (_dir, root) = workspace(YAML, "Clock.png");
        let session = session(None);
        let golden = root.join("test/goldens/Clock.png");
        let plan = session.plan(&golden, None, vec![pixel_mask(5)]).unwrap();
        assert_eq!(
            plan.tolerance,
            Tolerance::Ssim {
                min_similarity: 0.6,
                color_tolerance: 64.0
            }
        );
        assert_eq!(plan.masks.len(), 2, "the call's masks, then the rule's");
        assert_eq!(plan.masks[0], pixel_mask(5));
        assert!(plan.has_workspace);
        let golden_in = plan.in_workspace.as_ref().unwrap();
        assert_eq!(golden_in.name, "test/goldens/clock");
        assert_eq!(golden_in.golden_path, "test/goldens/Clock.png");
        assert_eq!(golden_in.record, Some(Record { console: false }));
        assert_eq!(golden_in.workspace.root, root);
        assert_eq!(golden_in.artifacts.as_str(), ".gleon/runs/latest/artifacts");

        let call = Tolerance::Exact {};
        assert_eq!(
            session.plan(&golden, Some(call), vec![]).unwrap().tolerance,
            call,
            "the call beats the rule"
        );
    }

    #[test]
    fn test_a_missing_golden_is_planned_by_its_directory() {
        let (_dir, root) = workspace(YAML, "a.png");
        let session = session(None);
        let plan = session
            .plan(&root.join("test/goldens/new.png"), None, vec![])
            .unwrap();
        assert_eq!(
            plan.in_workspace.unwrap().golden_path,
            "test/goldens/new.png"
        );
        let plan = session
            .plan(&root.join("test/gone/new.png"), None, vec![])
            .unwrap();
        assert!(
            plan.in_workspace.is_none(),
            "a missing directory is outside"
        );
    }

    #[test]
    fn test_a_golden_outside_the_given_workspace_has_no_rule() {
        let (_dir, root) = workspace(YAML, "Clock.png");
        let other = tempfile::tempdir().unwrap();
        let rule = session(None).rule(
            Arc::new(Workspace::new(root)),
            &other.path().join("Clock.png"),
        );
        assert!(matches!(rule, Ok(None)));
    }

    #[test]
    fn test_metrics_env_overrides_the_config() {
        let (_dir, root) = workspace(YAML, "a.png");
        let golden = root.join("test/goldens/a.png");
        let plan = session(Some("0")).plan(&golden, None, vec![]).unwrap();
        assert!(plan.recorded().is_none());
        assert!(
            plan.in_workspace.is_some(),
            "images are kept without metrics"
        );
        let (_off, off_root) = workspace(
            "required_version: \">=0.1.0\"\nscreenshots: [{ include: \"**/*.png\" }]",
            "a.png",
        );
        let session = session(Some("TRUE"));
        let plan = session
            .plan(&off_root.join("test/goldens/a.png"), None, vec![])
            .unwrap();
        assert!(plan.recorded().unwrap().1.console, "console defaults to on");
    }

    #[test]
    fn test_an_invalid_metrics_env_fails_every_call() {
        let (_dir, root) = workspace(YAML, "a.png");
        let session = session(Some(" yes "));
        let failure = session.failure().unwrap();
        assert_eq!(failure.kind, ErrorKind::Config);
        assert_eq!(
            failure.message,
            "gleon: GLEON_METRICS must be 1, 0, true or false (got 'yes')"
        );
        let error = session
            .plan(&root.join("test/goldens/a.png"), None, vec![])
            .unwrap_err();
        assert_eq!(&error, failure);
    }

    #[test]
    fn test_invalid_environment_overrides_fail_every_call() {
        for (options, message) in [
            (
                SessionOptions {
                    artifacts_env: Some("build/out".to_owned()),
                    ..SessionOptions::default()
                },
                "gleon: GLEON_ARTIFACTS_DIR: 'build/out' must be `.gleon/runs/latest/artifacts`",
            ),
            (
                SessionOptions {
                    run_id_env: Some("a/b".to_owned()),
                    ..SessionOptions::default()
                },
                "gleon: GLEON_RUN_ID: a run id must be",
            ),
            (
                SessionOptions {
                    metrics_env: Some("x".to_owned()),
                    run_id_env: Some("a/b".to_owned()),
                    ..SessionOptions::default()
                },
                "gleon: GLEON_METRICS must be",
            ),
        ] {
            let failure = Session::new(options).failure().cloned().unwrap();
            assert_eq!(failure.kind, ErrorKind::Config);
            assert!(failure.message.starts_with(message), "{failure:?}");
        }
        let session = Session::new(SessionOptions {
            artifacts_env: Some(" ".to_owned()),
            run_id_env: Some(" 42 ".to_owned()),
            ..SessionOptions::default()
        });
        assert!(session.failure().is_none());
        assert_eq!(session.run_id.unwrap().as_str(), "42");
    }

    #[test]
    fn test_plans_carry_the_artifacts_dir_of_the_rule() {
        let (_dir, root) = workspace(&format!("{YAML}artifacts: .gleon/runs/shots\n"), "a.png");
        let golden = root.join("test/goldens/a.png");
        let plan = session(Some("0")).plan(&golden, None, vec![]).unwrap();
        let golden_in = plan.in_workspace.unwrap();
        assert_eq!(
            (golden_in.artifacts.as_str(), golden_in.name.as_str()),
            (".gleon/runs/shots", "test/goldens/a")
        );
        let overridden = Session::new(SessionOptions {
            finds_workspaces: true,
            artifacts_env: Some(".gleon/runs/ram".to_owned()),
            ..SessionOptions::default()
        });
        let plan = overridden.plan(&golden, None, vec![]).unwrap();
        assert_eq!(
            plan.in_workspace.unwrap().artifacts.as_str(),
            ".gleon/runs/ram"
        );
    }

    #[test]
    fn test_unmatched_and_outside_goldens_compare_exact_without_records() {
        let (_dir, root) = workspace(YAML, "a.png");
        fs::write(root.join("lib.png"), b"").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let session = session(None);
        for (golden, has_workspace) in [
            (root.join("lib.png"), true),
            (outside.path().to_path_buf(), false),
        ] {
            let plan = session.plan(&golden, None, vec![]).unwrap();
            assert_eq!(plan.tolerance, Tolerance::Exact {});
            assert_eq!(plan.has_workspace, has_workspace);
            assert!(plan.in_workspace.is_none());
        }
    }

    #[test]
    fn test_without_a_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("a.png");
        fs::write(&golden, b"").unwrap();
        let (_ws, root) = workspace(YAML, "a.png");
        let ignoring = Session::new(SessionOptions::default());
        for (session, golden) in [
            (session(None), golden),
            (ignoring, root.join("test/goldens/a.png")),
        ] {
            let plan = session.plan(&golden, None, vec![pixel_mask(1)]).unwrap();
            assert_eq!((plan.tolerance, plan.masks.len()), (Tolerance::Exact {}, 1));
            assert!(!plan.has_workspace, "{}", golden.display());
        }
    }

    #[test]
    fn test_each_golden_finds_its_own_workspace() {
        let (_one, first) = workspace(YAML, "a.png");
        let (_two, second) = workspace(YAML, "b.png");
        fs::create_dir_all(first.join("test/goldens/deep")).unwrap();
        let session = session(None);
        let record = |golden: PathBuf| {
            session
                .plan(&golden, None, vec![])
                .unwrap()
                .in_workspace
                .unwrap()
        };
        let a = record(first.join("test/goldens/a.png"));
        let deep = record(first.join("test/goldens/deep/c.png"));
        let b = record(second.join("test/goldens/b.png"));
        assert!(
            Arc::ptr_eq(&a.workspace, &deep.workspace),
            "one workspace per root"
        );
        assert_eq!(
            b.workspace.root, second,
            "a second workspace in the same process"
        );
        let missing = record(first.join("test/goldens/new/dir/d.png"));
        assert_eq!(missing.golden_path, "test/goldens/new/dir/d.png");
        assert!(Arc::ptr_eq(&a.workspace, &missing.workspace));
    }

    #[test]
    fn test_config_errors_name_the_config() {
        let (_dir, root) = workspace("required_version: \">=0.1.0\"\nscreenshots: []", "a.png");
        let error = session(None)
            .plan(&root.join("test/goldens/a.png"), None, vec![])
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Config);
        assert!(error.message.starts_with(&format!(
            "gleon: {}: ",
            Workspace::new(root.clone()).config_display()
        )));
        assert!(error.message.contains("at least one rule"), "{error:?}");

        let (_dir, root) = workspace(YAML, "with space.png");
        let error = session(None)
            .plan(&root.join("test/goldens/with space.png"), None, vec![])
            .unwrap_err();
        assert!(
            error.message.contains("not a valid gleon test path"),
            "{error:?}"
        );

        let (_dir, root) = workspace(YAML, "a.png");
        fs::write(root.join(".gleon/gleon.yaml"), [0xff, 0xfe]).unwrap();
        let error = session(None)
            .plan(&root.join("test/goldens/a.png"), None, vec![])
            .unwrap_err();
        assert!(error.message.contains("cannot read it"), "{error:?}");
    }

    #[test]
    fn test_the_config_is_compiled_again_when_it_changes() {
        let (_dir, root) = workspace(YAML, "a.png");
        let session = session(None);
        let golden = root.join("test/goldens/a.png");
        let console = |session: &Session| {
            session
                .plan(&golden, None, vec![])
                .unwrap()
                .recorded()
                .unwrap()
                .1
                .console
        };
        assert!(!console(&session));
        // Same size and, on coarse file systems, the same timestamp: only the content differs.
        fs::write(
            root.join(".gleon/gleon.yaml"),
            YAML.replace("console: false", "console: true "),
        )
        .unwrap();
        assert!(console(&session));
        fs::remove_file(root.join(".gleon/gleon.yaml")).unwrap();
        let error = session.plan(&golden, None, vec![]).unwrap_err();
        assert!(error.message.contains("cannot read it"), "{error:?}");
    }

    #[test]
    fn test_the_missing_workspace_warning_is_given_once() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("a.png");
        let plan_of =
            |session: &Session, golden: &Path| session.plan(golden, None, vec![]).unwrap();
        let requested = session(Some(" true "));
        let plan = plan_of(&requested, &outside);
        assert_eq!(
            requested.missing_workspace_warning(&plan),
            Some(text::MISSING_WORKSPACE_WARNING)
        );
        assert_eq!(requested.missing_workspace_warning(&plan), None);
        for value in [None, Some(""), Some("0"), Some("FALSE")] {
            let session = session(value);
            let plan = plan_of(&session, &outside);
            assert_eq!(session.missing_workspace_warning(&plan), None);
        }
        let (_dir, root) = workspace(YAML, "a.png");
        let session = session(Some("1"));
        let plan = plan_of(&session, &root.join("test/goldens/a.png"));
        assert_eq!(session.missing_workspace_warning(&plan), None);
    }

    #[test]
    fn test_the_gitignore_is_created_once_and_never_replaced() {
        let (_dir, root) = workspace(YAML, "a.png");
        let workspace = Workspace::new(root.clone());
        let gitignore = root.join(".gleon/.gitignore");
        workspace.ensure_gitignore().unwrap();
        assert_eq!(
            fs::read_to_string(&gitignore).unwrap(),
            "blobs/\nruns/\n.env\n.env.local\ncredentials\ndashboard.html\nhistory.json\n"
        );
        fs::write(&gitignore, "mine\n").unwrap();
        workspace.ensure_gitignore().unwrap();
        Workspace::new(root).ensure_gitignore().unwrap();
        assert_eq!(fs::read_to_string(&gitignore).unwrap(), "mine\n");
    }

    #[test]
    fn test_relative_paths() {
        let (_dir, root) = workspace(YAML, "a.png");
        let workspace = Workspace::new(root.clone());
        assert_eq!(
            workspace
                .relative_path(&root.join("test/goldens/a.png"))
                .as_deref(),
            Some("test/goldens/a.png")
        );
        assert_eq!(workspace.relative_path(&root), None, "the root itself");
        assert_eq!(
            workspace.relative_path(&root.join("gone.png")).as_deref(),
            Some("gone.png"),
            "a missing file in an existing directory"
        );
        assert_eq!(
            workspace.relative_path(&root.join("gone/a.png")).as_deref(),
            Some("gone/a.png"),
            "missing directories too"
        );
        assert_eq!(
            workspace.relative_path(Path::new("gone.png")),
            None,
            "a bare name is in the working directory, outside"
        );
    }

    #[test]
    fn test_artifact_patterns() {
        let names = |golden: &str, candidate: &str, diff: &str| ArtifactNames {
            golden: golden.to_owned(),
            candidate: candidate.to_owned(),
            diff: diff.to_owned(),
        };
        assert_eq!(
            names("{name}_g.png", "{name}_t.png", "{name}_d.png").validate(),
            Ok(())
        );
        for (invalid, needle) in [
            (
                names("master.png", "{name}_t.png", "{name}_d.png"),
                "must contain `{name}`",
            ),
            (
                names("../{name}.png", "{name}_t.png", "{name}_d.png"),
                "without `/`",
            ),
            (
                names("{name}.png", "{name}.PNG", "{name}_d.png"),
                "must differ",
            ),
            (
                names("{name}_g.png", "{name}_t.png", "{name}_t.png"),
                "must differ",
            ),
            (
                names("{name}_g.png", "{name}_t.png", "{name}_G.png"),
                "must differ",
            ),
        ] {
            assert!(
                invalid.validate().unwrap_err().contains(needle),
                "{invalid:?}"
            );
        }
    }
}
