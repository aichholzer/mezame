# Okiro! backlog

Feature backlog, grouped by theme. Sizes are rough: *afternoon* / *day* / *weekend*.

All UI-only items live in `src/ui.html`. Server-touching items note the function in `src/main.rs`.

## Readability

### 1. Markdown rendering in agent output

- **Status:** done (2026-05-07). Shipped with `react-markdown` + `remark-gfm` + `rehype-highlight` + `highlight.js` github-dark theme. Agent turns render markdown, code fences get language pills and syntax highlighting, streaming re-parses per chunk (brief mid-fence flicker is accepted tradeoff). Follow-up if bundle size matters: pin `rehype-highlight` to explicit languages instead of auto-detect.
- **Size:** afternoon.
- **Where:** `src/ui.html`, the path that currently appends `agent`-role text spans into the log.
- **What:** replace plain-text append with a markdown-to-HTML pass. Render headings, lists, bold/italic, inline code, and fenced code blocks. Add syntax highlighting for fenced blocks (start with `highlight.js` via CDN, or hand-roll for the handful of languages we care about: rust, js, ts, bash, json, toml, md).
- **Gotchas:**
  - Kiro streams markdown in chunks. A naive per-chunk render will flicker and break half-open code fences. Buffer by turn, or maintain a running incremental renderer per agent message.
  - Escape first, then render. Never pass raw agent output to `innerHTML`.
  - Keep the terminal aesthetic: monospace body, code blocks styled as subtle panels, no heavy typography.
- **Acceptance:**
  - Fenced code blocks render with language tag and highlighting.
  - Inline code, bold, italic, lists, and headings render correctly.
  - Streaming feels smooth, no visible flicker mid-chunk.

### 2. Copy button on agent messages and code blocks

- **Status:** done (2026-05-07). Reusable `CopyButton` component with `navigator.clipboard` plus textarea fallback. Visible on hover for each fenced block (top-right of the block) and each agent message (in the hover-revealed meta row). Brief "copied" state via check icon.
- **Size:** afternoon (falls out of #1).
- **Where:** the markdown renderer from #1 wraps each `<pre><code>` in a container with a copy button. Optionally a copy-whole-message button on agent turns.
- **What:** hover reveals a small button in the top-right of each code block. Click copies to clipboard via `navigator.clipboard.writeText`, brief "copied" state.
- **Gotchas:** clipboard API needs a secure context; Cloudflare Access origin is HTTPS so fine in production. Locally over `http://127.0.0.1` it works in most browsers; fall back to a hidden textarea + `execCommand` if needed.

### 3. Expandable tool calls

- **Status:** done (2026-05-08). Server forwards the full ACP `tool_call` / `tool_call_update` payload as a structured `tool_call` WS event keyed by `toolCallId`. Browser merges updates into the existing log entry in place. `ToolCallCard` renders a collapsible row: summary shows title and a status pill; expanded reveals arguments (JSON), content (markdown), and file locations, each with a copy button where useful. Rows with no expandable detail collapse to the summary only.
- **Size:** afternoon-to-day.
- **Where:** `handle_agent_message` in `src/main.rs` for the `tool_call` / `tool_call_update` variants, and the `sys`-line rendering in `src/ui.html`.
- **What:** today the `[title — status]` line is plain text. Keep it as a summary row, but forward the full payload (arguments, locations, content, raw output) to the browser as structured JSON. The row becomes clickable; expanded state shows arguments, output preview, locations touched, and elapsed time.
- **Server change:** extend the `append`-style event we emit into a new event kind, e.g. `{ "type": "tool_call", "id": ..., "title": ..., "status": ..., "arguments": ..., "content": ..., "locations": ... }`, with `tool_call_update` merging into the existing row by id.
- **UI change:** track tool-call rows by id, mutate in place on updates, render an `aria-expanded` disclosure.
- **Polish:** colour the row by status (running, completed, failed).

### 4. Message timestamps

- **Status:** done (2026-05-07). Each text/permission log entry stamps `timestamp` at receive time. Hovering a user or agent entry reveals a muted "just now / N min ago / N h ago / N d ago" label; absolute timestamp in the `title` tooltip. Shared `useTick` hook (`useSyncExternalStore`-backed, single 30s interval) refreshes relatives app-wide.
- **Size:** afternoon.
- **Where:** `src/ui.html`. No server change; stamp client-side on receipt.
- **What:** each user and agent turn gets a timestamp attribute. Relative on hover ("3 min ago"), absolute in the `title` tooltip. Tick a single interval every 30 s to update relatives.
- **Gotchas:** do not re-render content, just update a small label element. Keep the terminal look.

## Notifications and ambient signal

### 5. Browser push notifications on background-tab completion

- **Size:** afternoon.
- **Where:** `src/ui.html`, alongside the attention-dot logic.
- **What:** when `prompt_done` (or `permission_request`) fires for a tab that is not the active tab, fire a `new Notification(...)`. Reuse the existing attention signal; this is an additional channel, not a replacement.
- **Flow:** first use asks for permission via a small in-UI prompt; persist the choice in `state.json` under a new `settings.notifications` key.
- **Gotchas:**
  - Requires HTTPS (fine behind Cloudflare Access).
  - Mobile Safari support is spotty; degrade gracefully.
  - Avoid double notifications when two browsers are open on the same session; a visibility check per browser is enough.

### 6. Favicon badge for total attention count

- **Status:** done (2026-05-08). `useAttentionBadge` hook subscribes to the session store, counts background sessions with attention, paints a red numeric pill onto the favicon and prefixes `document.title` with `(N)`. Base favicon lives at `ui/public/favicon.png`. Attention now also raises for the active in-app session when the whole Okiro browser tab is hidden (was: only raised for background in-app tabs), so you see a badge when a turn completes while you're reading elsewhere. Visibility change on the Okiro tab clears the active session's attention.
- **Size:** half an afternoon.
- **Where:** `src/ui.html`. Purely client-side.
- **What:** compute the sum of tabs with attention dots. If > 0, paint a small badge into a canvas, draw the base favicon underneath, set `link[rel=icon].href` to the canvas data URL. If 0, restore the base favicon.
- **Gotchas:** cache the base image once; don't re-decode every tick.

### 7. Optional sound on turn complete and permission prompt

- **Size:** quick.
- **Where:** `src/ui.html`. Setting lives in `state.json` under `settings.sounds`.
- **What:** two small WAV/OGG assets (or a synth blip via `AudioContext` to avoid bundling). Off by default. Two separate toggles: "turn complete" and "permission prompt", since the latter is higher urgency.
- **Gotchas:** browsers block audio until a user gesture; the first play after load may need to be primed. Do not play for the active tab; only for background or on permission prompts.

## Power-user ergonomics

### 8. Command palette (Cmd+K)

- **Size:** day.
- **Where:** `src/ui.html`. No server change.
- **What:** modal with a text input and a fuzzy-ranked list of actions:
  - Switch to tab *X*
  - New tab (optionally with cwd)
  - Rename current tab
  - Close current tab
  - Restore from history
  - Export current session as markdown (see #10)
  - Toggle notifications, sounds
- **Gotchas:** Cmd+K is owned by the browser address bar on some setups; also bind Ctrl+K for Linux/Windows. Provide Esc to close, arrow keys to navigate, Enter to select.

### 9. Keyboard shortcuts for tabs

- **Size:** afternoon.
- **Where:** `src/ui.html`.
- **What:** Alt+1..9 to switch to the Nth tab, Alt+W to close current, Alt+N for new tab, Alt+Shift+Left/Right to move tab. Ctrl+Enter to submit (document existing behaviour).
- **Gotchas:** avoid Cmd+T, Cmd+W, Cmd+N; browser owns those. Document the full set in a `?`-triggered help overlay.

### 10. Export session as markdown

- **Size:** afternoon.
- **Where:** `src/ui.html` if we export from the live DOM; `src/main.rs` if we want history-tab exports that were never loaded in this browser.
- **What:** button in the tab context menu and in the history menu. Produces a `.md` with a front-matter header (tab label, cwd, session id, timestamps) and user/agent turns. Includes tool-call summaries; optional verbose mode includes full tool payloads (depends on #3).
- **Gotchas:** for history entries we'd need to read from Kiro's own session storage at `~/.kiro/sessions/cli/`, or limit export to currently-loaded tabs only in v1.

## ACP and Kiro-specific

### 11. Slash commands surfaced from `_kiro.dev/commands/available`

- **Status:** done (2026-05-07). `handle_agent_message` in `src/main.rs` forwards the notification as a `commands` WS event (with a trimmed payload: just `commands` + `prompts`, the giant `tools` catalogue is dropped). The browser surfaces it as an autocomplete popover above the input that opens on `/`; Enter/Tab commits `/name ` into the input, Enter again submits. See `ui/src/features/SlashAutocomplete.tsx`.
- **Size:** day.
- **Where:** `handle_agent_message` in `src/main.rs` to stop ignoring the `_kiro.dev/*` extensions; forward command-list updates to the browser. `src/ui.html` to render the autocomplete.
- **What:** on `_kiro.dev/commands/available`, cache the list per session. When the user types `/` at the start of the input, show an autocomplete menu (filtered as they type). Enter inserts the command; Esc dismisses.
- **Gotchas:** commands may have parameters and descriptions; surface the description below each row. The list can change mid-session (workspace loads, MCP servers register); update on every notification.

### 12. Agent mode and model switch per tab

- **Status:** done (2026-05-07). `session/new` and `session/load` results carry `modes` and `models`, and `okiro` forwards them as a `session_info` WS event right after `ready`. Browser sends `set_mode`/`set_model` back, which okiro translates into ACP `session/set_mode`/`session/set_model`. UI lives in `ui/src/features/ModeModelSelectors.tsx` and renders as two dropdowns in a secondary header row below the tab bar. Disabled while the session is busy.
- **Size:** day.
- **Where:** `src/main.rs` to send `session/set_mode` / `session/set_model` and to forward the current mode/model and available options to the browser. `src/ui.html` for the selector UI.
- **What:** small selector in the tab header or status bar. Shows current mode (e.g. autopilot / supervised) and current model. Clicking opens a dropdown of allowed values reported by the agent.
- **Gotchas:** availability and wording are Kiro-specific; feature-gate on capabilities returned by `initialize`. A mode change mid-turn may need to wait for turn end.

### 13. MCP OAuth URL handling

- **Size:** afternoon.
- **Where:** `handle_agent_message` in `src/main.rs` for `_kiro.dev/mcp/oauth_request`; `src/ui.html` for the card.
- **What:** today this notification is silently dropped. Render an inline card: "MCP server *X* wants you to authorise at *URL*. Open." Clicking opens the URL in a new tab. The agent completes the OAuth dance out-of-band; we just need to surface the request.
- **Gotchas:** do not auto-open popups; always require a user click to satisfy browser popup blockers. If the same request fires twice, de-duplicate by a server-provided id.

## File and media in prompts

### 14. Paste and drop images

- **Size:** day.
- **Where:** `src/ui.html` for paste/drop handling; `src/main.rs` to forward as ACP image blocks in `session/prompt`.
- **What:** accept clipboard paste (`paste` event, look for image/*) and drag-drop onto the input area. Show a thumbnail chip above the input with a remove button. On submit, send the image as an ACP prompt content block (`{ type: "image", mimeType, data }`).
- **Gotchas:**
  - Base64-encoded images inflate by ~33%; cap per-image size and/or count.
  - Only enable if `promptCapabilities.image` is true from `initialize` (Kiro advertises this).
  - State persistence: pasted but not-yet-sent images don't need to survive a reload.

### 15. @-mention files from cwd

- **Size:** day, plus a small server endpoint for file listings.
- **Where:** new route in `src/main.rs` (`GET /fs?cwd=...&q=...`) that fuzzy-matches files under the session's cwd. Autocomplete UI in `src/ui.html`.
- **What:** typing `@` opens a fuzzy picker of files under the session's cwd. Selecting inserts a reference (e.g. the relative path wrapped in backticks or as an ACP resource link, whichever Kiro understands best).
- **Gotchas:**
  - Hard guardrail: the endpoint must refuse paths outside the requested cwd after symlink resolution. Traversal risk.
  - Respect `.gitignore` and skip `node_modules`, `.git`, `target`, etc.
  - Cap result count; 50 is plenty.

## Security and hardening

### 16. Cloudflare Access JWT validation

- **Size:** half a day including JWKS caching.
- **Where:** `ws_upgrade` (and any other public route) in `src/main.rs`.
- **What:** validate `Cf-Access-Jwt-Assertion` before allowing the WS upgrade. JWKS lives at `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`. Verify the signature, `aud`, `iss`, `exp`. Cache the JWKS with a reasonable TTL (15 min) and refresh on signature miss.
- **Config:** add `access.team_domain` and `access.aud` to `~/.okiro/config.toml`. Optional; when unset, skip validation (local-only mode).
- **Gotchas:**
  - Bypass the check when the request is from `127.0.0.1` so local smoke tests still work.
  - Add the crates we need: `jsonwebtoken`, `reqwest` (or reuse something we already have to avoid bloat; check tls features carefully).
  - Log the authenticated identity (email) into the session log for audit.

### 17. Per-tool allow/deny policy within a session

- **Size:** afternoon.
- **Where:** `src/main.rs` permission-request path; `src/ui.html` for the "remember for this session" checkbox on the permission card.
- **What:** if the user ticks "remember for this session" on a permission card, store the chosen `optionId` keyed by tool id (or tool name) for the lifetime of the session. Subsequent identical requests auto-reply without UI.
- **Scope:** session-local only in v1. Persisting across sessions is a separate, thornier decision (policy file, audit, revoke UX).
- **Gotchas:**
  - Define what "identical" means precisely: probably tool name plus a hash of arguments, or just tool name for the simple path.
  - Give the user an obvious way to clear the remembered set (command palette action or tab context menu).

## Mobile

### 18. Compact mobile layout

- **Size:** afternoon.
- **Where:** `src/ui.html` styles only.
- **What:**
  - Tab bar scrolls horizontally on narrow viewports.
  - Input row sticks above the on-screen keyboard (`env(keyboard-inset-height, 0)` where supported, `100dvh` elsewhere).
  - Touch-friendly hit targets (min 44 px).
  - History and settings menus collapse into a drawer.
- **Gotchas:** iOS Safari viewport quirks are legendary; test on a real device, not just dev tools. Watch for double-tap zoom on buttons.

## Priority shortlist (value per hour for a daily user)

Top picks from what's left:

1. Browser notifications (#5)
2. Command palette (#8)
3. MCP OAuth URL handling (#13)
4. Cloudflare Access JWT validation (#16)

Completed: #1, #2, #3, #4, #6, #11, #12.
