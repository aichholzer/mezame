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

## [Unreleased]

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
