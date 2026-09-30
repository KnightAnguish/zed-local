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

The candidate deliberately uses a read-only sandbox until permission-mode/settings
parity is implemented. Do not silently route a user's Full Access session to it.
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
| Settings | Not implemented | Model catalog/pagination, model/effort/fast/collaboration/modes, per-session persistence and defaults |
| Quota | Existing native reader remains in old connector | Connect weekly indicator to persistent native RPC, refresh/watch and account-change invalidation |
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
