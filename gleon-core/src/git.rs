//! Git context of a run: the branch, the HEAD commit, and untracking paths from the index.

use std::path::Path;

use crate::env::EnvProvider;

/// Errors that can occur during Git operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// Failed to discover or open Git repository
    #[error("Failed to discover Git repository: {0}")]
    Discover(String),

    /// Failed to read Git HEAD reference
    #[error("Failed to read Git HEAD reference: {0}")]
    HeadRead(String),

    /// HEAD is detached and no CI environment branch variables are set
    #[error("HEAD is detached and no CI environment branch variables are set")]
    DetachedHead,

    /// Path is outside the Git repository
    #[error("Path is outside the Git repository: {0}")]
    OutsideRepository(std::path::PathBuf),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolver for Git branch context.
pub struct GitResolver;

impl GitResolver {
    /// Resolves the branch name of a run (what `gleon dashboard` records it under): the first
    /// one named by `cli_branch`, `GLEON_BRANCH` or the branch variables of CI providers in
    /// `env`, else the HEAD of the repository at `base_dir`. The name is only recorded, so any
    /// text passes (an empty one names none).
    ///
    /// # Errors
    /// Returns `GitError::Discover` if no Git repository is found, `GitError::HeadRead`
    /// if HEAD cannot be read, or `GitError::DetachedHead` if HEAD is detached with no CI
    /// branch variables set.
    pub fn resolve_branch(
        cli_branch: Option<&str>,
        base_dir: &Path,
        env: &dyn EnvProvider,
    ) -> Result<String, GitError> {
        let named = |branch: &str| Some(clean_branch_name(branch)).filter(|b| !b.is_empty());
        if let Some(branch) = cli_branch
            .and_then(named)
            .or_else(|| env.get_var("GLEON_BRANCH").as_deref().and_then(named))
            .or_else(|| resolve_ci_branch(env).as_deref().and_then(named))
        {
            return Ok(branch);
        }

        match gix::discover(base_dir) {
            Ok(repo) => {
                match repo.head_name() {
                    Ok(Some(head_name)) => Ok(clean_branch_name(&head_name.shorten().to_string())),
                    Ok(None) => {
                        // Detached HEAD and no env overrides
                        Err(GitError::DetachedHead)
                    }
                    Err(e) => Err(GitError::HeadRead(e.to_string())),
                }
            }
            Err(e) => Err(GitError::Discover(format!(
                "Not a git repository (or no git installed). Please run inside a git repository, or provide the branch manually using the --branch CLI flag or the GLEON_BRANCH environment variable. Underlying error: {e}"
            ))),
        }
    }

    /// Gets the repository's full hexadecimal object ID of HEAD (supporting both SHA-1 and SHA-256 widths).
    ///
    /// # Errors
    /// Returns `GitError::Discover` if no Git repository is found, or `GitError::HeadRead`
    /// if HEAD cannot be read.
    pub fn get_head_commit_sha(base_dir: &Path) -> Result<String, GitError> {
        let repo = gix::discover(base_dir).map_err(|e| GitError::Discover(e.to_string()))?;

        let head_commit = repo
            .head_commit()
            .map_err(|e| GitError::HeadRead(e.to_string()))?;

        Ok(head_commit.id.to_string())
    }

    /// Untracks given relative or absolute paths from the Git index (equivalent to `git rm --cached`).
    /// Returns the list of paths that were actually present in the Git index and removed.
    /// Gracefully returns an empty Vec if `base_dir` is not in a Git repository.
    ///
    /// # Errors
    /// Returns `GitError::Io` if writing the updated index back to disk fails.
    pub fn untrack_from_index<P: AsRef<Path>>(
        base_dir: &Path,
        paths: &[P],
    ) -> Result<Vec<std::path::PathBuf>, GitError> {
        let Ok(repo) = gix::discover(base_dir) else {
            return Ok(vec![]);
        };

        let Some(repo_root) = repo.workdir() else {
            return Ok(vec![]);
        };
        let Ok(repo_root) = normalize_path(repo_root) else {
            return Ok(vec![]);
        };

        let Ok(mut index) = repo.open_index() else {
            return Ok(vec![]);
        };

        let mut seen_indices = std::collections::HashSet::new();
        let mut to_remove = Vec::new();

        for p in paths {
            let raw_path = if p.as_ref().is_absolute() {
                p.as_ref().to_path_buf()
            } else {
                base_dir.join(p.as_ref())
            };
            let abs_path = match normalize_path(&raw_path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!("Failed to normalize path {:?}: {}", raw_path, e);
                    continue;
                }
            };

            if let Ok(rel_to_repo) = abs_path.strip_prefix(&repo_root) {
                // Git index keys are byte-exact: normalize separators only. Folding case here
                // (as `normalize_path_str` does, since it also builds test identities) makes the
                // lookup miss every path containing an uppercase character, leaving the file
                // deleted on disk but still staged.
                // Bound separately so the `to_string_lossy()` temporary outlives the
                // `normalize_path_separators` borrow below (an inline `&rel_to_repo
                // .to_string_lossy()` is dropped at the end of the statement while still
                // borrowed). This still allocates at most once: `rel_str` only owns when the
                // path isn't valid UTF-8, and `norm_str` only owns on top of that when it also
                // contains a backslash.
                let rel_str = rel_to_repo.to_string_lossy();
                let norm_str = crate::naming::normalize_path_separators(&rel_str);
                if let Ok(entry_idx) = index.entry_index_by_path(norm_str.as_bytes().into())
                    && seen_indices.insert(entry_idx)
                {
                    to_remove.push((entry_idx, p.as_ref().to_path_buf()));
                }
            }
        }

        if to_remove.is_empty() {
            return Ok(vec![]);
        }

        // Sort by index descending so removing higher indices does not invalidate or shift lower indices
        to_remove.sort_unstable_by_key(|b| std::cmp::Reverse(b.0));

        let mut untracked = Vec::with_capacity(to_remove.len());
        for (entry_idx, original_path) in to_remove {
            index.remove_entry_at_index(entry_idx);
            untracked.push(original_path);
        }

        index.remove_tree();

        index
            .write(gix::index::write::Options::default())
            .map_err(|e| GitError::Io(std::io::Error::other(e.to_string())))?;

        // Restore original path discovery order
        untracked.reverse();
        tracing::debug!(
            "Successfully untracked {} files from Git index",
            untracked.len()
        );
        Ok(untracked)
    }
}

fn normalize_path(path: &Path) -> Result<std::path::PathBuf, GitError> {
    use std::path::Component;
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(GitError::OutsideRepository(path.to_path_buf()));
                }
            }
            Component::Normal(c) => {
                normalized.push(c);
            }
            Component::CurDir => {}
            other => {
                normalized.push(other.as_os_str());
            }
        }
    }
    Ok(normalized)
}

fn clean_branch_name(name: &str) -> String {
    let mut cleaned = name.trim();
    if let Some(stripped) = cleaned.strip_prefix("refs/heads/") {
        cleaned = stripped;
    }
    cleaned.trim().to_string()
}

fn is_env_truthy(val: Option<&String>) -> bool {
    val.is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
}

fn resolve_ci_branch(env: &dyn EnvProvider) -> Option<String> {
    let get_valid = |k: &str| crate::env::get_trimmed_var(env, k);

    // 1. GitHub Actions
    if is_env_truthy(env.get_var("GITHUB_ACTIONS").as_ref())
        && let Some(b) = get_valid("GITHUB_HEAD_REF").or_else(|| get_valid("GITHUB_REF_NAME"))
    {
        return Some(b);
    }

    // 2. GitLab CI
    if env.get_var("GITLAB_CI").is_some()
        && let Some(b) = get_valid("CI_MERGE_REQUEST_SOURCE_BRANCH_NAME")
            .or_else(|| get_valid("CI_COMMIT_BRANCH"))
            .or_else(|| get_valid("CI_COMMIT_REF_NAME"))
    {
        return Some(b);
    }

    // 3. CircleCI
    if is_env_truthy(env.get_var("CIRCLECI").as_ref())
        && let Some(b) = get_valid("CIRCLE_BRANCH")
    {
        return Some(b);
    }

    // 4. Bitbucket Pipelines
    if env.get_var("BITBUCKET_COMMIT").is_some()
        && let Some(b) = get_valid("BITBUCKET_BRANCH")
    {
        return Some(b);
    }

    // 5. Azure DevOps
    if is_env_truthy(env.get_var("TF_BUILD").as_ref())
        && let Some(b) = get_valid("BUILD_SOURCEBRANCHNAME")
    {
        return Some(b);
    }

    // 6. Travis CI
    if is_env_truthy(env.get_var("TRAVIS").as_ref())
        && let Some(b) = get_valid("TRAVIS_BRANCH")
    {
        return Some(b);
    }

    // 7. Codemagic
    if env.get_var("CM_BUILD_ID").is_some()
        && let Some(b) = get_valid("CM_BRANCH")
    {
        return Some(b);
    }

    // 8. Bitrise
    if is_env_truthy(env.get_var("BITRISE_IO").as_ref())
        && let Some(b) = get_valid("BITRISE_GIT_BRANCH")
    {
        return Some(b);
    }

    // 9. Generic CI Fallback list
    if env.get_var("CI").is_some() {
        let ci_vars = [
            "GITHUB_HEAD_REF",
            "GITHUB_REF_NAME",
            "CI_COMMIT_BRANCH",
            "CI_COMMIT_REF_NAME",
            "CIRCLE_BRANCH",
            "BITBUCKET_BRANCH",
            "BUILD_SOURCEBRANCHNAME",
            "TRAVIS_BRANCH",
            "CM_BRANCH",
            "BITRISE_GIT_BRANCH",
        ];
        for var in ci_vars {
            if let Some(b) = get_valid(var) {
                return Some(b);
            }
        }
    }

    None
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
    use std::collections::HashMap;

    use tempfile::tempdir;

    use super::*;

    struct MockEnv {
        vars: HashMap<String, String>,
    }

    impl EnvProvider for MockEnv {
        fn get_var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
    }

    fn create_mock_git_repo(path: &Path, head_content: &str) {
        let git_dir = path.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir_all(git_dir.join("refs")).unwrap();
        std::fs::write(git_dir.join("HEAD"), head_content).unwrap();
    }

    #[test]
    fn test_resolve_branch_cli_override() {
        let dir = tempdir().unwrap();
        let env = MockEnv {
            vars: HashMap::new(),
        };
        let result =
            GitResolver::resolve_branch(Some("refs/heads/cli-branch\r\n"), dir.path(), &env);
        assert_eq!(result.unwrap(), "cli-branch");
    }

    #[test]
    fn test_resolve_branch_env_override() {
        let dir = tempdir().unwrap();
        let mut vars = HashMap::new();
        vars.insert(
            "GLEON_BRANCH".to_string(),
            "refs/heads/env-branch\r\n".to_string(),
        );
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "env-branch");
    }

    #[test]
    fn test_resolve_branch_github_actions_pr() {
        let dir = tempdir().unwrap();
        let mut vars = HashMap::new();
        vars.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
        vars.insert("GITHUB_HEAD_REF".to_string(), "feature-pr".to_string());
        vars.insert("GITHUB_REF_NAME".to_string(), "main".to_string());
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "feature-pr");
    }

    #[test]
    fn test_resolve_branch_github_actions_push() {
        let dir = tempdir().unwrap();
        let mut vars = HashMap::new();
        vars.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
        vars.insert("GITHUB_REF_NAME".to_string(), "feature-push".to_string());
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "feature-push");
    }

    #[test]
    fn test_resolve_branch_gitlab_ci_mr() {
        let dir = tempdir().unwrap();
        let mut vars = HashMap::new();
        vars.insert("GITLAB_CI".to_string(), "true".to_string());
        vars.insert(
            "CI_MERGE_REQUEST_SOURCE_BRANCH_NAME".to_string(),
            "gitlab-mr".to_string(),
        );
        vars.insert("CI_COMMIT_BRANCH".to_string(), "main".to_string());
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "gitlab-mr");
    }

    #[test]
    fn test_resolve_branch_gitlab_ci_branch() {
        let dir = tempdir().unwrap();
        let mut vars = HashMap::new();
        vars.insert("GITLAB_CI".to_string(), "true".to_string());
        vars.insert("CI_COMMIT_BRANCH".to_string(), "gitlab-branch".to_string());
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "gitlab-branch");
    }

    #[test]
    fn test_resolve_branch_gix_success() {
        let dir = tempdir().unwrap();
        create_mock_git_repo(dir.path(), "ref: refs/heads/gix-branch\r\n");
        let env = MockEnv {
            vars: HashMap::new(),
        };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "gix-branch");
    }

    #[test]
    fn test_resolve_branch_gix_detached_head() {
        let dir = tempdir().unwrap();
        create_mock_git_repo(dir.path(), "0123456789abcdef0123456789abcdef01234567\n");
        let env = MockEnv {
            vars: HashMap::new(),
        };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert!(matches!(result, Err(GitError::DetachedHead)));
    }

    #[test]
    fn test_resolve_branch_no_git_fallback() {
        let dir = tempdir().unwrap();
        let env = MockEnv {
            vars: HashMap::new(),
        };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert!(matches!(result, Err(GitError::Discover(_))));
    }

    #[test]
    fn test_resolve_branch_generic_ci_fallback() {
        let dir = tempdir().unwrap();
        let mut vars = HashMap::new();
        vars.insert("CI".to_string(), "true".to_string());
        vars.insert(
            "GITHUB_REF_NAME".to_string(),
            "generic-ci-branch".to_string(),
        );
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "generic-ci-branch");
    }

    #[test]
    fn test_resolve_branch_generic_ci_no_ci_env() {
        let dir = tempdir().unwrap();
        let mut vars = HashMap::new();
        vars.insert(
            "GITHUB_REF_NAME".to_string(),
            "generic-ci-branch".to_string(),
        );
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert!(matches!(result, Err(GitError::Discover(_))));
    }

    /// The branch is only recorded (`gleon dashboard`): a name Git would refuse passes as given,
    /// and an empty one names none, so the next source decides.
    #[test]
    fn test_any_branch_name_is_recorded_and_an_empty_one_names_none() {
        let dir = tempdir().unwrap();
        let env = MockEnv {
            vars: HashMap::from([("GLEON_BRANCH".to_string(), "from env".to_string())]),
        };
        for (given, expected) in [
            (Some("feature space branch"), "feature space branch"),
            (Some("foo..bar"), "foo..bar"),
            (Some(" refs/heads/ "), "from env"),
            (None, "from env"),
        ] {
            assert_eq!(
                GitResolver::resolve_branch(given, dir.path(), &env).unwrap(),
                expected,
                "{given:?}"
            );
        }
    }

    #[test]
    fn test_is_env_truthy() {
        assert!(is_env_truthy(Some(&"true".to_string())));
        assert!(is_env_truthy(Some(&"True".to_string())));
        assert!(is_env_truthy(Some(&"TRUE".to_string())));
        assert!(is_env_truthy(Some(&"1".to_string())));
        assert!(!is_env_truthy(Some(&"false".to_string())));
        assert!(!is_env_truthy(Some(&"0".to_string())));
        assert!(!is_env_truthy(None));
    }

    #[test]
    fn test_get_head_commit_sha_no_git() {
        let dir = tempdir().unwrap();
        let result = GitResolver::get_head_commit_sha(dir.path());
        assert!(matches!(result, Err(GitError::Discover(_))));
    }

    #[test]
    #[cfg(not(miri))]
    fn test_get_head_commit_sha_real_repo() {
        let dir = tempdir().expect("tempdir creation should succeed");
        gix::init(dir.path()).expect("gix init should succeed");
        std::fs::write(
            dir.path().join(".git/config"),
            "[user]\n\tname = gleon Test\n\temail = test@gleon.rs\n",
        )
        .expect("write .git/config should succeed");
        let repo = gix::open(dir.path()).expect("gix open should succeed");
        let empty_tree_id = repo.empty_tree().id();
        let commit_id = repo
            .commit(
                "HEAD",
                "Initial commit",
                empty_tree_id,
                gix::commit::NO_PARENT_IDS,
            )
            .expect("commit should succeed");

        let sha = GitResolver::get_head_commit_sha(dir.path())
            .expect("get_head_commit_sha should succeed");
        assert_eq!(sha, commit_id.to_string());
    }

    #[test]
    fn test_resolve_branch_ci_providers() {
        let dir = tempdir().unwrap();

        // CircleCI
        let mut vars = HashMap::new();
        vars.insert("CIRCLECI".to_string(), "true".to_string());
        vars.insert("CIRCLE_BRANCH".to_string(), "circle-branch".to_string());
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "circle-branch");

        // Bitbucket
        let mut vars = HashMap::new();
        vars.insert("BITBUCKET_COMMIT".to_string(), "123".to_string());
        vars.insert(
            "BITBUCKET_BRANCH".to_string(),
            "bitbucket-branch".to_string(),
        );
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "bitbucket-branch");

        // Azure DevOps
        let mut vars = HashMap::new();
        vars.insert("TF_BUILD".to_string(), "True".to_string());
        vars.insert(
            "BUILD_SOURCEBRANCHNAME".to_string(),
            "azure-branch".to_string(),
        );
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "azure-branch");

        // Travis
        let mut vars = HashMap::new();
        vars.insert("TRAVIS".to_string(), "true".to_string());
        vars.insert("TRAVIS_BRANCH".to_string(), "travis-branch".to_string());
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "travis-branch");

        // Codemagic
        let mut vars = HashMap::new();
        vars.insert("CM_BUILD_ID".to_string(), "123".to_string());
        vars.insert("CM_BRANCH".to_string(), "codemagic-branch".to_string());
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "codemagic-branch");

        // Bitrise
        let mut vars = HashMap::new();
        vars.insert("BITRISE_IO".to_string(), "true".to_string());
        vars.insert(
            "BITRISE_GIT_BRANCH".to_string(),
            "bitrise-branch".to_string(),
        );
        let env = MockEnv { vars };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert_eq!(result.unwrap(), "bitrise-branch");
    }

    #[test]
    fn test_clean_branch_name_helper() {
        assert_eq!(
            clean_branch_name("  refs/heads/feature/branch  "),
            "feature/branch"
        );
        assert_eq!(clean_branch_name("  feature-branch  "), "feature-branch");
    }

    #[test]
    fn test_resolve_branch_gix_corrupt_head() {
        let dir = tempdir().unwrap();
        // Create a git repo where HEAD contains invalid data to cause HeadRead error
        create_mock_git_repo(dir.path(), "invalid_data_no_ref_format");
        let env = MockEnv {
            vars: HashMap::new(),
        };
        let result = GitResolver::resolve_branch(None, dir.path(), &env);
        assert!(result.is_err());
    }

    #[test]
    fn test_normalize_path_cur_dir() {
        let path = Path::new("./file.png");
        let norm = normalize_path(path).unwrap();
        assert_eq!(norm, Path::new("file.png"));
    }

    #[test]
    fn test_untrack_from_index_unit() {
        use std::path::PathBuf;
        let dir = tempdir().unwrap();
        let base_path = dir.path();

        // 1. Outside git repository
        let untracked =
            GitResolver::untrack_from_index(base_path, &[PathBuf::from("foo.png")]).unwrap();
        assert!(untracked.is_empty());

        // 2. Inside initialized git repository
        let _repo = gix::init(base_path).unwrap();

        // Populate index with test entries
        let index_path = base_path.join(".git").join("index");
        let state = gix::index::State::new(gix::hash::Kind::Sha1);
        let mut index = gix::index::File::from_state(state, index_path);
        let rel_path1 = "test/a.png";
        let rel_path2 = "test/b.png";
        index.dangerously_push_entry(
            gix::index::entry::Stat::default(),
            gix::hash::ObjectId::empty_tree(gix::hash::Kind::Sha1),
            gix::index::entry::Flags::empty(),
            gix::index::entry::Mode::FILE,
            rel_path1.as_bytes().into(),
        );
        index.dangerously_push_entry(
            gix::index::entry::Stat::default(),
            gix::hash::ObjectId::empty_tree(gix::hash::Kind::Sha1),
            gix::index::entry::Flags::empty(),
            gix::index::entry::Mode::FILE,
            rel_path2.as_bytes().into(),
        );
        index.write(gix::index::write::Options::default()).unwrap();

        // Untrack with duplicates and non-existent files
        let paths_to_untrack = vec![
            PathBuf::from("test/a.png"),
            base_path.join("test/a.png"), // Absolute duplicate
            PathBuf::from("test/b.png"),
            PathBuf::from("nonexistent.png"),
        ];

        let untracked = GitResolver::untrack_from_index(base_path, &paths_to_untrack).unwrap();
        assert_eq!(untracked.len(), 2);
        assert!(untracked.contains(&PathBuf::from("test/a.png")));
        assert!(untracked.contains(&PathBuf::from("test/b.png")));

        // 3. In bare repository (no workdir)
        let bare_dir = tempdir().unwrap();
        gix::init_bare(bare_dir.path()).unwrap();
        let bare_untracked =
            GitResolver::untrack_from_index(bare_dir.path(), &[PathBuf::from("test/a.png")])
                .unwrap();
        assert!(bare_untracked.is_empty());

        // 4. Invalid normalization path and path outside repository
        let invalid_paths = vec![
            PathBuf::from("../../../outside.png"),
            PathBuf::from("/nonexistent/absolute/outside.png"),
        ];
        let invalid_untracked = GitResolver::untrack_from_index(base_path, &invalid_paths).unwrap();
        assert!(invalid_untracked.is_empty());

        // 5. Test normalize_path error with root escape
        let root_escape = normalize_path(Path::new("/.."));
        assert!(root_escape.is_err());
        let rel_escape = normalize_path(Path::new("a/../../b"));
        assert!(rel_escape.is_err());
    }
}
