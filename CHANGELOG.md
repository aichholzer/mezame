# Changelog

All notable user-visible changes to Mezame!. Format follows
[Keep a Changelog](https://keepachangelog.com) loosely; versions are
[SemVer](https://semver.org).

The version is tracked in three places and must match:

- `Cargo.toml` → `package.version`
- `ui/package.json` → `version`
- the heading of the top-most release section below

The UI bundle surfaces its version in the top-right of the header via a
build-time Vite define.

## [0.8.34] - 2026-05-29

### Fixed

- Mode and model selections did not propagate across browsers
  attached to the same session. A user picking Sonnet in browser
  B left browser A still showing Opus, and the next attach saw
  whichever value was current at first negotiation. The hub now
  awaits the agent's response to `session/set_mode` and
  `session/set_model`, mutates the cached `session_info` half of
  its snapshot, and broadcasts the updated `session_info` event
  to every attached browser. Future attaches read the latest
  selection from the snapshot, so a fresh page load also picks
  up the correct value.

  When the agent rejects a mode or model change, the hub
  broadcasts a sys-line error notice so peers see the failure
  rather than a silent revert mismatch.

## [0.8.33] - 2026-05-29

### Changed

- Reasoning tokens (Kiro's `agent_thought_chunk`) are no longer
  rendered as a stream of `(thinking) word` sys lines. The server
  now emits a dedicated `thought` wire event, the browser
  aggregates chunks into a single log entry per turn, and the UI
  renders the result as a small collapsible "Reasoning" block,
  collapsed by default. Click to expand and see the model's
  thought process; turn boundaries (`prompt_done` / `error`) start
  a fresh block on the next turn's first chunk.

  No backwards compatibility for the old shape: any reasoning
  agent talking to a 0.8.33 server will produce the new event;
  older browsers viewing a 0.8.33 server would silently drop
  unknown events. Refresh the page after upgrading.

## [0.8.32] - 2026-05-29

### Fixed

- Peer browsers did not show the "Agent is working" spinner when
  another browser sent a request. The reducer treated the user
  prompt echo as a plain log entry, leaving `thinking`,
  `inFlight`, and `busy` unchanged on the receiving side.

  Fix: when the live broadcast is an `append { role: 'user' }`,
  set the same flags the sender sets in `sendPrompt`. The hub's
  prompt-echo is already the single source of truth for the start
  of a turn, so peers light up alongside the sender. The existing
  `prompt_done` arm clears them everywhere, so the spinner falls
  away in lockstep on every attached browser. History replays go
  through `/history`, not the live broadcast, so historic user
  turns do not trigger this branch.

  Side effect: peer browsers now also lock their composer for
  the duration of a turn. The hub serialises commands anyway, so
  letting both browsers send concurrently would just queue at the
  hub; locking is the friendlier signal.

## [0.8.31] - 2026-05-29

### Fixed

- Ctrl+C no longer stopped Mezame after the SSE stream landed in
  0.8.28. axum's `with_graceful_shutdown` waits for every in-flight
  request to drain, and an SSE long-poll never finishes on its own.
  The "Received SIGINT, shutting down." line printed because our
  signal handler ran, but axum then sat waiting on the SSE
  handlers forever.

  Fixed by threading a `tokio::sync::Notify` through `AppState`.
  The signal handler fires it before yielding control to axum,
  the SSE handler races its broadcast receiver against the
  shutdown notify, and either branch ending closes the stream.
  Axum's drain then completes promptly.

## [0.8.30] - 2026-05-29

### Fixed

- Closing a session on browser A did not propagate to browser B,
  and vice versa. Worse, after a few rounds the closed sessions
  would reappear on both browsers, making it effectively impossible
  to close anything when two browsers were open.

  Root cause: 0.8.28's reconcile was additive only. When B closed
  sessions 1 to 3, B's PUT held only [4]. A's reconcile saw 4
  already locally and did nothing; A's own next PUT then wrote
  [1, 2, 3, 4] back to the server, and B saw the closed sessions
  reappear on the next tick.

  Fix: reconcile is now authoritative. The server snapshot is the
  source of truth: sessions present locally but missing on the
  server get closed locally; sessions present on the server but
  missing locally get restored; sessions present on both keep
  their local instance (its WebSocket, log, busy flags) but pick
  up label changes. Newly-created local sessions that have not
  yet had a chance to round-trip through `/state` are exempt from
  the close so a fresh tab is not auto-killed by an SSE tick that
  fired before our first PUT landed.

  Also: cancel any pending sync after a reconcile, otherwise our
  pre-reconcile snapshot would clobber the server view we just
  applied. The closed-history list also follows the server now,
  so the recent-sessions dropdown stays consistent across
  browsers.

  Also: reverted the 0.8.28 change that persisted `acpSessionId`
  for unused sessions. That change caused a storm of "Session not
  found" errors at startup whenever Kiro had not yet written the
  session JSONL. Peer browsers now see a fresh tab from elsewhere
  only after the first prompt; the tradeoff is fewer log errors
  and a cleaner restart story.

  Also: reconcile on EventSource open, so a browser that missed
  ticks while offline catches up the moment the SSE stream
  reconnects.

## [0.8.29] - 2026-05-29

### Fixed

- Two browsers reconnecting at server startup with the same
  persisted session id raced through the hub registry's slow path:
  both saw an empty registry, both spawned an agent, both called
  `session/load` against the same Kiro session id. The second lost
  on the lockfile with "Session is active in another process",
  fell back to `session/new`, and ended up in an unrelated session
  with no history. Both browsers' chats came up empty.

  Fixed by serialising the slow path on the requested session id.
  `attach_or_create` now acquires (or creates) a per-id mutex
  before building a hub for a resume request, holds it across
  `build_hub`, and re-checks the registry after taking the mutex.
  The first arrival builds the hub and registers it; the second
  finds it already there and falls into the fast attach path.
  Fresh-session attaches (no `?session=`) skip the gate entirely
  since they are independent by definition.

  The per-id mutex is dropped from the auxiliary map as soon as
  nobody else is waiting on it, so the gate does not leak one
  mutex per session id ever attached.

## [0.8.28] - 2026-05-29

### Fixed

- A new session opened in browser A did not appear in browser B
  until B reloaded. The session list lives in `state.json`, written
  by every browser after a local change, but B had no signal that
  the file had moved on under its feet. Added a server-sent events
  stream at `/state/events` that ticks once on every `PUT /state`,
  and a client subscription that refetches `/state` on each tick
  and merges any sessions the other browser opened into the local
  list.

  The merge is additive: a tab that exists on the server but not
  locally gets restored; a tab that exists locally but not on the
  server stays put. We do not auto-close tabs from under a user
  just because another browser closed them; that asymmetry is
  acceptable for stage 1.

  Side effect of the same change: the client now persists
  `acpSessionId` on every session, including unused ones. The hub
  keeps unused sessions warm in the registry, and if the registry
  has forgotten one (grace expired) the existing
  `session/load` to `session/new` fallback in `negotiate_session`
  kicks in transparently. Without this, peer-browser reconcile
  would skip new tabs until the user sent a first prompt.

## [0.8.27] - 2026-05-29

### Fixed

- Sender's composer stayed locked at "Agent is working" after a
  multi-attach turn finished. The hub's prompt path fired
  `session/prompt` as fire-and-forget and never broadcast a
  `prompt_done` (or `error`) once the agent's reply resolved, so
  the sender's `busy`/`inFlight` flags, which only clear on those
  events, never flipped back. Peer browsers were unaffected because
  they had not entered the busy state in the first place.

  Fixed by awaiting the agent request inside the prompt's spawned
  task and broadcasting `prompt_done` to every attached subscriber
  on completion (and an additional `error` event on failure
  ahead of the `prompt_done`). Peers that were not busy receive a
  no-op clear; the sender unlocks. Covered by a regression test
  that drives a fake agent which auto-replies to any request line
  and asserts both the user-prompt echo and `prompt_done` appear
  on the broadcast channel.

## [0.8.26] - 2026-05-29

### Fixed

- Multi-browser attach (stage 1) had three reachable bugs that
  shipped in 0.8.25 and surfaced as soon as we drove a real two-
  browser scenario.

  Browser B joining an existing session saw an empty chat. The hub
  cached the `ready` event from the first attach with `resumed: false`
  (the first browser was the one that ran `session/new`); subsequent
  attaches replayed that snapshot, so the client never took the
  `resumed: true` branch that fetches `/history`. Fixed by always
  rewriting `ready.resumed` to `true` when subscribing to a hub:
  every attach is functionally a join to a conversation that
  already exists from the agent's perspective, regardless of how
  the hub was first created. The `session/new`-vs-`session/load`
  distinction stays inside the negotiation phase where it matters.

  The same root cause meant a reload (in either browser) lost
  history: after the WS dropped, the new attach landed on the
  cached snapshot with `resumed: false` and the client never
  rehydrated. Same fix covers it.

  Browser B's typed prompt did not show up in browser A. The hub
  broadcast only carried agent output; the sender's prompt was
  rendered locally from `sendPrompt` and never reached peers. Fixed
  by having the hub echo every `Prompt` command back through the
  broadcast as an `append { role: user }` frame, and dropping the
  local `appendLog` from `sendPrompt` so the broadcast echo is the
  single source of truth. Sender and peers now see the same text
  in the same order. The round-trip is microseconds (broadcast in
  process, WS sink is local) so the sender notices no perceptible
  delay.

## [0.8.25] - 2026-05-29

### Added

- Multi-browser attach (stage 1 of 2). The cloudflared transport now
  runs a `SessionHub` per ACP session id, with a single owner task
  that holds the agent and a `tokio::sync::broadcast` channel that
  fans agent output to every attached WebSocket. Two browsers loaded
  on the same tab id share one agent: both see chunks, tool calls,
  permission requests, and prompt_done events from the same source.
  Closing one browser keeps the agent alive for a 30 second grace
  window; reattach within the window and the existing agent picks
  up. Browsers' user prompts are still rendered locally only in
  this stage (stage 2 will broadcast peer prompts so the chat
  timeline matches across browsers). Permission responses use
  first-wins semantics: the first reply reaches the agent, later
  replies for the same id drop silently.

  Plumbing: new `src/hub.rs` module with `HubRegistry`, `SessionHub`,
  `AttachedHub`, and the owner loop. `AppState` in `src/http.rs`
  bundles the live hub registry alongside `Config` so the WS
  handler can reach both. `handle_ws` is now a thin attach: subscribe,
  replay the cached `ready` snapshot, run a small select between the
  WS stream and the broadcast, drop `AttachedHub` on exit so the
  counter decrements. The agent itself, the negotiate phase, and
  the JSON-RPC framing are unchanged.

  Three new integration tests under `tests/hub.rs` cover the
  snapshot replay (every subscriber gets the same `ready`/`session_info`),
  the broadcast flow (agent updates fan out to every subscriber),
  and the first-wins permission semantics. The pre-existing
  `tests/ws_select_loop.rs` and `tests/ws_negotiate_session.rs`
  files still pass: the negotiation helper and the original
  select-loop are kept around as a public test surface.

## [0.8.24] - 2026-05-28

### Changed

- Code-block language pill now matches the copy button's height
  (24 px) and uses a brighter background. The previous pill was
  shorter than the copy button (`py-0.5`) and used `bg-card/70`,
  which was nearly invisible against the pre's `#0d1117` code
  area. Switched the height to `h-6` with flex centering so the
  pill and the copy button sit on the same baseline, and the
  background to `bg-muted` for an unmistakable separation from
  the code panel.

## [0.8.23] - 2026-05-28

### Changed

- Timestamps spell out their units. The previous compact form
  (`17 d ago`, `13 h ago`, `1 min ago`) is replaced with full words
  and proper pluralisation: `just now`, `1 minute ago`,
  `13 hours ago`, `17 days ago`, `2 weeks ago`, `2 months ago`,
  `1 year ago`, etc. Unit boundaries: minutes flip to hours at 60,
  hours to days at 24, days to weeks at 14, weeks to months at 9
  (about 63 days), months to years at 12. Months and years use the
  standard 30.44 / 365.25 day approximations. Three new tests
  under `tests/ui/time.test.ts` cover the new weeks, months, and
  years thresholds.

## [0.8.22] - 2026-05-28

### Changed

- The copy button on code blocks now sits to the left of the
  language pill and is permanently visible. It used to live in the
  top-right corner, fading in on hover. Two issues with that:
  touch-device users had no hover state and could not discover the
  button at all, and the right-aligned position pushed the button
  past the code on narrow viewports. Left-of-pill keeps it in the
  same row regardless of width and surfaces it without ceremony.

## [0.8.21] - 2026-05-28

### Added

- Sidebar is now resizable. Drag the right edge to widen or narrow
  the panel; the chosen width is persisted to localStorage on a
  per-browser basis so it survives reloads. Clamped between 192 px
  (min, so labels stay readable) and 480 px (max, so the chat
  pane is never crowded). The drag handle is a thin strip pinned
  to the right edge, hidden on mobile (where the sidebar is a
  drawer and a resize handle would not make sense). New default
  width 272 px gives the larger MEZAME wordmark room to breathe.

### Changed

- Typography refresh. Body and chrome now use JetBrains Mono with the
  same monospace fallback chain we had before; the sidebar wordmark
  uses Chelsea Market for a single beat of personality. Both fonts
  ship from Google Fonts via the standard preconnect plus a stylesheet
  link in `index.html`. The wordmark text changed from the kanji form
  to a roman `MEZAME` so it renders consistently regardless of the
  installed font set, with the Japanese form retained on the README
  banner only.

## [0.8.20] - 2026-05-27

### Fixed

- The copy button on syntax-highlighted code blocks now copies the
  full source instead of dropping every highlighted token. Bug
  surfaced on a CSS snippet: `.element { width: 100vw; ... }`
  pasted as `{: ;: ( - );}`. After `rehype-highlight` runs, the
  `<code>` element's children are a mix of plain strings (the
  whitespace and punctuation between tokens) and React elements
  (the `<span class="hljs-keyword">` tokens themselves). The
  flattener was joining only the strings, silently dropping every
  span. New `nodeToText` helper recursively walks the React tree
  and concatenates every text node. Regression test under
  `tests/ui/markdown.test.tsx` clicks the copy button and asserts
  the css source survives the round-trip.

- Markdown code blocks no longer paint the language pill on top of
  the first line of code. The pill and copy button were
  absolute-positioned over the top corners of the code area and
  overlapped both the syntax-highlighted text and (on hover) any
  long first line. Replaced the absolute positioning with a flex
  gutter row at the top of the block: language label on the left,
  copy button on the right, code element underneath. Code blocks
  without a language render the same gutter (with an empty left
  slot) so the copy button stays in the same position regardless.

### Changed

- Reworked the remembered-permission affordances on the permission
  card. The previous "Forget for this session" button appeared on
  every resolved card whenever any policy was active, with wording
  that read like a state ("this is forgotten") instead of an action.
  Now the originating card and any subsequent auto-resolved cards
  carry a `Remembered for this session` status badge plus a `Disable`
  button; cards that have nothing to do with a remembered policy
  stay clean. New per-title `forgetRememberedPermission` action
  clears just the policy that matches a given card, so Disable on
  one card no longer wipes every other remembered policy on the
  tab. Tickbox label rewritten to "Remember my choice and apply
  automatically next time" so users know what ticking it does.

## [0.8.19] - 2026-05-26

### Fixed

- CI's `ui-tests` job kept failing on `npm ci` even after the 0.8.13
  and 0.8.15 lockfile fixes. The cause was finally diagnosed: every
  release commit was running
  `npm install --include=optional --package-lock-only` to refresh
  the lockfile after a version bump, but `--package-lock-only`
  skips the install step and so never re-resolves the optional
  dependency tree. Each release silently dropped the cross-platform
  optional entries the previous full install had recorded; by 0.8.18
  they were gone. Regenerated the lockfile cleanly with a real
  install (no `--package-lock-only`) and added a sentinel grep to
  the pre-commit hook that fails the commit when the lockfile is
  missing the top-level `@emnapi/core` entry, with an error message
  pointing at the correct regenerate sequence.

## [0.8.18] - 2026-05-26

### Changed

- CI coverage floor lifted from 35 to 60. Actual coverage is now
  65.57 after the 0.8.16 and 0.8.17 test additions; the old
  threshold no longer caught regressions because a wipeout of
  nearly half the suite would have been needed to trip it. The new
  floor sits comfortably below the current figure: routine drift
  passes, a five-plus-point drop fails. The floor is a regression
  alarm, not a target.

## [0.8.17] - 2026-05-26

### Changed

- Extracted `ws::negotiate_session` from `handle_ws`. The ACP
  handshake, the resume-vs-new branch, the `ready` emission, and the
  optional `session_info` emission are now in a public (but
  `#[doc(hidden)]`) function returning a `NegotiationOutcome`.
  `handle_ws` reads as a thin attach: spawn agent, negotiate, run the
  select loop. Production wire shape identical. Five new integration
  tests under `tests/ws_negotiate_session.rs` drive the helper with a
  duplex-pipe fake agent (via `Agent::from_io`), covering the fresh
  path, the resume path with replay-suppression, the fallback after
  a non-recoverable resume error (sys notice plus new session), the
  missing-sessionId error, and the empty-promptCapabilities default.
  Test count went from 84 to 88 Rust tests across 17 files.

## [0.8.16] - 2026-05-26

### Changed

- New tests covering previously-uncovered surfaces in `config`,
  `agent::spawn_agent`, and the HTTP asset path. `config_path`,
  `state_path`, and `load_config` now have a HOME-driven tempdir
  suite (8 cases). `spawn_agent` is exercised against a real `cat`
  subprocess, confirming the stdin / stdout / reader-task wiring
  and the failure path for a missing binary (3 cases). The asset
  route tests gained the `sw.js` no-cache assertion and a default-
  short-cache assertion for top-level static files (3 cases). The
  `build.rs` SPA stub now writes `sw.js` and a `favicon.png` stub
  so the new tests have real bytes to fetch under
  `MEZAME_SKIP_UI_BUILD=1`. Three `pub(crate)` items widened to
  `pub` so integration tests can import them (`config_path`,
  `load_config`, `spawn_agent`); no behaviour change. Test count
  went from 70 to 84 Rust tests across 16 files.

## [0.8.15] - 2026-05-26

### Fixed

- Coverage job started failing after the cargo-llvm-cov switch in
  0.8.12 because the `tests/session_steal_stale_lock.rs` file had
  two latent bugs that tarpaulin's parallelism throttle had been
  hiding. First: the five tests in the file all mutate the
  process-global `HOME` env var, so cargo's default
  intra-binary parallelism let one test swap `HOME` mid-flight
  for another. Second: the dead-PID test used `i32::MAX` as a
  "definitely dead" sentinel, which macOS reports as ESRCH but
  some Linux runners do not, so `steal_stale_session_lock`
  refused to fire. Added a file-scoped `tokio::sync::Mutex` via
  `OnceLock` (same pattern `tests/http_routes.rs` uses) to
  serialise the HOME-mutating tests, and replaced the magic PID
  with a freshly-reaped child PID (`std::process::Command::new("true").spawn().wait()`)
  which both kernels agree is dead.

- CI's `ui-tests` job kept failing on `npm ci` with `@emnapi/core`
  and `@emnapi/runtime` missing from `ui/package-lock.json`, even
  after the 0.8.13 refresh. Cause: Tailwind's optional
  `@tailwindcss/oxide-wasm32-wasi` package pulls in
  `@napi-rs/wasm-runtime`, which depends on emnapi. A plain
  `npm install` on macOS picks the macos-arm64 oxide and never
  visits the wasi variant, so the emnapi entries never landed in
  the lockfile; Linux `npm ci` walks a different optional tree and
  demands them. Regenerated with `npm install --include=optional`
  so the lockfile is a cross-platform superset.

## [0.8.14] - 2026-05-26

### Changed

- Pre-commit hook now refuses to commit when `ui/package-lock.json`
  has drifted from `ui/package.json`. Runs `npm ci --dry-run` against
  the lockfile and reports the mismatch with the fix instructions
  (`npm install` in `ui/`). Catches the class of CI failure that
  triggered 0.8.13 before it leaves the developer's machine. Skipped
  silently when `ui/node_modules` has not been populated, same
  pattern the Vitest step uses.

## [0.8.13] - 2026-05-26

### Fixed

- CI's `ui-tests` job no longer fails on `npm ci`. The lockfile had
  drifted from `package.json`: a prior local install had pulled
  `@emnapi/core` and `@emnapi/runtime` in transitively without
  refreshing `package-lock.json`. CI's strict `npm ci` refused to
  install. Regenerated the lockfile so it matches the manifest;
  twelve missing entries are now in place.

## [0.8.12] - 2026-05-26

### Changed

- Coverage job switched from cargo-tarpaulin to cargo-llvm-cov. Tarpaulin
  was failing reliably on Linux despite every test binary it spawned
  reporting ok; the recent duplex-pipe async tests and `tokio::time::pause`
  usage tripped its instrumentation. llvm-cov uses LLVM's source-based
  coverage built into rustc, so it does not need a custom test harness,
  is faster, and emits lcov directly which Codecov v5 accepts natively.
  The 35% threshold carries over.

## [0.8.11] - 2026-05-26

### Changed

- Trimmed the published crate. The `exclude` list in `Cargo.toml` now
  drops `tests/**`, `.github/**`, `utilities/**`, `Dockerfile`,
  `compose.yaml`, and the Vitest config files. `cargo install mezame`
  users no longer download integration tests, CI workflows, or dev
  hooks they will never run. Local development is unaffected: the
  exclude only changes the published tarball.

### Fixed

- Composer no longer pins to "Agent is working..." after a long idle
  drop. Three compounding issues were behind it:
  1. `ws.onclose` flagged every disconnect as busy, even when no turn
     was in flight. Idle drops (laptop sleep, network blip, the macOS
     WebSocket dying quietly across hours of inactivity) left the
     session pinned to busy with no `prompt_done` coming to clear it.
  2. `ready { resumed: true }` after a successful reconnect did not
     clear the busy markers. The server suppresses Kiro's live replay
     during the resume window, so the historical `prompt_done` never
     landed.
  3. Reconnect was driven solely by the exponential back-off (capped
     at 30s), so returning to an idle tab could mean waiting half a
     minute for the next attempt instead of going through immediately.

  Fixes: a new `Session.inFlight` flag tracks whether a prompt is
  actually outstanding, so `ws.onclose` only flags busy when one is.
  `ready { resumed: true }` now clears `busy`, `thinking`, and
  `inFlight` as a safety net. A `visibilitychange` listener kicks any
  session sitting in `reconnecting` to retry immediately when the tab
  comes back into focus, dropping the worst-case "back to a stale UI"
  delay from 30s to under a second.

- Fenced code blocks render with their language pill and copy button
  again. The Markdown renderer's switch between fenced and inline code
  used `className.startsWith('language-')`, but `rehype-highlight`
  runs first and prepends `hljs` to the className, so fenced blocks
  arrived as `"hljs language-rust"` and fell through to the inline
  branch. Switched the test to `/(?:^|\s)language-\w/` which matches
  the language token wherever it sits in the class list. Side
  effects: code blocks now show the language pill (top-left), the
  copy-on-hover button (top-right), and the proper `pre` wrapper
  styling that was getting skipped before. Caught by the new
  Markdown smoke tests.

### Added

- "Remember for this session" tickbox on permission cards. When ticked,
  the chosen option is stored on the session keyed by the
  permission-request title; subsequent requests with the same title
  auto-resolve and fire the WS reply without UI. The auto-resolved
  card renders an `auto` indicator next to the resolution and a
  "Forget for this session" button that clears every remembered
  policy on the tab. Auto-resolutions deliberately do not raise the
  attention dot or favicon badge: the user already opted in.
  Session-local; never persisted across browser reloads. Three new
  reducer tests under `tests/ui/useMezame.test.ts` cover the
  auto-resolve path, the no-match path, and the existing
  prompt-the-user path.

- `mezame --version` and `mezame -V` print the version and exit
  cleanly, instead of falling through to the normal startup path.
  `mezame --help` and `-h` print a short usage block listing the
  `init` subcommand, the two flags, and the `MEZAME_DEBUG_ACP` /
  `MEZAME_SKIP_UI_BUILD` environment variables. Both flags short
  circuit before any config load or runtime build.

- Smoke tests for the `Markdown` renderer. Nine cases under
  `tests/ui/markdown.test.tsx` lock down the rendering shape across
  `react-markdown`, `remark-gfm`, `rehype-highlight`, and the small
  custom code-block wrapper: paragraphs wrap plain text, ATX
  headings produce `h1`/`h2`, inline code stays inline, fenced
  blocks carry a `language-*` class on the inner `code`, the
  language pill text is visible, fenced blocks without a language
  do not gain a `language-*` class, the copy button appears next
  to fenced code, GFM pipe tables render as tables with `<th>` and
  `<td>` populated, and external links land with `target="_blank"`
  and a `rel` containing `noreferrer`. A future `react-markdown`
  or `highlight.js` upgrade is gated by these tests passing.

- Pure-formatter tests for `lib/time.ts`. Thirteen cases under
  `tests/ui/time.test.ts` lock the wording `timeAgo` produces at every
  threshold (just now, 1 min, 59 min, 1 h, 23 h, 1 d, multi-day),
  confirm the singular noun does not gain an "s", clamp future
  timestamps to "just now", and assert `formatAbsolute` includes
  year, day, month token, and a hh:mm separator. Test inputs use
  explicit `now` arguments and locale-tolerant assertions so the
  suite is deterministic across hosts and time zones.

## [0.8.10] - 2026-05-26

### Fixed

- PDF and other non-image binary uploads no longer surface a misleading
  "Unrecognised file type." error when the agent has not advertised
  `embeddedContext`. The mime was always fine; the rejection was
  always about the missing capability. `fileToAttachment` now folds
  the trailing `unknown-type` branch into the existing
  `embed-not-supported` reason, so any non-image embedded file gets
  the same accurate message regardless of mime ("This agent does not
  accept embedded files."). The `unknown-type` variant of
  `RejectReason` is gone; tests for it removed in lockstep. Bonus:
  the file picker's `accept` attribute now narrows to `image/*` when
  the agent only advertises image support, so the OS dialogue does
  not even offer file types the agent will reject.

### Added

- HTTP route tests for the cloudflared transport. Nine cases under
  `tests/http_routes.rs` drive the real axum router via
  `tower::ServiceExt::oneshot`, no TCP port required: `GET /state`
  returns `{}` when the file is missing, `PUT /state` round-trips
  through `state_path()`, `GET /history` rejects missing and
  traversing session ids with `400`, returns `{ entries: [] }` for
  a missing fixture, and parses a real Kiro JSONL into the wire
  shape the UI expects (role, text, ms-precision timestamps that
  inherit forward from the most recent `Prompt`). The asset path
  asserts the cache headers we promise: long max-age + `immutable`
  for hashed `/assets/*`, `no-cache` for `index.html`, SPA fallback
  for unknown routes. Drives a new `pub fn build_router` extracted
  from `run_cloudflared` so production behaviour stays the same.
  Unblocked by a small `build.rs` tweak: when `MEZAME_SKIP_UI_BUILD=1`
  the build script now writes a stub `index.html` plus a hashed
  asset under `assets/` so `rust-embed` has real bytes for the
  routing tests in CI.

- Browser push notifications for background sessions. When a session
  that is not the active in-app tab transitions into an attention
  state (turn complete, permission requested, error), or any session
  does so while the whole Mezame tab is hidden, Mezame fires a
  desktop notification via the browser's `Notification` API. First
  use surfaces an inline banner asking the user to opt in; clicking
  "Enable" triggers `Notification.requestPermission()` and persists
  the choice. The `tag` field deduplicates rapid status changes so
  the OS replaces prior notifications instead of stacking them.
  Preference stored in `state.json` under `settings.notifications`
  (`unset` / `pending` / `on` / `off`). Requires a secure context
  (https or localhost), so plain-LAN bind addresses won't see the
  prompt; the favicon badge and attention dots still work in that
  case.

- Routing tests for `attachments::fileToAttachment` and the surrounding
  helpers. Sixteen cases across `tests/ui/attachments.test.ts` cover
  every accept/reject branch (images, text-resources, binary-resources,
  size cap), the rejection-message rendering, the `attachmentToBlock`
  base64/text reader paths, and the `cleanup` URL revocation. The PDF
  bug (#31) has both an expected-failing test for the desired
  behaviour and a passing test that locks in the current buggy
  behaviour, so the fix lands paired with the `it.fails` removal.

- Reducer tests for `useMezame`. Sixteen cases across
  `tests/ui/useMezame.test.ts` lock down the wire shape the UI
  expects from every server message: `ready` (with and without
  resume), `append` (merge by role, no merge across roles),
  `permission_request`, `mcp_oauth_request` (dedupe by id, fallback
  by serverName+url), `tool_call` (push and in-place update),
  `prompt_done`, `error`, `session_info` (full and partial), and
  `commands` (last-wins on re-emission). Drives a new pure
  `applyServerMessage(state, msg)` reducer extracted from
  `handleMessage`; side effects (build-id reload, history fetch)
  stay in `handleMessage` so the reducer is fully testable.

### Changed

- `useMezame.ts`: extracted the WS message switch into a pure
  `applyServerMessage` exported function. Production behaviour is
  unchanged; the change unlocks unit testing without React or a
  real WebSocket.

### Added

- UI test scaffolding: Vitest 4.1, jsdom 29, @testing-library/react
  16, @testing-library/jest-dom 6 and @testing-library/user-event 14.
  Tests live under `tests/ui/` to match the Rust convention; vitest
  config inherits `vite.config.ts` so the `@/` alias and Tailwind
  plugin work the same in tests as in production. New scripts:
  `npm test` (single pass), `npm run test:watch`, `npm run test:ui`.
  CI runs the suite in a dedicated `ui-tests` job; the pre-commit
  hook runs Vitest after the Rust steps when `ui/node_modules` is
  present.

- Branch coverage for `session::steal_stale_session_lock`. Four new
  cases under `tests/session_steal_stale_lock.rs` cover the
  failure-path returns: lockfile missing, lockfile not valid JSON,
  lockfile JSON without a `pid` field, and live PID (the test
  process's own pid, which must never be stolen). Each case asserts
  the lockfile is preserved when stealing was refused.

- Dispatch tests for `ws::handle_agent_message`. Sixteen cases under
  `tests/ws_handle_agent_message.rs` lock down the wire shape of every
  branch: `agent_message_chunk`, `user_message_chunk`,
  `agent_thought_chunk`, `tool_call` (with and without id),
  `tool_call_update`, `session/request_permission` (including the
  `name`-fallback for the title and the suppression-during-resume
  bypass), `_kiro.dev/commands/available` (asserts the `tools`
  catalogue is dropped), `_kiro.dev/mcp/oauth_request` (canonical
  fields, alternative field names, missing-URL drop), the
  resume-suppression toggle, and unknown-method silent drops.

- Retry/back-off tests for `session::try_load_session`. Five cases
  across `tests/session_try_load.rs` and `tests/session_steal_stale_lock.rs`
  cover the full state machine: first-attempt success,
  non-recoverable error breaks immediately, stale-lock error retries
  through to success, dead-PID lockfile is stolen and the retry
  succeeds, and the attempt budget is exhausted on a permanent
  stale-lock condition. Uses `tokio::time::pause` to fast-forward
  through the back-off so the suite stays under a second.

- JSON-RPC framing round-trip tests for `Agent`. Six cases under
  `tests/agent_jsonrpc.rs` exercise the wire bytes of `request`,
  `respond`, and `notify` end-to-end via `Agent::from_io` and a
  duplex pipe: happy-path request, error-response propagation,
  result frames, notification frames (no id), monotonic id
  allocation with out-of-order response routing, and the
  notification-fallthrough path that lands unmatched messages on
  the updates channel.

- WebSocket integration tests. New `Agent::from_io` constructor and an
  extracted `ws::run_select_loop` function let the per-session loop be
  tested with in-memory streams instead of a real subprocess. Five
  tests under `tests/ws_select_loop.rs` cover the disconnect paths
  that bug #32 missed: stream close (`None`), Close frame, transport
  error, agent exit, and a permission round-trip that confirms a
  browser reply lands on the agent's stdin.

### Changed

- Tests moved out of `#[cfg(test)] mod tests { ... }` blocks inside
  source files into the `tests/` directory. The functions they cover
  (`mime_for`, `parse_kiro_history`, `extract_text_blocks`,
  `is_stale_lock_error`, `extract_session_info`, `short_reason`,
  `pid_is_alive`, `send_signal`) are now `pub` so the integration
  tests can reach them through the crate's public API. Source files
  read cleaner; test surface area is identical.

### Added

- Test foundation. Crate is now split into `src/lib.rs` + a thin
  `src/main.rs` so integration tests in `tests/` can reach internals.
  25 unit tests cover the pure functions in `session.rs`, `http.rs`,
  and `unix.rs`: lock-recovery helpers, session-info extraction,
  Kiro JSONL history parsing, mime lookup, and the libc FFI
  bindings. The pre-commit hook now runs `cargo test` as a fourth
  check after fmt, clippy, and check. WS integration tests are
  tracked separately because they need a mockable agent.

## [0.8.9] - 2026-05-26

### Added

- MCP OAuth requests are now surfaced to the browser. When an MCP
  server emits `_kiro.dev/mcp/oauth_request`, Mezame forwards a
  `mcp_oauth_request` WS event and the UI renders an inline card with
  the server name, the auth URL, and an "Open" button. The button
  must be triggered by a user gesture (browsers block popups
  otherwise), so we never auto-open. Re-emissions are de-duped by
  request id, falling back to serverName + url when the agent did not
  include an id.

## [0.8.8] - 2026-05-25

### Changed

- `mime_for` now reads from a const lookup table instead of a chain of
  branches. Removes the per-request `to_ascii_lowercase()` allocation,
  keeps the extension-to-mime mapping in one place, and adds
  `webmanifest` to the known extensions.

## [0.8.7] - 2026-05-25

### Fixed

- WebSocket disconnect now reliably triggers cooperative agent shutdown.
  Previously, the select-loop branch matching browser frames used a
  `Some(Ok(...))` pattern guard. When the stream returned `None`
  (peer closed) or `Some(Err(_))` (transport error), the pattern did
  not match and tokio disabled the branch silently. The agent-updates
  branch stayed active, so the loop kept looping, never reached
  `else => break`, and `agent.shutdown()` never ran. Symptoms: leaked
  agent subprocess on every browser close during a long turn, stale
  Kiro session lockfile blocking the next `session/load`, and
  long-running servers accumulating orphans until OOM. Both branches
  now match the full `Option<Result<...>>` / `Option<Value>` and break
  out cleanly on close, error, or agent exit.

## [0.8.6] - 2026-05-25

### Changed

- `handle_ws` no longer duplicates the `session/new` plumbing across the
  resume-fallback arm and the no-resume arm. Both paths now go through
  a small `start_new_session` helper that owns the request, sessionId
  extraction, and `extract_session_info` call. Behaviour unchanged.
- `Agent::request`, `Agent::respond`, and `Agent::notify` now share a
  single `write_message` helper that owns the JSON-RPC framing (line
  serialise, lock stdin, write, flush). Removes three near-identical
  copies of the same dance and keeps the wire format in one place.
  Behaviour unchanged.
- Consolidated the duplicated `extern "C" { fn kill, fn setsid }`
  declarations from `agent.rs` and `session.rs` into a new
  `src/unix.rs` module exposing `send_signal` and `new_session` helpers.
  Removes drift risk between the two FFI blocks and keeps the unsafe
  surface in one place. Behaviour unchanged.
- `Agent` no longer hands the updates receiver out via a runtime-checked
  `take_updates` method. `spawn_agent` now returns
  `(Agent, UnboundedReceiver<Value>)`, making single-ownership of the
  channel a compile-time invariant. Removes two `expect()` panics and
  the `Mutex<Option<...>>` wrapper. Behaviour unchanged.
- HTTP handlers `/state`, `/history`, and the corresponding writes now
  use `tokio::fs` instead of the synchronous `std::fs` API. Removes a
  latent footgun where each request blocked a tokio worker thread for
  the duration of the I/O. Behaviour and response shape unchanged.

### Breaking

- **Project renamed from `okiro` to `mezame`** to avoid confusion with
  the AWS product of the same stem. User-facing wordmark now reads
  "Mezame!"; Japanese for "awakening" (目覚め). Knock-on renames:
  - Crate and binary: `okiro` to `mezame`. Reinstall via
    `cargo install mezame`.
  - Config and state directories: `~/.okiro/` to `~/.mezame/`. Existing
    users need to move `config.json` and `state.json` manually or run
    `mezame init`.
  - Environment variables: `OKIRO_DEBUG_ACP` to `MEZAME_DEBUG_ACP`,
    `OKIRO_SKIP_UI_BUILD` to `MEZAME_SKIP_UI_BUILD`. `KIRO_LOG_LEVEL`
    is unchanged (it belongs to the spawned Kiro CLI, not to us).
  - Build-time define (`__OKIRO_VERSION__` to `__MEZAME_VERSION__`),
    UI store hook (`useOkiro` / `okiroActions` to `useMezame` /
    `mezameActions`), UI package (`okiro-ui` to `mezame-ui`), and CSS
    keyframes (`okiro-pulse-orange` / `okiro-border-spin` to
    `mezame-pulse-orange` / `mezame-border-spin`) all renamed.
- README now carries an explicit disclaimer that the project is not
  affiliated with AWS or the Kiro product.

## [0.8.0] — 2026-05-08

### Breaking

- **Config format switched from TOML to JSON.** Config path moved from
  `~/.mezame/config.toml` to `~/.mezame/config.json`. Existing users are
  dropped into `mezame init` on next launch; the new config is written
  after interactive setup.
- **Transports are now a list.** The top-level `transport` enum and
  `bind` string are gone. Replaced by `transports: [{ "kind":
  "cloudflared", "bind": "..." }]`. Only one entry is accepted at
  runtime today; the list shape is forward-compatible with a future
  Telegram transport without another break.
- **Default port changed from 7842 to 9510.** Existing tunnels and LAN
  bookmarks need to be updated accordingly, or set a custom `bind` in
  the new config.

### Added

- **`cargo install mezame` support.** Crate now carries the metadata
  crates.io requires (`description`, `license`, `keywords`,
  `categories`, `repository`, `readme`, `exclude`). `build.rs`
  compiles the UI inside `$OUT_DIR` instead of the source tree so
  `cargo publish` verify passes.
- **Interactive init with arrow keys.** `mezame init` now uses
  `dialoguer` for the bind-address and agent menus. `init` also probes
  `$PATH` for known ACP CLIs (Kiro CLI, Claude Agent CLI, Gemini CLI,
  Codex) and offers them as a pick list; "Other" drops to a free-form
  prompt. Kiro CLI pre-fills `acp` as the subcommand.
- **Graceful shutdown on SIGTERM / SIGINT.** Mezame now stops accepting
  new connections and exits promptly when `systemctl stop` or
  `launchctl bootout` asks. Pairs with the new
  [service guide](./docs/service.md) covering systemd user and system
  units and a macOS LaunchAgent.

### Changed

- **Source layout.** `src/main.rs` split into six modules (`config`,
  `agent`, `session`, `http`, `ws`, plus a thin `main`). Behavioural
  change: none. Easier to navigate.
- **Dependency trim.** `toml` removed (6 transitive crates gone). axum
  dropped to `default-features = false` with only the features we
  actually use. `rust-embed`'s `debug-embed` swapped for
  `interpolate-folder-path`.
- **Docs split.** Architecture, wire protocol, Cloudflare setup,
  development, and the service guide all moved to `docs/`. README down
  from ~260 lines to ~130.

## [0.7.1] — 2026-05-08

### Added

- **Attachments in prompts.** Paste an image, drop a file on the
  composer, or click the paperclip to open a file picker. The server
  forwards the agent's `promptCapabilities` from `initialize` in the
  `ready` event, and the composer gates which file types it accepts
  (images only when the agent advertises `image`, resources only when
  it advertises `embeddedContext`). Per-attachment cap 5 MB, total 20
  MB, max 10 per message.
- **Tool-call cards.** Tool calls now render as a collapsible row in
  the chat: summary shows title and a status pill, expanding reveals
  arguments (JSON), output (markdown or JSON), and the file locations
  touched. Updates with the same `toolCallId` merge in place.
- **Favicon attention badge.** Background-tab attention (permission
  request, turn complete, error) paints a red numeric pill onto the
  favicon and prefixes `document.title` with `(N) `. Attention now
  also raises for the active in-app session when the whole Mezame
  browser tab is hidden, so you see a badge when a turn finishes
  while you're reading elsewhere. `visibilitychange` clears it on
  return.
- **Cwd chip in the composer.** Shows the working directory the
  session was started with, with the server-reported resolved path
  (not just the user-supplied override). Double-click to open a
  sibling tab at a different path.
- **Bind-address menu in `mezame init`.** Three options: loopback
  (default), `0.0.0.0` for trusted-LAN setups where cloudflared runs
  elsewhere, or a custom address. The LAN option prints an explicit
  warning that Mezame has no auth of its own today.

### Changed

- **UI deps refreshed.** TypeScript 5.7 to 6.0, Vite 6 to 8 with
  `@vitejs/plugin-react` 4 to 6, react-markdown 9 to 10, lucide-react
  0.475 to 1.14, `@types/node` 22 to 25; all other deps bumped to the
  current latest. TypeScript 6 required dropping `baseUrl` from the
  tsconfigs (moduleResolution "bundler" resolves `paths` relative to
  the tsconfig itself) and adding an ambient `declare module '*.css'`
  for side-effect CSS imports.
- **Header restyle.** Tab bar now renders as a floating card with
  rounded corners and the same primary-blue border as the composer at
  the bottom.
- **Telegram option removed from `mezame init`.** Picking it prints an
  informative "not yet implemented" message and re-prompts. The enum,
  config field, and stub remain for forward compatibility.
- **Prose sweep.** User-facing stderr lines, anyhow contexts, error
  messages to the browser, and init prompts rewritten as sentence case
  with em dashes replaced by colons. Init prompt accepts transport
  identifiers case-insensitively. New logos, new favicon, and a new
  "Why Mezame" section in the README positioning the tool against
  direct-to-provider gateways.

### Fixed

- `cargo build` on a clean checkout no longer requires the
  `cloudflared/` folder to exist; the example config is inlined in
  the README instead, and `ui/dist/` is gitignored as expected.
- Rebased a `build.rs` docstring typo and cleaned up stale comment
  blocks in `src/main.rs`.

### Known gaps

- Attachments in historical turns are not rehydrated on resume. The
  `/history` parser only handles text today; inspecting what Kiro
  records for image and resource blocks in its JSONL is a follow-up.

## [0.6.0] — 2026-05-07

### Changed
- Project renamed from `racp` to `okiro`. User-facing wordmark read
  "Okiro!" in the UI header and the window title. Japanese for "wake
  up!" (起きろ), which is what you do to a Kiro that is asleep
  somewhere else.
- Config and state directories moved from `~/.racp/` to `~/.okiro/`.
- Environment variables renamed: `RACP_DEBUG_ACP` became
  `OKIRO_DEBUG_ACP`, `RACP_SKIP_UI_BUILD` became `OKIRO_SKIP_UI_BUILD`.
- CSS keyframes (`racp-pulse-orange`, `racp-border-spin`), build-time
  define (`__RACP_VERSION__`), UI store hook (`useRacp` /
  `racpActions`), and UI package name (`racp-ui`) all renamed to their
  `okiro` equivalents.

## [0.5.6] — 2026-05-07

### Changed
- Agent / Model selector trigger text is brighter: the current value
  uses `--foreground` now (was `--muted-foreground`) and the
  "Agent:" / "Model:" label prefix bumped from `/70` opacity to
  full `--muted-foreground` for a bit more contrast.
- Composer border uses a 60%-opacity tint of the send button's
  primary blue. Gives the floating card a clearer outline against
  the dark log pane.

## [0.5.5] — 2026-05-07

### Changed
- Single shared centred column at 1440px: TabBar and the chat area
  now live inside it as siblings, so the tab bar fills its parent
  with a plain `w-full` instead of duplicating the max-width
  constant.

## [0.5.4] — 2026-05-07

### Changed
- Content column max width bumped to 1440px. Tab bar synced so the
  header and the chat pane stay aligned.

## [0.5.3] — 2026-05-07

### Changed
- UI content column is now centred with a 1100px max width. Prevents
  the chat from spreading across the full viewport on ultrawide
  displays while still filling regular laptop and desktop screens.
  Tab bar, log pane, and floating composer all share the same column.

## [0.5.2] — 2026-05-07

### Changed
- Agent and Model selectors are back inline (side-by-side) inside the
  composer's bottom-right, wrapping only if the composer gets
  unusually narrow. Textarea bottom padding reduced accordingly.

## [0.5.1] — 2026-05-07

### Removed
- In-UI cancel button is gone. While the agent is working the send
  button is disabled and the textarea becomes read-only with a
  "agent is working..." placeholder. The WS-level `cancel` path is
  still in place in the store for future keyboard shortcut or
  command-palette use.

## [0.5.0] — 2026-05-07

### Changed
- Composer is now a floating card pinned to the bottom of the chat
  pane. The log pane spans the full height of `<main>`; the composer
  overlays it with `bg-background/70` plus `backdrop-blur-md`, so the
  latest message shows through faintly as you type. Rounded corners,
  drop shadow, 12px inset from the pane edges.
- Send button moved to the top-right corner of the composer.
- Agent and Model selectors moved to the bottom-right corner inside
  the composer (still stacked vertically).
- Standalone cancel button removed. While the session is busy the
  send button swaps to a cancel button in the same slot; clicking it
  posts `session/cancel`. Placeholder text also swaps to "agent is
  working..." during a turn.
- Textarea has no visible border any more (the floating card owns the
  chrome) and auto-grows 2–8 rows as before.

## [0.4.1] — 2026-05-07

### Changed
- Composer is a two-column layout now: message textarea fills column 1,
  Agent and Model selectors stack vertically in column 2.
- Send and Cancel icon buttons moved inside the textarea (bottom-right
  corner) so the composer feels like one integrated field. Extra right
  padding on the textarea keeps typed text clear of the buttons.
- "Agent" and "Model" labels capitalised in the selectors.

## [0.4.0] — 2026-05-07

### Changed
- Composer redesign. Input is now a multiline `textarea` (starts at two
  rows, auto-grows up to eight, scrolls thereafter). Enter sends,
  Shift+Enter inserts a newline.
- Cancel and Send are icon-only buttons (ban, send-arrow) with
  tooltips.
- Agent and model dropdowns moved from their dedicated header row into
  the composer's bottom toolbar, so all input controls sit together.
  Header is one row taller but the chat pane now has more vertical
  space on short viewports.
- Mode/model selectors hide themselves entirely when the agent
  advertises neither (non-Kiro agents).

## [0.3.5] — 2026-05-07

### Changed
- Busy-tab border thickness bumped from 1px to 2px while the travelling
  glow is running, so more of the rotating colour is visible. Padding
  trimmed by 1px on each side so the overall tab footprint stays
  identical to neutral tabs.

## [0.3.4] — 2026-05-07

### Fixed
- Busy-tab travelling border no longer bleeds across the label. The
  previous version used a 180-degree bright arc plus a translucent
  inner fill, which let the gradient's bright centre peek over the
  short edges and ride across the text as it rotated. Opaque inner
  fill now, and the conic gradient is mostly transparent with a
  narrow (~45-degree) bright arc, so a discrete glow moves around
  the outline instead of a rotating half-disc.

## [0.3.3] — 2026-05-07

### Changed
- Busy-in-background tabs: the pulsing green fill is replaced by a
  travelling green highlight around the tab border. Uses an animated
  `@property --busy-border-angle` plus a layered background (opaque
  inner fill + conic gradient clipped to the border box). More
  distinctive than the pulse and less visually noisy for tabs that are
  running longer turns.

## [0.3.2] — 2026-05-07

### Fixed
- Permission-request dot wasn't appearing on busy-in-background tabs.
  The dot was gated to `connected` only; permissions always arrive
  while the tab is `busy` (awaiting the user's click) so the new
  green pulse was hiding the orange permission signal. Dot now rides
  on top of the pulse for both `connected` and `busy-background`
  states.

## [0.3.1] — 2026-05-07

### Fixed
- Tab pulse, for real this time. Dropped the `color-mix` + custom-
  property + Tailwind-utility-class indirection and just hardcoded
  two plain keyframes (`mezame-pulse-orange`, `mezame-pulse-green`), then
  attached them to the tab via an inline `style={{ animation: ... }}`
  so nothing in the cascade can flatten them. Connecting/Reconnecting
  now pulse orange; busy-in-background pulses green.

## [0.3.0] — 2026-05-07

### Added
- Tabs now pulse **green** when a turn is still running in the
  background (busy, not the active tab). Precedence on the tab colour
  is now: error > connecting/reconnecting > busy-in-background >
  connected.
- Status verbs in the tab tooltip expanded to include "Working..." for
  the busy-background state.

### Changed
- The attention dot moved to the left of the tab title so it reads as a
  leading indicator rather than trailing after the name.

### Fixed
- Connecting/Reconnecting tabs actually pulse orange now. The previous
  keyframe was living inside a Tailwind v4 `@layer utilities` block
  that was flattening the animation in production. Moved the pulse
  classes and keyframe to top-level CSS and parametrised the colour
  via a `--pulse-color` custom property so the same animation drives
  both the orange connecting pulse and the new green busy pulse.

## [0.2.2] — 2026-05-07

### Fixed
- Connecting/Reconnecting tabs now visibly pulsate orange. The keyframe
  was being flattened by the tab's `transition-colors` and the
  low-opacity mix made it invisible even when animating. Intensity
  bumped (28-60% of the permission orange) and `transition-colors`
  suppressed while pulsing.

## [0.2.1] — 2026-05-07

### Fixed
- Attention dot on tabs is now legible against any tab background
  (including the matching "Connected" green). Bigger, with a
  background-coloured outline ring and a subtle shadow so the
  semantic fill colour stays the signal without blending in.

## [0.2.0] — 2026-05-07

### Changed
- Connection status moved from an in-log pill into the tab itself.
  Tabs now carry a subtle coloured background: green for Connected,
  pulsing orange for Connecting / Reconnecting, red for Disconnected.
  The attention dot still appears on non-active Connected tabs.
- Status verbs surface on hover via the tab tooltip.

## [0.1.0] — 2026-05-07

Baseline. Everything that landed up to this point is folded into 0.1.0.

### Core
- Rust bridge from a browser WebSocket to a locally spawned ACP agent
  (Kiro CLI or any stdio-ACP agent) over JSON-RPC 2.0.
- Config at `~/.mezame/config.toml`, interactive `init` subcommand.
- Cloudflared transport, axum HTTP + WebSocket on `127.0.0.1:7842`.
- Telegram transport stubbed.

### UI (React + Vite + Tailwind v4 + shadcn)
- Multi-tab chat. One tab = one WS = one Kiro subprocess = one ACP session.
- Cross-device state persisted via `GET/PUT /state` (`~/.mezame/state.json`).
- Session resume on reconnect via `?session=<id>` and `session/load`.
- Per-tab `cwd` override via `?cwd=...`.
- Permission prompts rendered as inline cards, reply carries selected `optionId`.
- Tool calls surface as `[title — status]`, thinking chunks as `(thinking)`.
- Reconnect with exponential backoff (max 30s), scroll pinning,
  per-tab attention dots (`done` / `permission` / `error`).
- Cooperative Kiro shutdown (`session/cancel` + stdin EOF + 500ms wait)
  so lockfiles are released cleanly.
- Stale-lock recovery on `session/load` failure: PID liveness check, lock
  steal when the PID is dead, retry loop for the reload race (~1.25s
  budget, 250ms spacing).

### Markdown and chat layout
- Agent turns rendered with `react-markdown` + `remark-gfm` +
  `rehype-highlight` + `github-dark` highlight.js theme, plus KaTeX
  math (`remark-math` + `rehype-katex`).
- Chat-style bubbles: user on the right in a tinted bubble, agent full
  width below, system lines centred, permissions left-aligned.
- Per-message copy button (user and agent) and hoverable absolute
  timestamp (tooltip carries the exact time).
- History rehydration on resume: mezame reads Kiro's `<id>.jsonl` event
  log and serves it at `GET /history?session=<id>` with real per-turn
  timestamps. Server-side suppression of Kiro's live replay during the
  resume window to avoid double rendering.

### Kiro extensions
- `_kiro.dev/commands/available` forwarded as a `commands` event for
  slash-command autocomplete above the input.
- `modes` and `models` parsed from `session/new` / `session/load` and
  surfaced as per-tab picker selectors just below the tab bar. Mode
  and model changes go back over `session/set_mode` and
  `session/set_model` respectively.

### Layout niceties
- Header: History and New-session buttons on the far left; new tabs
  appear leftmost. `Mezame!` wordmark + version string on the far right.
- Mode and model selectors in a secondary row below the tab bar, shown
  only when the active session has populated them.

### Developer ergonomics
- `MEZAME_DEBUG_ACP=1` dumps every inbound ACP line to stderr prefixed
  with `[acp<-]`, useful when wiring new Kiro extensions.
- `MEZAME_SKIP_UI_BUILD=1` skips the Vite build in `build.rs` for
  Rust-only iterations (developer owns refreshing `ui/dist`).
