//! Where the workspace of a path is: the nearest directory above it with `.gleon/gleon.yaml`.
//!
//! The search has a ceiling. A repository is the outermost workspace its files can belong to,
//! so the walk ends with the first directory that is the root of one ([`REPOSITORY_MARKERS`]:
//! `.git`, a directory or the file of a worktree or submodule, and the roots of Mercurial,
//! Jujutsu and Sapling); and the home directory and its parents are no workspace roots unless they
//! are the root of a repository themselves, so a `~/.gleon/gleon.yaml` (a place for user
//! settings, not a workspace) never applies to a scratch project without a repository. The CLI and the integrations resolve workspaces with
//! [`find_root`], so a golden belongs to the same workspace whoever compares it.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// The variable that names the home directory of the user: `USERPROFILE` on Windows, `HOME`
/// elsewhere (what `flutter test` and shells forward).
pub const HOME_VAR: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

/// The entries that make a directory the root of a repository: Git (a directory, or the file of
/// a worktree or submodule), Mercurial, Jujutsu, Sapling.
pub const REPOSITORY_MARKERS: &[&str] = &[".git", ".hg", ".jj", ".sl"];

/// The home directory `named` (the value of [`HOME_VAR`]) as walks see directories: with
/// symbolic links resolved ([`crate::fs::canonical`]), else as named; `None` when empty.
#[must_use]
pub fn home(named: &OsStr) -> Option<PathBuf> {
    if named.is_empty() {
        return None;
    }
    let named = Path::new(named);
    Some(crate::fs::canonical(named).unwrap_or_else(|_| named.to_path_buf()))
}

/// Whether `dir` has a `.gleon/gleon.yaml` file: the root of a workspace.
#[must_use]
pub fn has_config(dir: &Path) -> bool {
    dir.join(".gleon").join("gleon.yaml").is_file()
}

/// Walks up from `start` (inclusive) to the first directory `is_root` accepts.
///
/// The walk stops after a directory that is the root of a repository ([`REPOSITORY_MARKERS`]),
/// and at the home directory or any of its parents (`home`, resolved like `start`, see
/// [`home`]; `None` when unknown): none of them is a root unless it is the root of a repository
/// (CI that sets `HOME` to the checkout keeps its config). `is_root` is given the
/// directory; the caller decides what makes it a workspace ([`has_config`], or a `.gleon/`
/// directory for a config named explicitly).
#[must_use]
pub fn find_root(
    start: &Path,
    home: Option<&Path>,
    is_root: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let is_repository = is_repository(&mut dir);
        // The home directory and its parents hold user settings, not workspaces, unless they are
        // a checkout themselves (CI that points `HOME` at it).
        if !is_repository && home.is_some_and(|home| is_within(home, &dir)) {
            return None;
        }
        // The repository root itself is a workspace root when it has the config.
        if is_root(&dir) {
            return Some(dir);
        }
        if is_repository || !dir.pop() {
            return None;
        }
    }
}

/// Whether `path` is `dir` or lies inside it ([`crate::fs::same_component`]). Only paths of the
/// same kind compare: a relative `dir` (the empty path a relative walk ends with among them,
/// whose components are none) never holds an absolute `path`.
fn is_within(path: &Path, dir: &Path) -> bool {
    if path.is_absolute() != dir.is_absolute() || dir.as_os_str().is_empty() {
        return false;
    }
    let mut path = path.components();
    dir.components().all(|own| {
        path.next()
            .is_some_and(|other| crate::fs::same_component(own, other))
    })
}

/// Whether `dir` is the root of a repository; `dir` is the same afterwards (it is extended in
/// place to probe each marker, so the walk allocates nothing per directory).
fn is_repository(dir: &mut PathBuf) -> bool {
    REPOSITORY_MARKERS.iter().any(|marker| {
        dir.push(marker);
        let exists = dir.exists();
        dir.pop();
        exists
    })
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
    use super::*;

    fn config_in(dir: &Path) {
        std::fs::create_dir_all(dir.join(".gleon")).unwrap();
        std::fs::write(dir.join(".gleon/gleon.yaml"), "screenshots: []\n").unwrap();
    }

    #[test]
    fn test_finds_the_nearest_config_above() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let nested = root.join("test/goldens");
        std::fs::create_dir_all(&nested).unwrap();
        config_in(temp.path());
        config_in(&root);

        assert_eq!(find_root(&nested, None, has_config), Some(root.clone()));
        assert_eq!(find_root(&root, None, has_config), Some(root));
        assert_eq!(
            find_root(temp.path(), None, has_config),
            Some(temp.path().to_path_buf())
        );
    }

    #[test]
    fn test_stops_after_the_repository_root() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let nested = repo.join("packages/app/test");
        std::fs::create_dir_all(&nested).unwrap();
        // A config above the repository (a `~/.gleon/gleon.yaml`-like stray file).
        config_in(temp.path());

        // `.git` as a directory, as a file (a worktree): the walk ends at the repository.
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(find_root(&nested, None, has_config), None);
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: ../.git/worktrees/x\n").unwrap();
        assert_eq!(find_root(&nested, None, has_config), None);

        // The repository root itself is a workspace root when it has the config.
        config_in(&repo);
        assert_eq!(find_root(&nested, None, has_config), Some(repo.clone()));
        // A nested repository (a submodule) does not reach the outer config.
        let inner = nested.join("vendor/lib");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        assert_eq!(find_root(&inner, None, has_config), None);
        // Without a repository the walk goes on upward.
        std::fs::remove_file(repo.join(".git")).unwrap();
        std::fs::remove_dir_all(repo.join(".gleon")).unwrap();
        assert_eq!(
            find_root(&nested, None, has_config),
            Some(temp.path().to_path_buf())
        );
    }

    #[test]
    fn test_home_and_its_parents_are_never_roots() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let scratch = home.join("scratch/app/test");
        std::fs::create_dir_all(&scratch).unwrap();
        config_in(&home);
        config_in(temp.path());

        // A scratch project without `.git` under a home directory with a `.gleon/gleon.yaml`.
        assert_eq!(find_root(&scratch, Some(&home), has_config), None);
        assert_eq!(find_root(&home, Some(&home), has_config), None);
        // A project inside the home directory with its own config is found.
        let project = home.join("scratch/app");
        config_in(&project);
        assert_eq!(
            find_root(&scratch, Some(&home), has_config),
            Some(project.clone())
        );
        // Without a known home, the stray config applies (as before).
        std::fs::remove_dir_all(project.join(".gleon")).unwrap();
        assert_eq!(find_root(&scratch, None, has_config), Some(home));
    }

    /// The parents of the home directory are no roots either, also for a path outside it
    /// (`/Users/Shared/app` never takes `/Users/.gleon/gleon.yaml`).
    #[test]
    fn test_parents_of_home_are_never_roots() {
        let temp = tempfile::tempdir().unwrap();
        let users = temp.path().join("users");
        let home = users.join("me");
        let shared = users.join("shared/app/test");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        config_in(&users);
        config_in(temp.path());

        assert_eq!(find_root(&shared, Some(&home), has_config), None);
        assert_eq!(find_root(&shared, None, has_config), Some(users));
    }

    /// A repository root is a workspace root even when it is the home directory or one of its
    /// parents: CI that sets `HOME` to the checkout (or below it) keeps its config.
    #[test]
    fn test_a_repository_root_is_a_root_even_at_home() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let nested = repo.join("test/goldens");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        config_in(&repo);
        config_in(temp.path());

        for home in [repo.clone(), repo.join(".home")] {
            assert_eq!(
                find_root(&nested, Some(&home), has_config),
                Some(repo.clone()),
                "{}",
                home.display()
            );
        }
        // Without a config the walk still ends there.
        std::fs::remove_dir_all(repo.join(".gleon")).unwrap();
        assert_eq!(find_root(&nested, Some(&repo), has_config), None);
    }

    /// A relative start (an empty directory once walked up) is never taken for the home
    /// directory, which is absolute: `Iterator::all` of no components would say it is.
    #[test]
    fn test_a_relative_start_is_not_the_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        assert!(!is_within(&home, Path::new("")));
        assert!(!is_within(&home, Path::new("a/b")));
        assert!(is_within(&home, temp.path()));
        assert!(is_within(&home, &home));
        assert!(!is_within(&home, &home.join("project")));
        // Walked up to the empty path, the relative start still finds nothing above it.
        assert_eq!(find_root(Path::new("a/b"), Some(&home), |_| false), None);
        assert_eq!(
            find_root(Path::new("a/b"), Some(&home), |dir| dir
                .as_os_str()
                .is_empty()),
            Some(PathBuf::new())
        );
    }

    /// Every version control system ends the walk at its repository root, not only Git.
    #[test]
    fn test_other_repositories_end_the_walk() {
        for marker in [".hg", ".jj", ".sl"] {
            let temp = tempfile::tempdir().unwrap();
            let nested = temp.path().join("repo/test");
            std::fs::create_dir_all(temp.path().join("repo").join(marker)).unwrap();
            std::fs::create_dir_all(&nested).unwrap();
            config_in(temp.path());
            assert_eq!(find_root(&nested, None, has_config), None, "{marker}");
        }
    }

    /// The home directory is resolved like the paths walked; an empty variable names none.
    #[test]
    fn test_home_is_resolved() {
        assert_eq!(home(OsStr::new("")), None);
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        assert_eq!(home(missing.as_os_str()), Some(missing));
        assert_eq!(
            home(temp.path().as_os_str()),
            Some(crate::fs::canonical(temp.path()).unwrap())
        );
    }

    #[test]
    fn test_nothing_found_without_config() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        assert_eq!(find_root(&nested, None, has_config), None);
        assert_eq!(find_root(&nested, None, |_| false), None);
    }
}
