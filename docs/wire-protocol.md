# Wire protocol

Mezame speaks two protocols: JSON text frames to the browser over `/ws`, and ACP (JSON-RPC 2.0) to the agent over stdio. This document is for contributors or anyone plumbing a new client. The main README covers the higher-level architecture.

## Browser to Mezame (JSON text frames over `/ws`)

```json
{ "type": "prompt", "text": "hello" }
{ "type": "prompt", "blocks": [{ "type": "text", "text": "look at this" },
                               { "type": "image", "mimeType": "image/png", "data": "iVBOR..." }] }
{ "type": "cancel" }
{ "type": "permission_response", "id": <original id>, "optionId": "allow_once" }
{ "type": "set_mode", "modeId": "kiro_planner" }
{ "type": "set_model", "modelId": "claude-sonnet-4.5" }
```

`prompt` accepts either a legacy `text` string (wrapped into a single text block on the server) or a full ACP `blocks` array. The server forwards blocks unchanged, so the agent is the one that validates types against its own capabilities.

## Mezame to browser

```json
{ "type": "ready", "sessionId": "<uuid>", "resumed": true | false, "cwd": "<path>",
   "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true } }
{ "type": "session_info", "info": { "modes": { "currentModeId", "availableModes": [...] },
                                    "models": { "currentModelId", "availableModels": [...] } } }
{ "type": "commands", "commands": [...], "prompts": [...] }
{ "type": "append", "role": "user" | "agent" | "sys", "text": "..." }
{ "type": "tool_call", "toolCallId": "...", "title": "...", "status": "in_progress" | "completed" | "failed" | ...,
                       "kind": "...", "rawInput": {...}, "content": [...], "locations": [...] }
{ "type": "prompt_done" }
{ "type": "permission_request", "id": <original id>, "title": "...", "options": [...] }
{ "type": "error", "message": "..." }
```

Details:

- `ready` fires once per (re)connect. `resumed: true` means the browser should clear the active log and seed from `/history`. The browser persists the `sessionId` so the next reconnect can pass `?session=<id>`. `promptCapabilities` is the agent's `initialize`-time advertisement (see [ACP protocol](https://agentclientprotocol.com/protocol/content)); the composer uses it to gate paste/drop/upload affordances.
- `session_info` arrives immediately after `ready` whenever Kiro reported `modes` / `models` on `session/new` or `session/load`. Drives the mode and model selectors in the header.
- `commands` forwards Kiro's `_kiro.dev/commands/available` catalogue (commands + prompts only; the massive tool catalogue is stripped on the server to keep WS frames light). Drives the `/` autocomplete.
- `append` is the ACP streaming path for a live turn. During a resume window the server suppresses these so the browser's `/history`-seeded log doesn't get duplicated.
- `tool_call` carries the full ACP tool-call payload (arguments, content, file locations). Updates for the same `toolCallId` merge into the existing UI row in place rather than appending.
- `permission_request` renders an inline card with one button per option; the user's click returns a `permission_response` with the matching `optionId`, which Mezame forwards to the agent to unblock it.
- `cancel` triggers `session/cancel` on the agent.

## Cross-device UI state

`GET` / `PUT /state` persists the open-tabs list, recently-closed history, active tab, and numeric label counter. Backing file is `~/.mezame/state.json`. Any browser hitting this Mezame sees the same list, useful when moving between devices behind the same tunnel. Actual conversation content stays with the agent (Kiro at `~/.kiro/sessions/cli/`); Mezame only stores labels, cwds, and ACP session ids.

## Mezame to agent (stdio, ACP JSON-RPC 2.0)

Requests Mezame sends:

- `initialize` on spawn.
- `session/new` when the browser opens a fresh tab.
- `session/load` when the browser reconnects with `?session=<id>`. With recovery: retries the load with 250 ms back-off while the error is "Session is active in another process", stealing the lockfile whenever the holding PID is dead. Covers the browser-reload race and the stale-lock case after a dirty shutdown.
- `session/prompt` for each user message.
- `session/cancel` (notification) for explicit cancel and during cooperative shutdown.
- `session/set_mode`, `session/set_model` when the user picks a new value in the header.

Notifications Mezame handles:

- `session/update`:
  - `agent_message_chunk` to `append` with `role: "agent"`. Rendered as markdown in the chat pane.
  - `user_message_chunk` to `append` with `role: "user"` during `session/load` replay (suppressed when we're seeding from `/history`, see above).
  - `agent_thought_chunk` to `append` with `role: "sys"` and a `(thinking)` prefix. Kiro itself does not emit these today; reasoning-model agents do.
  - `tool_call` / `tool_call_update` forwarded as a structured `tool_call` event keyed by `toolCallId`. Updates mutate the existing entry in place on the browser, so one tool call equals one collapsible row regardless of how many update notifications it emits.
- `session/request_permission` to `permission_request` event for the browser.
- `_kiro.dev/commands/available` to `commands` event (trimmed to `commands` + `prompts`).

Other `_kiro.dev/*` notifications (MCP OAuth URL, compaction status, clear status) are currently ignored. Extension points live in `handle_agent_message` in `src/ws.rs`.
