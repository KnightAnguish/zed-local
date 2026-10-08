# Native Codex migration

## Send crash repair (2026-10-02)

The first main-profile trial exposed reentrant AcpThread access in native prompt.
A regression through AcpThread::send_raw fails before the fix and passes afterward;
direct connection tests had missed the real caller. Optimistic message identification
now runs in the foreground task before RPC, after the caller entity update returns.
Desktop Zed Local targets `builds/native-codex-sendfix-20261002`; user relaunch and
GUI acceptance remain pending. Pencil/branch actions now share the copy/navigation
row instead of standalone text buttons. Remaining migration gaps below still apply.

## Decision and deployment boundary

2026-09-30: the user chose a **complete Rust replacement** of `codex-acp-modern`,
not bundling the Node/TypeScript adapter. Branch: `decay/native-codex`, based on
`c5adca403b` (native history stages 1–3). Do not ask this architecture question again.

The replacement is **not fully parity-complete**. `CustomAgentServer::connect` defaults
to the old ACP connector. An explicit `ZED_CODEX_NATIVE=1` in the configured Codex
agent environment selects the native connector. No automatic fallback to ACP occurs
after a native connection error. Keep the adapter and previous build for rollback.

2026-10-02 trial deployment exception: after the isolated GUI attempt, the user
explicitly requested rebuilding and trying the main installation, accepting that
problems would be reported through Codex. The desktop Zed Local shortcut now targets
`builds/native-codex-20261002`; the existing profile has only the native opt-in added.
The Node command remains as backward-compatible configuration for the old executable;
the new connector ignores it and starts CODEX_PATH directly. This is a manual-restart
pending trial, NOT proof of GUI acceptance or completion of the remaining gates.
Settings, shortcut and SQLite online snapshots are backed up under the maintenance
workspace's `state/backups/native-cutover-20261002`. Account files were not accessed.

## Structure

2026-10-02 pencil correction: message editing now uses the inline editor on the
original thread. Opening/cancelling the editor performs no fork or rewind. Sending
resolves the edited content, forks the session, and auto-submits only in the new
variant. A fork failure leaves the inline draft available for retry. While the
request is pending, the editor is read-only; variant switching is blocked until
editing is cancelled or sent. Existing legacy drafts created by the earlier
eager-fork UI are not deleted or rewritten by this change.

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

The candidate opens with a read-only sandbox; explicit permission-mode selection
changes subsequent turns. Saved editor model/effort/Fast/mode defaults are applied
before a turn; an unsupported saved value fails visibly rather than being substituted.
Unknown server requests are rejected, never accepted automatically. Unsupported prompt
attachments fail explicitly rather than disappearing from a submitted prompt.

## Parity gates before any switch

| Area | Current native replacement | Still required |
| --- | --- | --- |
| Process/RPC | Implemented; fake-peer race/disconnect tests; real-runtime read-only probe passed | Process shutdown/reconnect/UI smoke |
| Sessions | Create/resume/list/name/archive/unsubscribe implemented | Resume/open error cleanup; active-turn reattachment; lifecycle/close tests |
| Chat/history | Streaming, final dedup, initial tail ≤20 turns, page reader | Controller integration with native connector; long-session RSS; no full-history regression |
| Permissions | Command/file/session/policy choices, explicit permission-profile grants; questions and standard MCP forms/URLs; resolution/disconnect cleanup | Real GUI contract; protected secret questions, OpenAI-form variants, URL-completion reconciliation and overlapping callbacks for one tool |
| Tools | Bounded command output streaming, final replacement, text diffs, MCP and mixed media output, generated images, plans, basic subagent state | Native terminal controls/lifetime, structured file diffs/action log, background task reconciliation and GUI smoke |
| Skills | Existing registry/catalog/cache merge ported; policy unit tests | Real project profile comparison against adapter; skill mention/completion; multi-root and project MCP bridge |
| Settings | Paginated catalog, per-session model/effort/Fast/permissions, next-turn overrides, UI watches, editor defaults with legacy effort-key compatibility | Collaboration mode, durable unsent per-session picker choices, server-settings reconciliation and GUI tests |
| Quota | Persistent native RPC, shared formatter, refresh/watch, account/disconnect invalidation | Native GUI smoke and sustained reconnect tests |
| Auth/provider | Existing account checked through RPC | Login/logout browser/terminal flows, token refresh, gateway/custom providers; do not expose credentials |
| Forks/edit | Native full/message-boundary forks; copy-only first-turn revert; live user IDs; in-memory preference inheritance | Native GUI/grouped-variant smoke, attachment drafts, persisted unsent preferences, interrupted-operation reconciliation |
| Attachments | Text/image/audio, resource links and embedded text/binary media; generated and tool-output images | Skill input/completion, historical input restoration without loading large media eagerly, live/UI smoke |
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

## Native forks checkpoint (2026-10-02)

- Production still uses the old adapter bundle; it was NOT rebuilt or replaced.
  The requested end state remains the native Rust connection, not an updated adapter.
- `codex_native/forks.rs` performs native `thread/fork` and `thread/revert` requests.
  Message boundaries are resolved one summary page at a time (20 turns), with cursor
  cycle/page-count guards. First-message edits clear only the new copy and verify it
  contains no turns. Failed copy preparation archives only that copy; an ID equal to
  the source is rejected before any revert/archive. Local active/uncertain sessions,
  concurrent fork/send/close and unsupported remote/multi-root paths are rejected.
- Native user item events bind their server ID to the exact optimistic entry without
  echoing its text. A stable local ClientUserMessageId survives history prepends;
  late events from a different turn cannot bind a new user message.
  Forks inherit current in-memory model/effort/Fast/permission picks on first load.
  The existing variant controller owns grouping and per-session context indicators.
  The legacy unavailable-edit overlay is hidden when message forking is supported.
- Normal resume/open failures unsubscribe their server session, keep the source and
  pending fork preferences, and do not register a broken local session. Cancellation
  while awaiting open/fork still requires lifecycle reconciliation before deployment.
- Real installed 0.159.2 probe passed: empty first-message fork, final-reply prefix,
  full copy and unchanged source turns. No inference requests. Three copies per probe
  archived; the previously archived diagnostic source was restored to its archive.
  The first attempt only discovered the source was archived and made no copies.
- The live Rust test is ignored by default. Use the existing explicit runtime/thread
  environment variables, plus `ZED_CODEX_FORK_TEST_UNARCHIVE=1` ONLY for an archived
  disposable diagnostic source; it restores archive state after the test.
- Final suites/build results are recorded in the workspace maintenance log. These
  changes are not an installed fix: all remaining parity gates and isolated GUI tests
  still apply. Do not switch the user's profile merely because fork tests pass.
- Final sequential tests: 159 acp_thread + 75 agent_servers + 469 agent_ui passed;
  three live tests ignored by default. Current-binary real fork probe separately passed.
  One earlier parallel UI run had a WSL metadata migration failure; isolated and serial
  reruns passed, cause not established. Full dev Zed + CLI build passed (9m53s).
  Candidate was not installed or launched; native GUI interaction remains unverified.

## Native input, permissions and events checkpoint (2026-10-02)

- Added explicit command/file session grants and server-proposed policy amendments;
  permission-profile choices grant exactly the requested fields for a turn/session.
  Cancellation never selects an allow option. Permission details are visible in the card.
- Questions use the existing Zed elicitation form with stable question IDs and validated
  answers. Server-resolved requests cancel the form. Secret inputs are rejected because
  the current form path does not guarantee protected entry. Standard MCP forms and URL
  requests are translated; unsupported form modes fail explicitly.
- Input resources retain their content/order; audio is no longer discarded. Tool output
  tails are limited to 64 KiB (plus a marker), UTF-8 safe, with at most 128 live output
  streams per turn. Completed command output replaces the stream, not appends it.
  MCP output, generated PNGs, mixed function media, plan updates and subagent summaries
  are represented in the UI data model. This is not yet a bound on all live chat memory.
- Open/fork operations finish reconciliation even if their UI waiter is dropped.
  Abandoned opens unsubscribe; abandoned forks archive only the new copy. Released
  thread entities remove only their own session generation and unsubscribe.
- Explicit experimental routing is available for a separate profile; production routing,
  executable, account and chat store have NOT been changed. Remaining table gates still
  apply. GPUI/unit tests are not a manual/native GUI acceptance test.
