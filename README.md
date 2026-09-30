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

The agent panel's ellipsis menu offers **Continue in New Chat** when the current agent
advertises ACP session forking and history loading, and the root thread is idle.
It forks the stored session through ACP, then opens the returned session with normal
history loading. It never rebuilds agent context from the bounded visible transcript.
The source conversation is preserved; no prompt is submitted automatically. Files in
the project are shared, not rolled back or copied. Repeated clicks while a fork is pending
are ignored, and a failed fork reports an error without replacing the original chat.

With adapter capability `_meta.zedMessageFork: 1`, idle root conversations offer
**Edit Message** on user messages with a server ID and **New Variant After Reply**
on final replies. Editing forks before the user turn and opens an unsent draft; it never
changes the source conversation. The adapter validates the selected boundary against
stored Codex history. Editing the first message uses `thread/revert` on the new fork only
(tested with Codex 0.159.2). A later steering message within the same turn is rejected.
Live attachment messages may require reopening history before their server ID is available.

Message-boundary edits/forks are grouped locally into one sidebar conversation, with a
`< 1/2 >` variant selector in the chat toolbar. Each variant keeps its own session and
context meter; token usage is never summed across variants. Selection loads the saved
session, not a new prompt. A running turn, pending prompt, queued message or nonempty
composer blocks switching. The separate **Continue in New Chat** action remains independent.
The metadata database stores parent/message boundaries and the selected variant in a new
`conversation_variants` table; per-variant drafts use the existing draft-prompt store.
Sidebar queries show only the selected member. Archiving it hides the group; deleting a
member does not delete other histories and exposes a surviving member if necessary.
Corrupt group storage leaves original rows visible and disables creating new groups.
The selector currently lists all group variants in creation order, not a per-message tree.
These changes live on `decay/grouped-variants`; the official profile is not migrated.
The candidate adapter lives on `local/zed-message-fork`; production dist is not replaced.
The full dev build and all 447 agent_ui tests pass, including per-variant context,
group persistence, draft storage and empty first-message fork durability. Protocol fork
tests passed in the preceding candidate; mouse layout/clicks and production integration
remain unverified.

Remaining backlog: UI smoke verification and isolated profile migration, including
preservation of the bounded restored UI history and weekly usage indicator provided by
`codex-acp-modern`. Chat forks share history up to
the split, not subsequent messages; project files remain shared unless a worktree is chosen.

Native integration, stages 1–2 (`decay/native-history`): a local `codex-acp` connection with
an explicit absolute executable in `CODEX_PATH` can now read older history directly from
Codex, without asking the ACP adapter to replay it. Scrolling near the top or clicking
**Load earlier messages** requests `thread/turns/list` pages of 20 turns. The reader uses
the configured runtime/environment, never a PATH fallback, and is disabled for remote
projects. Each short-lived app-server only initializes and reads stored history/account limits: no
resume, prompt, history mutation, shell wrapper, or model request. A 30-second deadline,
16 MiB response-line bound, cursor progress check and three-page-per-gesture bound protect
the reader. Errors remain visible with an explicit retry; no full-history fallback.

Messages are prepended by stable protocol IDs, with an overlap check on the first page,
without changing the context meter or stored Codex history. UI editors are rebound to
their new indices and the previous scroll anchor is restored. Loading/rebinding is
deferred while the thread is generating or a message is being edited. The initial tail,
prompts, tools, permissions and skill profiles still use the existing adapter.
This is NOT yet a replacement for that adapter. Older attachments currently have text
placeholders; generated-image tool results are filtered with other historical tool steps.
When scrolling back toward newer messages, whole loaded pages more than 40 entries above
the viewport are released, with the request cursor retained for reloading. Live entries,
agent context and stored history are never evicted. This is one-sided reclamation, NOT a
fully bounded bidirectional window: deep upward browsing and a long live session can still
grow memory; recreating a view over a retained thread loses its page bookkeeping.
Lazy attachment previews and native live-session integration remain unfinished.

Weekly usage is now read and formatted in Rust via `account/rateLimits/read`, replacing the
adapter's `weekly-usage` config option in the existing percentage/arrow UI. Only the account
Codex weekly window is used, never a model-specific quota. Unknown/failed/expired values
show an em dash, not 0%. Tooltip contains remaining percent and reset time in Moscow time.
Refresh occurs at connection/config access/prompt start (60-second throttle), or explicitly
via Refresh (2-second throttle); there is no background periodic timer. No model prompt is
submitted. The adapter may still perform its own legacy usage reads until it is replaced.

Validation: final dev build, 150 acp_thread tests, 39 ordinary agent_servers tests and
449 agent_ui tests pass. The final parallel UI run had one nonreproducing workspace-DB
backfill failure; that test alone and all 449 tests run sequentially pass. An explicitly
selected real diagnostic session read plus account-limits read pass against Codex 0.159.2,
with zero model prompts. Mouse scrolling,
long-chat memory measurements and production-profile integration are still unverified.
The official editor, its profile and production adapter bundle remain unchanged.

Assistant responses also render standalone `:codex-followup[Label]{prompt="Text"}`
lines as buttons below the response. Straight and smart quotes are supported. Hover shows
the complete prompt; clicking uses the normal message send path. Buttons are disabled while
the agent is busy, in subagent views, or while the composer contains a draft. Source history,
copy/search text, code examples, and malformed/incomplete directives remain unchanged.
Only the last 64 directive-bearing message chunks have an extra presentation cache.
Inline directives embedded in prose are not converted. UI clicking remains to be verified.

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
