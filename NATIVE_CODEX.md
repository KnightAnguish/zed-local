# Native Codex migration

## Decision and deployment boundary

2026-09-30: the user chose a **complete Rust replacement** of `codex-acp-modern`,
not bundling the Node/TypeScript adapter. Branch: `decay/native-codex`, based on
`c5adca403b` (native history stages 1–3). Do not ask this architecture question again.

The replacement is **not production-ready**. `CustomAgentServer::connect` still
selects the old ACP connector. `codex_native` is compiled and tested, but is not
reachable from editor routing yet. Do not remove the adapter or switch settings
until the remaining parity gates below have passed. No account, production profile,
chat database, installed editor, or adapter bundle is changed by these sources.

## Structure

Deployment clarification (2026-09-30): the user approved using the Zed fork before
native connector parity. Deploy the editor with the existing ACP connector while
continuing the Rust rewrite; the native routing gate above remains unchanged.
Upstream 1.22.0 is being integrated before profile migration. Keep the production
editor and profile intact until the candidate build and tests pass and the user
can close the running editor for a consistent profile backup.

Profile cutover checklist: use a fresh explicit `--user-data-dir`; copy the closed
official `%LOCALAPPDATA%/Zed` data and `%APPDATA%/Zed` configuration into that
directory and its `config` subdirectory, respectively. Never overwrite the source.
The 1.22.0 candidate's `crates/zed/RELEASE_CHANNEL` is `stable`, so retain the
`db/0-stable` and `db/0-global` scopes (do not rename them to dev). Preserve all
chat stores and settings; verify copied file hashes and SQLite integrity before
launch. Keep the existing configured Codex runtime/account environment; do not
print or transplant auth secrets. Adapter candidate source and production bundle
must be distinguished when validating message forks. Do not claim successful
cutover until restored projects/chats and a fresh adapter connection are checked.

- `codex_native/transport.rs`: one configured `codex.exe app-server --listen stdio://`
  child; bidirectional JSONL requests/responses/notifications. No Node or shell.
  Uses the explicit executable/environment, preserves the runtime-owned account.
  Reader and writer run concurrently; no cancelling partially read frames.
  16 MiB/frame, 8 queued frames in each direction, 128 pending requests, 60 s RPC
  response timeout (not a turn generation deadline). Dropped callers remove pending
  response slots; disconnect fails pending RPCs. Writes are never automatically replayed.
  No raw stderr retention and no auth-file reads. Child lifetime follows connection.
- `codex_native.rs`: native `AgentConnection` implementation, foreground event router,
  per-thread turn state, visible start failures, conservative reconnect requirement after
  unknown outcomes, cancel-before-start handling, account status, session create/resume,
  list/name/archive/unsubscribe, bounded initial history tail and existing paged reader.
- `codex_native/events.rs`: internal UI translation using existing ACP **data types**;
  this is not ACP transport. Stable message IDs, streamed/final reply deduplication,
  thoughts, basic tool lifecycle/output, token usage, visible retry diagnostics.
- `codex_native/profiles.rs`: read the existing `acp-skill-profiles.json`, select the
  longest matching project root, merge inherited disables plus catalog/cache matches.
  No second registry, no config writes, no automatically reenabled skill. Invalid
  profiles fail visibly. `CODEX_ACP_SKILL_PROFILES=off`/explicit path preserved.

The candidate opens with a read-only sandbox; explicit permission-mode selection now
changes subsequent turns. Import of Zed's saved defaults is still pending. Do not
silently route a user's Full Access session to it.
Unknown server requests are rejected, never accepted automatically. Unsupported prompt
attachments fail explicitly rather than disappearing from a submitted prompt.

## Parity gates before any switch

| Area | Current native replacement | Still required |
| --- | --- | --- |
| Process/RPC | Implemented; fake-peer race/disconnect tests; real-runtime read-only probe passed | Process shutdown/reconnect/UI smoke |
| Sessions | Create/resume/list/name/archive/unsubscribe implemented | Resume/open error cleanup; active-turn reattachment; lifecycle/close tests |
| Chat/history | Streaming, final dedup, initial tail ≤20 turns, page reader | Controller integration with native connector; long-session RSS; no full-history regression |
| Permissions | Command/file allow-once/reject/cancel UI | Full decision contract, server resolution cleanup, permission profile requests, structured questions, MCP elicitations/auth |
| Tools | Basic tool lifecycle and completed text output | Terminal streaming/lifetime/output bounds, structured file diffs/action log, MCP output, plans/compaction, subagents/background tasks |
| Skills | Existing registry/catalog/cache merge ported; policy unit tests | Real project profile comparison against adapter; skill mention/completion; multi-root and project MCP bridge |
| Settings | Paginated catalog, per-session model/effort/Fast/permissions, next-turn overrides, UI watches | Collaboration mode, saved picker choices before a turn, Zed defaults, server-settings reconciliation and GUI tests |
| Quota | Persistent native RPC, shared formatter, refresh/watch, account/disconnect invalidation | Native GUI smoke and sustained reconnect tests |
| Auth/provider | Existing account checked through RPC | Login/logout browser/terminal flows, token refresh, gateway/custom providers; do not expose credentials |
| Forks/edit | Not advertised by replacement | Full fork, message-boundary resolution, empty first-message fork/revert, stable user IDs, grouped variants regression tests |
| Attachments | Text and inline-image input | Resource/embedded/skill/audio inputs; generated images/artifacts and historical input restoration |
| Commands/goals | Not implemented | Command inventory, compact/review/goals/steer, async-task reconciliation |
| Remote | Explicitly rejected | Remote executable/environment/filesystem routing, or separately agreed unsupported scope |

The adapter's candidate source on `local/zed-message-fork` is the parity oracle, **not**
the stale production `dist/index.js`. Inspect installed app-server schemas before assuming
old APIs: generated protocol source is under `codex-acp-modern/src/app-server`.
Current runtime is 0.159.2. Reference: https://learn.chatgpt.com/docs/app-server.

## Tests and rollout

Focused: `cargo test --locked -p agent_servers --lib codex_native -j 6` using the
MSVC/Cargo environment documented by `script/local-build.ps1`.

The explicitly ignored `codex_native::transport::tests::live_read_only` requires
`ZED_CODEX_HISTORY_TEST_EXE` and `ZED_CODEX_HISTORY_TEST_THREAD`. It only initializes,
reads a chosen diagnostic thread page, lists models and reads account limits on the
same connection. It never resumes, prompts, archives or edits a real user thread.
Fake-peer tests spend no model tokens. A successful read-only test is not live chat parity.

Before routing: complete the table, run native fake-peer and existing core/UI suites,
then build. Use an isolated ZedLocal profile and disposable conversations for functional
tests, including approvals, cancel, reconnect, editing/forks, tools, images, history scrolling,
skill settings and quota. Record actual tests in maintenance history; do not promote
production verification from version/compilation alone. Keep old connector selectable
for rollback until native parity is demonstrated. Rollback now requires no configuration
change: the production connector was never switched.

The upstream WebSocket-to-HTTPS warning concerns Codex-to-service traffic, not this
local stdio transport. Rewriting this layer does not prove that warning is fixed.

## Verified checkpoint (2026-09-30)

- All 57 `agent_servers` unit/GPUI tests passed (2 explicit live tests ignored by default).
  17 tests exercise this new implementation: RPC routing/framing/bounds/disconnect,
  cancelled requests, streaming dedup, terminal status, permission rejection/decisions,
  cancel-before-start, early completion, thread isolation and skill profile policy.
- The separately enabled native read-only probe passed against installed Codex 0.159.2
  (0.64 s; repeated after final source changes, 0.76 s), using only the chosen diagnostic
  thread. Zero model requests and no user conversation mutations.
- The first real probe exposed a test-harness mismatch: deterministic GPUI scheduling
  advanced time immediately and rejected external IO wakeups. The explicitly live-only
  test now opts into parking/external IO and ticks without advancing simulated time,
  with a separate 30 s wall-clock deadline. No production timeout was relaxed.
- Manual editor interactions and full live native agent turns remain **unverified**.
  This checkpoint is not a claim of complete replacement or an active deployment.
- Full Windows dev build passed (`script/local-build.ps1 -Jobs 6`, 1m07s).
  Formatting and `git diff --check` passed. The binary was not installed or launched.

## Settings and quota checkpoint (2026-09-30)

- Native connection obtains the paginated server model catalog; effort options and Fast
  support are discovered, not hard-coded. Cursor cycles/duplicate IDs/excessive lists fail.
  Changing model preserves compatible effort or uses its reported default, and clears
  unsupported Fast. Unknown saved models remain visible rather than silently substituted.
- Settings belong to each session and are snapshotted into `turn/start`; changing them
  during an active/uncertain turn fails. Full access requires explicit mode selection.
  Leaving permission selection on "Keep server permissions" sends no permission override.
  This is not a migration of the old adapter's default or unsent per-chat preferences:
  pending picker choices are currently memory-only until a turn uses them. Collaboration
  mode is intentionally not guessed from outdated docs; installed 0.159.2 TurnStartParams
  does not expose the older collaborationMode field. No global config is written.
- Weekly percentage uses the same persistent RPC and existing tested formatter. Automatic
  refresh is throttled to 60 seconds; manual refresh to 2 seconds; overlapping refreshes
  coalesce. Refresh on connect, option retrieval, turn completion and rate-limit events.
  Account changes/disconnect clear cached data and cancel requests; generations prevent
  old-account replies from repopulating the indicator. No new subprocess or timer polling.
- All 65 agent_servers tests passed (2 live probes ignored by default), including eight
  new tests for model compatibility, next-turn parameter wiring, active-turn guards,
  catalog pagination, quota throttling and late old-account replies. Actual server turn
  generation, GUI controls and durable preference restoration remain unverified.
- OpenAI Docs app-server documentation was consulted; model capabilities are server-owned.
  Installed runtime schemas take precedence where current documentation differs.
- Updated real-runtime read-only probe passed in 0.84 s, including model type validation.
  Full Windows dev build passed in 52.42 s. No production switch or manual GUI test.
