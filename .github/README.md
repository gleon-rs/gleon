[![codecov](https://codecov.io/gh/tsinis/gleon/graph/badge.svg?token=KIUODCEVAK)](https://codecov.io/gh/tsinis/gleon)

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

- `gleon.yaml`: Workspace configuration file.
- `.gleon/.gitignore`: Automatically ignores large binary blobs (`blobs/`) and run outputs (`runs/`).
- `.gleon/.env.template`: Storage credentials template.
- `.gleon/manifests/`: Directory where lightweight, deterministic JSON baseline manifests will be stored in Git.

### 3. Configure `gleon.yaml`

Edit `gleon.yaml` to point to where your test framework outputs golden screenshots. For example:

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

`gleon stage` computes cryptographic hashes, copies the image files into local content-addressable storage, and writes deterministic JSON manifests into `.gleon/manifests/<platform>/`.

Commit these manifest files to Git:

```bash
git add gleon.yaml .gleon/manifests/
git commit -m "chore: record initial visual regression baselines"
```

### 5. Verify & Inspect Diffs (`diff` & `report`)

When you or your team make changes and re-run your visual tests:

```bash
# Check test status (Clean, Added, Modified, Deleted)
gleon status

# Run pixel/SSIM comparison against committed baselines
gleon diff

# View visual diff report in browser
gleon report html --out report.html
```

---

## 🛠️ CLI Command Reference

| Command                    | Description                                                                                 | Example                                                                         |
| :------------------------- | :------------------------------------------------------------------------------------------ | :------------------------------------------------------------------------------ |
| `gleon init`               | Scaffolds the `.gleon/` directory tree and default `gleon.yaml`.                            | `gleon init`                                                                    |
| `gleon stage [PATHS...]`   | Records matching screenshots as official baseline manifests for the current platform.       | `gleon stage`<br>`gleon stage test/goldens/login.png`                           |
| `gleon status`             | Reports the status (`Clean`, `Added`, `Modified`, `Deleted`) of all discovered screenshots. | `gleon status`<br>`gleon status --json`                                         |
| `gleon diff`               | Runs visual comparison between actual screenshots and committed baselines.                  | `gleon diff`<br>`gleon diff --target-branch main`                               |
| `gleon report <FORMAT>`    | Generates a report (`html`, `markdown`, `junit`, `json`) from the last `gleon diff` run.    | `gleon report html --out report.html`<br>`gleon report markdown --pr-number 42` |
| `gleon approve [NAMES...]` | Accepts detected visual differences as the new baseline manifests.                          | `gleon approve`<br>`gleon approve auth/login`                                   |
| `gleon pull`               | Downloads missing baseline blobs from remote object storage to local storage.               | `gleon pull`<br>`gleon pull --all-platforms`                                    |
| `gleon push`               | Uploads locally staged baseline blobs to remote object storage.                             | `gleon push`                                                                    |
| `gleon clean`              | Removes ephemeral diff artifacts and orphaned files.                                        | `gleon clean`<br>`gleon clean --dry-run`                                        |
| `gleon lint`               | Verifies integrity and schema compliance of all manifests and configs.                      | `gleon lint`                                                                    |
| `gleon resolve`            | Interactively or automatically resolves Git merge conflicts in baseline manifests.          | `gleon resolve`                                                                 |

### Global Flags

All commands support the following global options:

- `--config <PATH>`: Specify an explicit path to `gleon.yaml`.
- `--target-branch <BRANCH>`: Target branch for baseline comparison (defaults to `main`, or `GLEON_TARGET_BRANCH`).
- `--platform <STRING>`: Override platform context with an opaque string (e.g. `--platform my-custom-env`).
- `--os <OS>` / `--arch <ARCH>` / `--renderer <RENDERER>`: Override individual platform context dimensions.
- `--label <KEY=VALUE>`: Add custom isolation labels (e.g. `--label theme=dark`).
- `--verbose`: Enable debug logging output (routed to `stderr`).
- `--quiet`: Suppress informational output (only display warnings and errors).

---

## ⚙️ Configuration Reference (`gleon.yaml`)

Below is a complete, annotated `gleon.yaml` reference:

```yaml
# Enforce minimum CLI version across the team and CI
required_version: ">=0.1.0"

# Rules for discovering and comparing screenshots
screenshots:
  - include: "test/**/goldens/**/*.png" # Single pattern or list of glob patterns
    mode: pixel # 'pixel' (exact per-pixel compare) or 'ssim' (tolerates rendering noise, see below)
    diff:
      threshold: 0.1 # 'pixel': allowed fraction of differing pixels [0.0 - 1.0] (default: 0.1)
      anti_alias: true # Reserved, currently has no effect; use mode 'ssim' to tolerate anti-aliasing
      min_similarity: 0.8 # 'ssim': minimum local SSIM of every neighborhood [0.0 - 1.0] (default: 0.8)
      color_tolerance: 8 # 'ssim': tolerated deviation beyond the local 3x3 envelope, 8-bit units (default: 8)
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
fallback_platform:
  os: macos
  arch: aarch64

# Optional: Remote blob storage (AWS S3, Cloudflare R2, Google Cloud Storage)
storage:
  url: "s3://my-visual-baselines-bucket/blobs"
  options:
    region: "us-east-1"
```

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

### Ephemeral Diff Branch Cleanup

When visual diffs occur, ephemeral branches (`gleon/diffs/pr-<PR_NUMBER>`) store diff artifacts. To automatically delete them when a PR is merged or closed, add `.github/workflows/gleon-cleanup.yml`:

```yaml
name: Gleon Ephemeral Branch Cleanup

on:
  pull_request_target:
    types: [closed]

jobs:
  cleanup:
    permissions:
      contents: write
    uses: gleon-rs/gleon/.github/workflows/cleanup.yml@main
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

1. Configure `fallback_platform` in `gleon.yaml` (e.g. `os: macos`, `arch: aarch64`).
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

Gleon is licensed under the [Business Source License 1.1](../LICENSE) (BUSL-1.1), converting to Apache 2.0 after 4 years. Free for non-commercial use, open-source projects, and evaluation.
