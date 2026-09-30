> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed

## Local fork

Branch `decay/local` starts at official tag `v1.21.0` (33c95853ed2b6956f339733c63a8220964ecbeb6).
`upstream` points to the official repository; pushes to it are disabled locally.
`upstream/main` is tracked for inspection, but stable releases are the default merge candidates.
There is no automatic merge, installation, or background Windows service.

Build with `script/local-build.ps1`; generated artifacts and the Cargo cache stay in
`D:/Build/zed-local`. Run only with `script/local-run.ps1`, which requires an explicit
isolated user-data directory and refuses the official Zed profile. The official installation
and its settings/databases must not be overwritten. Do not copy live databases as ordinary
files: migrate using a consistent backup only after the user agrees to switch.

The binary is `zedlocal.exe`. Building on Windows requires the x64 Spectre libraries;
the local build script selects an installed MSVC toolchain containing them without
changing the machine-wide compiler default. Do not share the same Cargo target directory
between worktrees with different source revisions; use a separate `-BuildRoot` for experiments.

Update workflow: fetch upstream, inspect the stable release changes, create a candidate
branch from our last tested commit, merge the chosen release, run relevant tests/build,
then switch only after a functional check. Keep the preceding tested build for rollback.
Source availability, compilation, and a successful UI test are distinct verification stages.

The local updater checks stable release metadata at startup and once per hour while Zed
is running (unless `auto_update` is disabled). A title-bar notice opens release notes;
it never downloads or installs the official editor. No checks run while Zed is closed.
Default application data paths use `ZedLocal`, with a separate Windows app identifier.
The launcher still requires an explicit profile to keep experiments isolated from daily use.

Initial backlog: mouse-driven sent-message editing;
forking a conversation at a selected turn; preservation of the bounded restored UI history
and weekly usage indicator provided by `codex-acp-modern`. Chat forks share history up to
the split, not subsequent messages; project files remain shared unless a worktree is chosen.

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
