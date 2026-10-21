#![cfg(not(miri))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]

//! Which workspace the CLI finds (it stops at a repository root and never takes the home
//! directory), and which `gleon.yaml` `gleon init` writes there.

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;

mod common;

/// The `gleon` binary in `dir` without [`common::GLEON_ENV`], its home directory `home`.
fn gleon(dir: &Path, home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("gleon").unwrap();
    for var in common::GLEON_ENV {
        cmd.env_remove(var);
    }
    cmd.current_dir(dir)
        .env("HOME", home)
        .env("USERPROFILE", home);
    cmd
}

/// A project directory for a test and a home directory next to it, with symbolic links resolved
/// as the CLI resolves them (without Windows' `\\?\` prefix).
fn temp() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = gleon_model::fs::canonical(dir.path()).unwrap();
    let (project, home) = (path.join("project"), path.join("home"));
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&home).unwrap();
    (dir, project, home)
}

/// A generic config for every `.png` in `dir`.
fn config_in(dir: &Path) {
    fs::create_dir_all(dir.join(".gleon")).unwrap();
    fs::write(
        dir.join(".gleon/gleon.yaml"),
        "screenshots:\n  - include: '**/*.png'\n    mode: exact\n",
    )
    .unwrap();
}

/// A real golden of the Flutter integration.
fn golden() -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gleon-ffi/tests/fixtures/counter_initial.png"),
    )
    .unwrap()
}

fn write_golden(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, golden()).unwrap();
}

/// The host as a config names it.
fn host() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A config above a repository is not its workspace: the walk ends at the repository root.
#[test]
fn test_a_config_above_the_repository_is_no_workspace() {
    let (_dir, root, home) = temp();
    config_in(&root);
    let repo = root.join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    write_golden(&repo.join("test/a.png"));

    gleon(&repo, &home)
        .arg("status")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not initialized"));
}

/// A `~/.gleon/gleon.yaml` is no workspace of a scratch project without a repository, and its
/// `.env` sets nothing there.
#[test]
fn test_the_home_directory_is_no_workspace() {
    let (_dir, _project, home) = temp();
    config_in(&home);
    fs::write(home.join(".gleon/.env"), "GLEON_PLATFORM=android-arm\n").unwrap();
    let scratch = home.join("scratch/app");
    write_golden(&scratch.join("test/a.png"));

    gleon(&scratch, &home)
        .arg("status")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not initialized"));
    gleon(&scratch, &home).arg("init").assert().success();
    assert!(
        scratch.join(".gleon/manifests").join(host()).is_dir(),
        "the platform of the home .env does not apply"
    );
    // The project is its own workspace now.
    gleon(&scratch, &home)
        .args(["status", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test/a.png"));
}

/// The walk sees directories resolved, wherever its start comes from: a config named through a
/// symbolic link (or a working directory reached through one) still never makes the home
/// directory a workspace.
#[cfg(unix)]
#[test]
fn test_the_home_directory_is_no_workspace_behind_a_symbolic_link() {
    let (_dir, project, home) = temp();
    // `.gleon/` alone marks the root of a workspace whose config is named explicitly.
    fs::create_dir_all(home.join(".gleon")).unwrap();
    let app = home.join("scratch/app");
    write_golden(&app.join("test/a.png"));
    fs::create_dir_all(app.join("ci")).unwrap();
    fs::write(
        app.join("ci/gleon.yaml"),
        "screenshots:\n  - include: '**/*.png'\n    mode: exact\n",
    )
    .unwrap();
    let link = project.join("link");
    std::os::unix::fs::symlink(&home, &link).unwrap();
    let linked_app = link.join("scratch/app");

    let config = linked_app.join("ci/gleon.yaml");
    gleon(&linked_app, &home)
        .arg("--config")
        .arg(&config)
        .args(["status", "--json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not initialized"));
}

/// Next to a `pubspec.yaml`, `gleon init` writes the Flutter template: the goldens under every
/// `test/` compared exactly, their text on this platform, failure images left out.
#[test]
fn test_init_writes_the_flutter_template_next_to_a_pubspec() {
    let (_dir, root, home) = temp();
    fs::write(root.join("pubspec.yaml"), "name: app\n").unwrap();

    gleon(&root, &home)
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::contains("for a Flutter package"));
    assert_eq!(
        fs::read_to_string(root.join(".gleon/gleon.yaml")).unwrap(),
        format!(
            "required_version: '>={}'\nbaselines: goldens\nfallback_platform: {}\nscreenshots:\n- \
             include: '**/test/**/*.png'\n  mode: exact\nexclude: '**/failures/**'\n",
            env!("CARGO_PKG_VERSION"),
            host()
        )
    );
    // The goldens are the baselines: no blobs, manifests or storage settings (the rule itself is
    // tested in gleon-model, `test_flutter_template`).
    assert!(root.join(".gleon/runs/latest").is_dir());
    assert!(!root.join(".gleon/blobs").exists());
    assert!(!root.join(".gleon/manifests").exists());
    assert!(!root.join(".gleon/.env.template").exists());

    // The commands that scan screenshots point to `gleon test` instead of listing the goldens.
    write_golden(&root.join("test/goldens/a.png"));
    gleon(&root, &home)
        .args(["status", "--json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("gleon test --"));
}

/// `--generic` writes the generic template next to a `pubspec.yaml`, with the tolerance of its
/// mode spelled out; `--flutter` the Flutter one anywhere; both at once are refused.
#[test]
fn test_init_flags_choose_the_template() {
    let (_dir, root, home) = temp();
    let flutter = root.join("flutter");
    fs::create_dir_all(&flutter).unwrap();
    fs::write(flutter.join("pubspec.yaml"), "name: app\n").unwrap();
    gleon(&flutter, &home)
        .args(["init", "--generic"])
        .assert()
        .success();
    let generic = fs::read_to_string(flutter.join(".gleon/gleon.yaml")).unwrap();
    assert!(generic.contains("include: '**/*.png'"), "{generic}");
    assert!(generic.contains("mode: pixel"), "{generic}");
    assert!(generic.contains("threshold: 0.01"), "{generic}");
    assert!(!generic.contains("min_similarity"), "{generic}");
    assert!(!generic.contains("masks"), "{generic}");

    let plain = root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    gleon(&plain, &home)
        .args(["init", "--flutter"])
        .assert()
        .success();
    let template = fs::read_to_string(plain.join(".gleon/gleon.yaml")).unwrap();
    assert!(template.contains("mode: exact"), "{template}");

    gleon(&plain, &home)
        .args(["init", "--flutter", "--generic"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

/// `gleon init` initializes the directory it runs in, like `git init`: in a package of a
/// workspace it starts a nested workspace (whose config applies below it) and says so, leaving
/// the outer config as it is.
#[test]
fn test_init_in_a_package_of_a_workspace_starts_a_nested_one() {
    let (_dir, root, home) = temp();
    fs::create_dir_all(root.join(".git")).unwrap();
    config_in(&root);
    let outer = fs::read_to_string(root.join(".gleon/gleon.yaml")).unwrap();
    let app = root.join("packages/app");
    fs::create_dir_all(&app).unwrap();
    fs::write(app.join("pubspec.yaml"), "name: app\n").unwrap();

    gleon(&app, &home)
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::contains("nested workspace"))
        .stderr(predicate::str::contains(root.display().to_string()));
    let nested = fs::read_to_string(app.join(".gleon/gleon.yaml")).unwrap();
    assert!(nested.contains("mode: exact"), "{nested}");
    assert_eq!(
        fs::read_to_string(root.join(".gleon/gleon.yaml")).unwrap(),
        outer
    );
    // Its own workspace now: initialized again, it nests in nothing.
    gleon(&app, &home)
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::contains("nested").not());
}

/// `gleon init` keeps an existing config, and says so when a template was asked for.
#[test]
fn test_init_keeps_an_existing_config() {
    let (_dir, root, home) = temp();
    fs::write(root.join("pubspec.yaml"), "name: app\n").unwrap();
    gleon(&root, &home).arg("init").assert().success();
    let written = fs::read_to_string(root.join(".gleon/gleon.yaml")).unwrap();

    gleon(&root, &home)
        .args(["init", "--generic"])
        .assert()
        .success()
        .stderr(predicate::str::contains("left as is"));
    assert_eq!(
        fs::read_to_string(root.join(".gleon/gleon.yaml")).unwrap(),
        written
    );
}

/// The Flutter template names the platform of the run (its flags too) as the platform that
/// compares text, under the names a process reports; a platform no process reports is refused.
#[test]
fn test_the_flutter_template_names_the_platform_of_the_run() {
    let (_dir, root, home) = temp();
    for (flags, expected) in [
        (&["--os", "linux", "--arch", "x86_64"][..], "linux-x86_64"),
        (&["--os", "darwin", "--arch", "arm64"][..], "macos-aarch64"),
        (&["--platform", "windows-x64"][..], "windows-x86_64"),
    ] {
        let dir = root.join(expected);
        fs::create_dir_all(&dir).unwrap();
        gleon(&dir, &home)
            .args(flags)
            .args(["init", "--flutter"])
            .assert()
            .success();
        let config = fs::read_to_string(dir.join(".gleon/gleon.yaml")).unwrap();
        assert!(
            config.contains(&format!("fallback_platform: {expected}\n")),
            "{flags:?}: {config}"
        );
        assert!(!dir.join(".gleon/manifests").exists(), "{flags:?}");
    }

    let custom = root.join("custom");
    fs::create_dir_all(&custom).unwrap();
    gleon(&custom, &home)
        .args(["--platform", "my-custom-env", "init", "--flutter"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("my-custom-env"));
    assert!(!custom.join(".gleon").exists(), "nothing created");
}

/// `mode: exact` fails where `pixel` with a threshold of 1% passes: one changed pixel of a real
/// screenshot.
#[test]
fn test_an_exact_rule_fails_on_one_pixel() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gleon-core/tests/fixtures");
    for (config, code) in [("config/shots", 0), ("config/shots-mode-exact", 1)] {
        let (_dir, root, home) = temp();
        gleon(&root, &home)
            .args(["init", "--generic"])
            .assert()
            .success();
        common::copy_config(&root, config);
        let shot = root.join("shots/a.png");
        fs::create_dir_all(shot.parent().unwrap()).unwrap();
        fs::copy(fixtures.join("baseline_gradient_100x100.png"), &shot).unwrap();
        gleon(&root, &home).arg("stage").assert().success();
        fs::copy(fixtures.join("diff_1px_black_center_100x100.png"), &shot).unwrap();
        gleon(&root, &home).arg("diff").assert().code(code);
    }
}
