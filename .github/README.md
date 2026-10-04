[![codecov](https://codecov.io/gh/gleon-rs/gleon/graph/badge.svg?token=KIUODCEVAK)](https://codecov.io/gh/gleon-rs/gleon)

# gleon

⛵ `gleon` is a high-performance, developer-first, framework-agnostic visual regression testing CLI built in Rust. It isolates screenshot baselines by platform and Git branch and uses a content-addressed storage (CAS) model for baseline artifacts, minimizing bandwidth and storage overhead in CI pipelines.

---

## ⚡ Quick Start for New Projects

Follow these 5 steps to add visual regression testing to any codebase (Flutter, Web, iOS, Android, etc.):

### 1. Install the CLI

Ensure you have [rustup](https://rustup.rs) installed. The toolchain is pinned to Rust 1.100 nightly (`nightly-2026-09-25`, Edition 2024) in `rust-toolchain.toml`, and rustup installs it automatically when you build from the repository root. Then install `gleon`:

```bash
cargo install --path gleon --bin gleon --force
```

### 2. Initialize your Project

In the root of your repository, run:

```bash
gleon init
```

This creates the `.gleon/` workspace scaffold:

- `.gleon/gleon.yaml`: Workspace configuration file (always inside `.gleon/`, not the repository root).
- `.gleon/.gitignore`: Automatically ignores large binary blobs (`blobs/`) and run outputs (`runs/`).
- `.gleon/.env.template`: Storage credentials template.
- `.gleon/manifests/`: Directory where lightweight, deterministic JSON baseline manifests will be stored in Git.

### 3. Configure `.gleon/gleon.yaml`

Edit `.gleon/gleon.yaml` to point to where your test framework outputs golden screenshots. For example:

```yaml
required_version: ">=0.1.0"

screenshots:
  - include: "test/**/goldens/**/*.png"
    mode: pixel
    diff:
      threshold: 0.1
      anti_alias: true

exclude:
  - "**/build/**"
  - "**/target/**"
  - "**/node_modules/**"
```

### 4. Record Initial Baselines (`stage`)

Generate golden screenshots using your existing test suite (e.g. `flutter test` or `npm test`), then record them as your baseline:

```bash
gleon stage
```

`gleon stage` computes cryptographic hashes, copies the image files into local content-addressable storage, and writes deterministic JSON manifests into `.gleon/manifests/<platform>/` (`<os>-<arch>`, e.g. `macos-aarch64`, then `+<renderer>` and `+<key>=<value>` per label when set).

Commit these manifest files to Git:

```bash
git add .gleon/gleon.yaml .gleon/.gitignore .gleon/manifests/
git commit -m "chore: record initial visual regression baselines"
```

### 5. Verify & Inspect Diffs (`diff` & `report`)

When you or your team make changes and re-run your visual tests:

```bash
# Check test status (Clean, Added, Modified, Deleted)
gleon status

# Run pixel/SSIM comparison against committed baselines (one case report per screenshot)
gleon diff

# View visual diff report in browser
gleon report html --out report.html
```

Integrations such as the gleon Flutter package compare in their own test runner and write the same
case reports; run the tests through `gleon test` so they form one run, then use the same commands:

```bash
gleon test -- flutter test
gleon report markdown
gleon dashboard
```

---

## 🛠️ CLI Command Reference

| Command                    | Description                                                                                 | Example                                                                         |
| :------------------------- | :------------------------------------------------------------------------------------------ | :------------------------------------------------------------------------------ |
| `gleon init`               | Scaffolds the `.gleon/` directory tree and default `.gleon/gleon.yaml`.                     | `gleon init`                                                                    |
| `gleon stage [PATHS...]`   | Records matching screenshots as official baseline manifests for the current platform.       | `gleon stage`<br>`gleon stage test/goldens/login.png`                           |
| `gleon status`             | Reports the status (`Clean`, `Added`, `Modified`, `Deleted`) of all discovered screenshots. | `gleon status`<br>`gleon status --json`                                         |
| `gleon diff`               | Runs visual comparison between actual screenshots and committed baselines.                  | `gleon diff`<br>`gleon diff --artifacts .gleon/runs/ci`                         |
| `gleon test -- <COMMAND>`  | Runs a test command (e.g. `flutter test`) as one run with metrics on.                       | `gleon test -- flutter test`<br>`gleon test -- npm test`                        |
| `gleon report <FORMAT>`    | Renders the case reports of the latest run (`html`, `markdown`, `junit`, `json`).           | `gleon report html --out report.html`<br>`gleon report markdown --pr-number 42` |
| `gleon dashboard`          | Adds the latest run to `history.json` and compiles the static history dashboard.            | `gleon dashboard`<br>`gleon dashboard --push`                                   |
| `gleon approve [NAMES...]` | Accepts the candidates of failed cases as new baselines.                                    | `gleon approve`<br>`gleon approve auth/login`                                   |
| `gleon pull`               | Downloads missing baseline blobs from remote object storage to local storage.               | `gleon pull`<br>`gleon pull --all`                                              |
| `gleon push`               | Uploads locally staged baseline blobs to remote object storage.                             | `gleon push`                                                                    |
| `gleon clean`              | Removes `.gleon/runs/`; `--screenshots` also deletes, untracks and ignores screenshots.     | `gleon clean`<br>`gleon clean --screenshots --dry-run`                          |
| `gleon lint`               | Verifies integrity and schema compliance of all manifests and configs.                      | `gleon lint`                                                                    |
| `gleon resolve`            | Interactively or automatically resolves Git merge conflicts in baseline manifests.          | `gleon resolve`                                                                 |

### Global Flags

All commands support the following global options:

- `--config <PATH>`: Specify an explicit path to the configuration file (default: `.gleon/gleon.yaml`, searched upwards from the current directory).
- `--target-branch <BRANCH>`: Target branch for baseline comparison (defaults to `main`, or `GLEON_TARGET_BRANCH`).
- `--platform <STRING>`: Override platform context with an opaque string (e.g. `--platform my-custom-env`).
- `--os <OS>` / `--arch <ARCH>` / `--renderer <RENDERER>`: Override individual platform context dimensions.
- `--label <KEY=VALUE>`: Add custom isolation labels (e.g. `--label theme=dark`).
- `--verbose`: Enable debug logging output (routed to `stderr`).
- `--quiet`: Suppress informational output (only display warnings and errors).

---

## ⚙️ Configuration Reference (`.gleon/gleon.yaml`)

Below is a complete, annotated `.gleon/gleon.yaml` reference. Unknown keys are rejected. A JSON Schema is committed at [`gleon-model/schema/config.v1.json`](../gleon-model/schema/config.v1.json).

```yaml
# Enforce minimum CLI version across the team and CI
required_version: ">=0.1.0"

# Rules for discovering and comparing screenshots. A file is excluded if it matches `exclude`;
# otherwise the FIRST rule whose `include` matches applies (paths are matched case-insensitively,
# relative to the workspace root, with `/` separators).
screenshots:
  - include: "test/**/goldens/**/*.png" # Single pattern or list of glob patterns
    mode: pixel # 'pixel' (exact per-pixel compare) or 'ssim' (tolerates rendering noise, see below)
    diff:
      threshold: 0.1 # 'pixel': allowed fraction of differing pixels [0.0 - 1.0] (default: 0.1)
      anti_alias: true # Reserved, currently has no effect; use mode 'ssim' to tolerate anti-aliasing
      min_similarity: 0.8 # 'ssim': minimum local SSIM of every neighborhood [0.0 - 1.0] (default: 0.8)
      color_tolerance: 8 # 'ssim': tolerated deviation beyond the local 3x3 envelope, 8-bit units (default: 8)
    # Optional, 'pixel' only: integrations that report the text of a screenshot (the Flutter package)
    # compare it under this tolerance and everything else strictly: the text passes while every
    # 16x16 square of it has at most this share of differing pixels [0.0 - 1.0]. Unset, the default
    # depends on the golden: 0.05 against a golden of the platform the test runs on (see
    # `fallback_platform`), else 1 (text never fails, because operating systems rasterize glyphs
    # differently, about 40% of a square, more than a changed character does). A value set here
    # always applies: 1 turns text comparison off everywhere. `gleon diff` sees no text.
    text_tolerance: 1.0
    masks:
      # Optional: Ignore dynamic regions (clocks, avatars, blinking cursors)
      - path: "**/dashboard.png"
        zones:
          - x: 10
            y: 20
            width: 150 # Absolute pixels (150) or relative percentage ("25%")
            height: 40

# Global directory exclusion patterns
exclude:
  - "**/build/**"
  - "**/target/**"
  - "**/node_modules/**"

# Optional: Fallback platform for Sparse Multi-Platform Baselines
# When secondary platforms render identically to the fallback, no duplicate manifests are stored.
# For integrations with golden files (Flutter) it is the platform of the shared goldens
# `<dir>/<file>` (OS and architecture, with the names a process reports: `macos-aarch64`, not
# `macos-arm64`): there text is compared by default, every other platform keeps its own goldens
# in `<dir>/<os>-<arch>/<file>` (e.g. `linux-x86_64`).
fallback_platform:
  os: macos
  arch: aarch64

# Optional: per-golden comparison metrics, recorded by integrations such as the gleon Flutter
# package into `.gleon/runs/latest/cases/<test name>.json` (git-ignored with `runs/`).
# The GLEON_METRICS environment variable (1/true or 0/false) overrides `enabled`.
metrics:
  enabled: false # default: false
  console: true # also print one line per golden (default: true)

# Optional: where the images of failed cases go (golden, candidate, diff), relative to the
# workspace root: this default or a directory under `.gleon/runs/` outside `latest/`.
# GLEON_ARTIFACTS_DIR and `gleon diff --artifacts` override it.
artifacts: .gleon/runs/latest/artifacts
```

Remote blob storage (AWS S3, Cloudflare R2, Google Cloud Storage) is configured through environment variables, not in `gleon.yaml`, so credentials never end up in Git: copy `.gleon/.env.template` to `.gleon/.env.local` and set `GLEON_STORAGE_URL` (e.g. `s3://my-visual-baselines-bucket/blobs`) plus the provider credentials (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, ...).

### Case reports (`metrics`)

Case reports are the one result format of gleon (schema: [`gleon-model/schema/case.v2.json`](../gleon-model/schema/case.v2.json)): `gleon diff` writes one per screenshot, integrations one per golden with metrics enabled. A report holds the golden and candidate SHA-256 and size, the effective tolerance and masks, the outcome (`identical`, `match`, `mismatch`, `dimension_mismatch`, `error` with its kind, `updated`, `missing`), the metrics including their headroom to the thresholds (for example `min_ssim - min_similarity` and `color_tolerance - peak_excess` in SSIM mode) and the paths of the images a failure keeps in the artifacts directory. Passing goldens report their margin too, so thresholds can be tuned from measurements instead of guesses.

`gleon report`, `gleon dashboard` and `gleon approve` read the reports of one run: the run of `GLEON_RUN_ID` (set it in CI, e.g. `${{ github.run_id }}-${{ github.run_attempt }}`), else the run `gleon test` recorded in `.gleon/runs/latest/run.json`, else the run of the newest report (`gleon diff` names its own run when `GLEON_RUN_ID` is not set). Reports without a run id (tests run without `gleon test`) are read together, without those whose golden no longer exists, and with a warning that they may mix runs; without metrics integrations record only failures, so such a run counts no passes: run the tests through `gleon test` for complete totals. Only the process environment sets `GLEON_RUN_ID`, never `.gleon/.env`. Invalid reports are skipped with a warning. `--from <dir>` reads a downloaded copy of `.gleon/runs/latest` (relative to the working directory, with its `cases/`) as it is, ignoring `GLEON_RUN_ID`; `gleon approve` takes several (`--from linux/latest --from macos/latest`). `golden.path` is the golden file of an integration, and for `gleon diff` the screenshot whose baseline lives in the manifests. Approving turns cases of `gleon diff` into manifests and blobs of the platform the case ran on, and overwrites the golden PNG of an integration (only `.png` files inside the workspace, outside hidden directories and not through symlinks); every candidate is checked before anything is written, and two different candidates for one golden are refused. `.gleon/history.json` keeps the failures of each run and counts the passes.

#### Integrations in CI (e.g. Flutter)

Tests of an integration write their case reports themselves, so there is no `gleon diff` step: give the job a run id, run the tests through `gleon test`, then report that run and upload `.gleon/runs/` for `/gleon approve` (the artifact name must start with `gleon-artifacts-<PR number>-`):

```yaml
env:
  GLEON_RUN_ID: ${{ github.run_id }}-${{ github.run_attempt }}
steps:
  - run: gleon test -- flutter test
  - if: failure() && github.event_name == 'pull_request'
    run: gleon report markdown --pr-number ${{ github.event.pull_request.number }} > gleon-report.md
  - if: failure() && github.event_name == 'pull_request'
    uses: actions/upload-artifact@v7
    with:
      name: gleon-artifacts-${{ github.event.pull_request.number }}-${{ runner.os }}
      path: .gleon/runs/
```

On Windows `gleon test -- flutter test` finds `flutter.bat` like a shell does.

#### Per-platform goldens of integrations

Integrations commit goldens as PNG files, one per test, recorded on one platform: name it with
`fallback_platform`, with the names a process reports (`std::env::consts`: `macos-aarch64`,
`linux-x86_64`, `windows-x86_64`; `macos-arm64` or `darwin` are config errors, since they would
never match). On that platform the shared goldens `<dir>/<file>` are its own, so text is compared
by default (`text_tolerance` 0.05: a pixel of font-engine noise per 16x16 square passes, a
changed digit fails). Every other platform has its own goldens in `<dir>/<os>-<arch>/<file>`
(`linux-x86_64`, `linux-aarch64`, `windows-x86_64`, `macos-x86_64`): `--update-goldens` writes
them there, and until one exists the shared golden is compared with text under the default 1
(text ignored, layout exact). A `text_tolerance` set in the rule or the call applies on every
platform, so `1` turns text comparison off on the goldens' platform too.

A case compared with the shared golden names the own golden in `golden.path` and the compared
one in `golden.fallback`; with metrics on, a pass that differs from it keeps its candidate.
Approving the runs of a CI matrix therefore records one golden per platform, from failures and
passes alike (a pass without differences is copied from the shared golden), without conflicts; a
plain `gleon approve` does so for every such case, and a path filter may name the shared golden
the test printed. Only goldens a rule matches follow this layout: an excluded or unmatched golden
stays one file for every platform.

```bash
gleon approve --from metrics-linux-x64 --from metrics-windows-x64
gleon approve --from metrics-linux-x64 test/goldens/a.png
```

The platform is the test process's: an x86_64 Flutter under Rosetta on Apple silicon runs as
`macos-x86_64` and keeps its own goldens there.

Without `fallback_platform` every platform compares the shared goldens with text under
`text_tolerance` (default 1). The case `name` and rule matching use the shared golden on every
platform.

---

## 🚀 CI/CD Integration (GitHub Actions)

`gleon` provides a composite GitHub Action (`gleon-rs/gleon`) for turnkey CI verification.

### CI/CD Prerequisites (Shallow Clone Constraint)

> [!IMPORTANT]
> `gleon` computes baseline manifests by resolving the `merge-base` commit between the pull request branch and the target branch (`main`).
> Default CI checkout actions (`actions/checkout`) perform a **shallow clone** (`fetch-depth: 1`), which lacks commit ancestry.
>
> **You must configure `actions/checkout` with `fetch-depth: 0`:**

```yaml
- name: Checkout repository
  uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
  with:
    fetch-depth: 0 # Required for gleon merge-base resolution
```

### Pull Request Verification Workflow

Add `.github/workflows/visual-tests.yml` to your repository:

```yaml
name: Visual Regression Tests

on:
  pull_request:
    branches: [main]

jobs:
  verify:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      pull-requests: write # Required to post diff reports as PR comments
    steps:
      - name: Checkout code
        uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          fetch-depth: 0

      - name: Run Test Suite (generate actual screenshots)
        run: npm test # or flutter test, cargo test, etc.

      - name: Run gleon Visual Regression Verify
        uses: gleon-rs/gleon@main
        with:
          command: "verify"
          github-token: ${{ secrets.GITHUB_TOKEN }}
```

### Action Inputs

| Input               | Description                                                                                           | Default                       |
| :------------------ | :---------------------------------------------------------------------------------------------------- | :---------------------------- |
| `version`           | Release version tag to download (e.g. `'v0.2.2'` or `'latest'`)                                       | `'latest'`                    |
| `command`           | Execution mode: `'verify'` (pull + diff + report) or single command (`'diff'`, `'pull'`, `'approve'`) | `'diff'`                      |
| `github-token`      | Token (`${{ secrets.GITHUB_TOKEN }}`) for release downloads and PR comments                           | `${{ github.token }}`         |
| `target-branch`     | Target branch for baseline comparison                                                                 | PR base ref or default branch |
| `working-directory` | Working directory to run gleon from (useful for monorepos)                                            | `'.'`                         |
| `args`              | Additional flags for the selected command                                                             | `''`                          |
| `license-key`       | Commercial BSL license key for private enterprise repositories                                        | `''`                          |
| `strict`            | Fail build immediately on license violation (`'true'` / `'false'`)                                    | `'false'`                     |

---

## 📸 Approving Visual Baseline Changes in Pull Requests

When `gleon` detects visual regressions during a PR CI run, it automatically posts a detailed Markdown report with diff previews in the PR comment section.

To accept the new visual changes as the updated baseline:

1. **Approve All Changed Screenshots**:
   Comment directly on the PR:

   ```text
   /gleon approve
   ```

2. **Approve Specific Tests Only**:

   ```text
   /gleon approve auth/login
   ```

### Enabling `/gleon approve` Comments

To enable comment-based approvals, add `.github/workflows/gleon-approve.yml` referencing the reusable workflow:

```yaml
name: Gleon Approve

on:
  issue_comment:
    types: [created]

jobs:
  approve:
    permissions:
      actions: write
      contents: write
      pull-requests: read
    uses: gleon-rs/gleon/.github/workflows/approve.yml@main
    with:
      trigger-workflow: "visual-tests.yml" # Optional: auto-rerun CI after baseline approval
    secrets: inherit
```

---

## 🏗️ Architecture & FAQ

### Why does gleon enforce `.gitignore` for baseline images?

`gleon` separates the **control plane** (manifests) from the **data plane** (images) using a **Content-Addressable Storage (CAS)** architecture.

Committing binary blobs directly to Git causes repository bloat, slow clone times, and unmanageable PR diffs. `gleon` solves this:

- **Manifests in Git:** Tiny, deterministic JSON files (`.gleon/manifests/**/*.json`) containing cryptographic digests (SHA-256) and spatial dimensions.
- **Blobs in Object Storage:** Actual PNG images (`.gleon/blobs/`) are ignored by Git. They are uploaded to S3-compatible object storage via `gleon push` and downloaded on demand via `gleon pull`.

### How do I handle cross-platform rendering diffs?

Different operating systems (macOS vs Ubuntu CI) render fonts and anti-aliasing differently. **Never inflate global error thresholds to mask these differences!**

Instead, use **Sparse Multi-Platform Baselines with Fallback**:

1. Configure `fallback_platform` in `.gleon/gleon.yaml` (e.g. `os: macos`, `arch: aarch64`).
2. Tests that render identically across platforms dynamically inherit the fallback baseline in memory.
3. Only genuine platform-specific differences generate override manifests when approved (`/gleon approve`).
4. If an override later becomes byte-identical to the fallback, `gleon approve` automatically prunes the redundant manifest.

### How do I delete obsolete tests (Orphan Cleanup)?

When a golden test is removed from the codebase:

1. `gleon status` detects the missing image file and reports it as `Deleted`.
2. Running `gleon stage` on the **fallback platform (macOS)** removes the manifest from Git.
3. Once the fallback manifest is removed, all secondary platforms automatically stop tracking the deleted test.

---

## 💻 Building and Contributing Locally

### Prerequisites

- Rust 1.100 nightly (Edition 2024), pinned in `rust-toolchain.toml` and installed automatically by rustup

### Commands

```bash
# Build binary in release mode
cargo build --release --workspace

# Install binary into local cargo bin (~/.cargo/bin)
cargo install --path gleon --bin gleon --force

# Run full test suite
cargo test --workspace

# Run clippy lints
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Format code
cargo fmt --all
```

---

## 📄 License

The crates shared with other integrations — `gleon-engine` (comparison engine), `gleon-model` (configuration, naming, platform keys and case reports) and `gleon-ffi` (C ABI for the Flutter package) — are licensed `MIT OR Apache-2.0`.

The rest of Gleon is licensed under the [Business Source License 1.1](../LICENSE) (BUSL-1.1), converting to Apache 2.0 after 4 years. Free for non-commercial use, open-source projects, and evaluation.
