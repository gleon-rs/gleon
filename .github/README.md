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

- `.gleon/gleon.yaml`: Workspace configuration file (always inside `.gleon/`, not the repository root). Next to a `pubspec.yaml` (a Flutter package or app, or the root of a pub workspace) it is the Flutter template: the goldens under every `test/` compared exactly (`include: "**/test/**/*.png"`, `mode: exact`), their text on the platform of the run (`fallback_platform`, the OS and architecture of the host or of `--os`/`--arch`), the images of failures excluded (`exclude: "**/failures/**"`); `gleon init --generic` writes the generic template instead (every `.png`, `mode: pixel` with its `threshold` spelled out), `--flutter` the Flutter one anywhere. An existing `gleon.yaml` is never replaced. Like `git init`, `gleon init` initializes the directory it runs in: inside another workspace (a package of a monorepo) it starts a nested one, whose config applies to the goldens below it, and says so.
- `.gleon/.gitignore`: Automatically ignores large binary blobs (`blobs/`), run outputs (`runs/`), the `.env` files, `credentials`, `dashboard.html` and `history.json`.
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
      threshold: 0.01

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
| `gleon init`               | Scaffolds the `.gleon/` directory tree and a `.gleon/gleon.yaml` (`--flutter`/`--generic`). | `gleon init`<br>`gleon init --generic`                                          |
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

- `--config <PATH>`: Specify an explicit path to the configuration file (default: `.gleon/gleon.yaml`, searched upwards from the current directory, no further than the repository root (the first directory with `.git`, `.hg`, `.jj` or `.sl`) and never in the home directory or above unless that is a repository root itself: a `~/.gleon/gleon.yaml` is no workspace, and neither is its `.env`). Integrations search upwards from each golden the same way.
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
# Optional: enforce a minimum CLI version across the team and CI (`gleon init` writes its own).
# A workspace of an integration alone needs none.
required_version: ">=0.1.0"

# Rules for discovering and comparing screenshots (optional: a workspace may name only its
# `fallback_platform`). A file is excluded if it matches `exclude`;
# otherwise the FIRST rule whose `include` matches applies (paths are matched case-insensitively,
# relative to the workspace root, with `/` separators). Globs: `*` and `?` stay within a path
# segment, `**` (a whole segment) crosses segments, `[...]` is a character class; `{a,b}`
# alternatives and a trailing `/` are config errors.
screenshots:
  - include: "test/**/goldens/**/*.png" # Single pattern or list of glob patterns
    # Required: 'exact' (every pixel identical, no `diff` keys), 'pixel' (a share of the pixels
    # may differ, 1% by default) or 'ssim' (tolerates rendering noise, see below). There is no
    # default: a rule added for its masks alone would otherwise tolerate 1% silently (rules
    # without a mode meant 'pixel' before gleon 0.4.0).
    mode: pixel
    diff: # each key belongs to one mode: a key of another mode is a config error, even at its default
      threshold: 0.01 # 'pixel': allowed fraction of differing pixels [0.0 - 1.0] (default: 0.01; 0: exact)
      min_similarity: 0.8 # 'ssim': minimum local SSIM of every neighborhood [0.0 - 1.0] (default: 0.8)
      color_tolerance: 8 # 'ssim': tolerated deviation beyond the local 3x3 envelope, 8-bit units (default: 8)
      # 'pixel' only (a config error on an 'ssim' rule), all off by default, never inside text:
      # differing pixels that count as equal (rendering noise of shapes; not a substitute for
      # 'ssim' or per-platform goldens). Case reports count them as `tolerated_pixels` and
      # `edge_pixels`.
      channel_tolerance: 0 # no RGBA byte differs by more than this [0 - 254]: GPU and color drift
      anti_alias: false # the pixel looks anti-aliased in either image (the detection of pixelmatch)
      # The Sobel gradient of the golden's luma there exceeds this [0 - 254] (Skia Gold's edge
      # mask, unnormalized: a sharp step of 16 gives 64). Every change on edges passes too: a
      # missing glyph or small icon, a 1px move (see gleon-engine/tests/ssim_corpus.rs); the lower
      # the value, the more pixels are edges.
      edge_threshold: 0
    # Optional: integrations that report the text of a screenshot (the Flutter package) compare it
    # under this tolerance (in 'ssim' mode left out of both of its gates), the rest under the
    # rule's mode: the text passes while every 16x16 square of it has at most this share of
    # differing pixels [0.0 - 1.0]. Unset, the default
    # depends on the golden: 0.05 on the platform it was recorded on (see `fallback_platform`),
    # else 1 (text never fails, because operating systems rasterize glyphs differently, about
    # 40% of a square, more than a changed character does). A value set here always applies: 1
    # turns text comparison off everywhere. `gleon diff` sees no text.
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
# For integrations with golden files (Flutter) it is the canonical platform (OS and architecture,
# with the names a process reports: `macos-aarch64`, `linux-x86_64`; `arm64`, `x64`, `amd64` and
# `darwin` are read as them): goldens without a gleon stamp count as recorded there, it records
# again what other platforms recorded, and only with it a golden recorded elsewhere is kept by
# update mode while only its text differs.
fallback_platform:
  os: macos
  arch: aarch64

# Optional: per-golden comparison metrics, recorded by integrations such as the gleon Flutter
# package into `.gleon/runs/latest/cases/<platform>/<test name>.json` (git-ignored with `runs/`).
# The GLEON_METRICS environment variable (1/true or 0/false) overrides `enabled`.
metrics:
  enabled: false # default: false
  console: true # also print one line per golden (default: true)

# Optional: where the images of failed cases go (`<dir>/<platform>/<test name>/{golden,candidate,diff}.png`),
# relative to the workspace root: this default or a directory under `.gleon/runs/` outside `latest/`.
# GLEON_ARTIFACTS_DIR and `gleon diff --artifacts` override it.
artifacts: .gleon/runs/latest/artifacts
```

Remote blob storage (AWS S3, Cloudflare R2, Google Cloud Storage) is configured through environment variables, not in `gleon.yaml`, so credentials never end up in Git: copy `.gleon/.env.template` to `.gleon/.env.local` and set `GLEON_STORAGE_URL` (e.g. `s3://my-visual-baselines-bucket/blobs`) plus the provider credentials (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, ...).

### Case reports (`metrics`)

Case reports are the one result format of gleon (schema: [`gleon-model/schema/case.v5.json`](../gleon-model/schema/case.v5.json)): `gleon diff` writes one per screenshot, integrations one per golden with metrics enabled. A report holds the golden and candidate SHA-256 and size, the effective tolerance and masks, the outcome (`identical`, `match`, `mismatch`, `dimension_mismatch`, `error` with its kind, `updated`, `missing`), the metrics including their headroom to the thresholds (for example `min_ssim - min_similarity` and `color_tolerance - peak_excess` in SSIM mode) and the paths of the images a failure keeps in the artifacts directory. Passing goldens report their margin too, so thresholds can be tuned from measurements instead of guesses.

Reports and images are kept per platform: `.gleon/runs/latest/cases/<platform>/<test name>.json` and `<artifacts dir>/<platform>/<test name>/{golden,candidate,diff}.png`, where `<platform>` is the platform key (`macos-aarch64`, `linux-x86_64`, `os=ios-sim+arch=arm` when the OS or architecture contains `-`, then `+<renderer>` and `+<key>=<value>` per label) and `<test name>` the same on every platform. A report lists only images of its own platform and name, and readers skip a report anywhere else (the flat `cases/<test name>.json` of an older gleon, a copy) with a warning; `gleon clean` removes them. Several platforms in one workspace therefore never overwrite each other: a macOS host and a Linux container on a bind-mounted checkout, or several `gleon diff --platform` runs; `gleon diff` clears only its own platform's previous reports and images. To read them as one joint run, give the processes of every platform the same `GLEON_RUN_ID` (`gleon test` keeps a preset one); the HTML and Markdown reports then name the platform next to each test (`test/goldens/a (linux-x86_64)`), while `junit.xml` always names the platform as the `classname` and the test as the `name`, so CI tools track a test case the same way in every run. `gleon diff` exits with the result of its own cases, but renders `report.md`, `junit.xml` and `report.html` from the cases of `gleon diff` in its run on every platform: the joint run when the platforms share `GLEON_RUN_ID`, its own cases otherwise. Downloaded runs of several CI jobs can also be copied into one `latest/` without clashes.

`gleon report`, `gleon dashboard` and `gleon approve` read the reports of one run: the run of `GLEON_RUN_ID` on every platform (set it in CI, e.g. `${{ github.run_id }}-${{ github.run_attempt }}`); without it, the run of each platform on its own: on the platform where `gleon test` ran, the run it recorded in `.gleon/runs/latest/run.json` unless a newer report of that platform belongs to another run; else, and on every other platform, the run of the platform's newest report (`gleon diff` names its own run when `GLEON_RUN_ID` is not set). Platforms that ran apart (two CI jobs copied into one `latest/`, or a container's tests run apart) are thus both read, with a warning that they come from different runs. Reports without a run id (tests run without `gleon test`) are read together, without those whose golden no longer exists, and with a warning that they may mix runs; without metrics integrations record only failures, so such a run counts no passes: run the tests through `gleon test` for complete totals. Only the process environment sets `GLEON_RUN_ID`, never `.gleon/.env`. Invalid reports are skipped with a warning. `--from <dir>` reads a downloaded copy of `.gleon/runs/latest` (relative to the working directory, with its `cases/`) as it is, ignoring `GLEON_RUN_ID`; `gleon approve` takes several (`--from linux/latest --from macos/latest`), and its filters also take `<platform>/<test name>` (as under `cases/`) to approve one platform's case, or a bare platform key (`gleon approve linux-x86_64`) to approve every case of that platform; filters match whole names, by test name and by `<platform>/<test name>` (so `linux-x86_64/foo` also matches a test named `linux-x86_64/foo` on any platform). The approved cases are listed as `<platform>/<test name>`, sorted. `golden.path` is the golden file of an integration, and for `gleon diff` the screenshot whose baseline lives in the manifests. Approving turns cases of `gleon diff` into manifests and blobs of the platform the case ran on, and overwrites the golden PNG of an integration (only `.png` files inside the workspace, outside hidden directories and not through symlinks); every candidate is checked before anything is written, and two different candidates for one golden are refused, naming both candidate files and how to pick one (a `<platform>/<test name>` filter when they come from two platforms, else one run at a time), unless the `fallback_platform` reported that golden of an integration: then the other platforms' cases for it are skipped (see "Goldens of integrations on several platforms"). The manifests of the `fallback_platform` are written first, so another platform's candidate equal to its new baseline keeps no override. `.gleon/history.json` keeps the failures of each run and counts the passes; an entry names the platforms of its run (a joint run all of them, replacing the entry of its first platform when it is recorded again), and the dashboard's platform filter shows a run under each of them.

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

#### Goldens of integrations on several platforms

Integrations commit goldens as PNG files, one per test, recorded on any platform: each golden
gleon writes names the platform it was recorded on in a `tEXt` chunk of its PNG
(`gleon:platform`, e.g. `macos-aarch64`; decoders ignore it), with or without a workspace or a
rule, except a golden the workspace excludes (written and compared like Flutter does: its text
is pixels like the rest, unless the call sets a text tolerance). Its text is
compared there (`text_tolerance` 0.05: a pixel of font-engine noise per 16x16 square passes, a
changed digit fails) and ignored elsewhere (1, layout exact). A golden without the stamp
(recorded by another tool, or by the Flutter package before 0.4.0) counts as recorded on
`fallback_platform`, with the names a process reports (`std::env::consts`: `macos-aarch64`,
`linux-x86_64`, `windows-x86_64`; `macos-arm64`, `linux-x64` and `darwin` are read as them, in
the config, the flags and the variables alike, while a custom key such as `custom-arm64` keeps
its spelling),
else nowhere known: its text is ignored everywhere, and a pass whose text differed says so once
per test process.

| Update mode (`--update-goldens`) | |
|---|---|
| A missing golden, or one recorded here or nowhere known | written, stamped with this platform |
| The same image, stamped | left as it is |
| The same image, unstamped (recorded here or nowhere known) | written once more, stamped: `--update-goldens` on the recording platform is how an older golden gets its stamp |
| A golden recorded on another platform, with a `fallback_platform` | left as it is while the candidate passes against it (only its text differs), with a warning naming the `fallback_platform` that records it again, so goldens never flip between platforms over their glyphs; written over when it would change |
| A golden recorded on another platform, without a `fallback_platform` | written over (nothing could record it again) |
| On the `fallback_platform`, a golden recorded elsewhere | recorded again, even when only its text differs |

The `fallback_platform` warns about each golden recorded elsewhere it compares (its text is not
compared there); with `GLEON_REQUIRE_CANONICAL=1` it compares them as its own instead, so their
glyphs fail until they are recorded there. A `text_tolerance` set in the rule or the call
applies on every platform, so `1` turns text comparison off everywhere. `golden.platform` of a
case report names the stamp. A failure of a golden a rule covers names its case report (by
its absolute path) and `gleon report html`, which renders the reports of the run.

`gleon approve` writes the candidates of integrations stamped with the platform of their case.
When the `fallback_platform` reported a golden in the approved runs, its result holds: passed, the
golden stays; failed, its candidate is written; the other platforms' cases for that golden are
skipped with a warning (a `fallback_platform` that names only an OS makes every architecture of it
canonical, so their differing candidates conflict). PNG optimizers that strip text chunks drop the
stamp. A `text_tolerance` set below 1 compares text on every platform, so update mode on another
platform than the golden's rewrites it whenever the glyphs differ: the golden then moves between
the platforms that update it.

```bash
gleon approve --from metrics-macos-arm64
gleon approve --from metrics-macos-arm64 --from metrics-linux-x64
```

The platform is the test process's: an x86_64 Flutter under Rosetta on Apple silicon runs as
`macos-x86_64`. The `platform` of `.gleon/gleon.yaml`, `GLEON_PLATFORM` and the platform flags
set the CLI's platform only. The case `name` and rule matching are the same on every platform.

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
| `version`           | Release version tag to download (e.g. `'v0.3.0'` or `'latest'`)                                       | `'latest'`                    |
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

The rest of Gleon — the `gleon` CLI and `gleon-core` — is licensed under the [Business Source License 1.1](../LICENSE) (BUSL-1.1), converting to Apache 2.0 after 4 years. Free for non-commercial use, open-source projects, and evaluation.
