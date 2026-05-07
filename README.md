# racp

A small Rust bridge that puts a remote UI in front of a locally running ACP
agent (Kiro CLI, Claude Agent CLI, Gemini CLI, Codex, or anything else that
speaks the [Agent Client Protocol](https://agentclientprotocol.com) over
stdio).

## What it does, in one paragraph

`racp` is an **ACP client**. When a browser connects, `racp` spawns your
configured agent binary as a child process and speaks JSON-RPC 2.0 with it
over stdio. User messages from the browser become `session/prompt` requests;
`session/update` notifications from the agent stream back to the browser as
terminal-style text. The agent keeps its own credentials and model choice;
`racp` carries none.

## Architecture

```
  browser  <--WS-->  racp (this crate)  <--stdio JSON-RPC-->  agent
                      |                                        (kiro-cli acp,
                      |                                         claude,
                      |                                         gemini,
                      |                                         codex, ...)
                      |
                  transport layer
                  (cloudflared today, telegram TODO)
```

- One browser WebSocket connection = one `racp` session = one freshly spawned
  agent subprocess = one ACP session.
- `racp` listens only on `127.0.0.1`. Public reachability is delegated to an
  existing Cloudflare Tunnel on your network.
- The terminal-style web UI is a single embedded HTML file (`src/ui.html`)
  compiled into the binary via `include_str!`.

## File layout

```
RACP/
├── Cargo.toml
├── src/
│   ├── main.rs     # config, ACP bridge, transports, single binary entry
│   └── ui.html     # embedded terminal-style web UI (served at /)
├── cloudflared/
│   └── config.yml  # example named-tunnel config (only for a fresh tunnel)
└── README.md
```

## Quick start

```sh
cd /Users/stefan/Development/RACP
cargo run --release
```

First launch, no config present: `racp` drops into an interactive setup and
writes `~/.racp/config.toml`. Re-run setup any time:

```sh
cargo run --release -- init
```

Then point a browser at `http://127.0.0.1:7842` to smoke-test locally, or at
your public hostname once the tunnel is wired.

## Configuration reference

`~/.racp/config.toml`:

```toml
# Transport picks how clients reach racp.
#   "cloudflared" — serves HTTP + WebSocket on `bind`, for an external tunnel.
#   "telegram"    — stub; long-polls a Telegram bot. Not yet implemented.
transport = "cloudflared"

# Local bind address. Keep it on loopback; public exposure is the tunnel's job.
bind = "127.0.0.1:7842"

# Command to launch the ACP agent. Either a bare name (resolved via $PATH) or
# an absolute path. For Kiro: `kiro-cli` with args ["acp"].
agent_cmd = "kiro-cli"
agent_args = ["acp"]

[telegram]
# Only used when transport = "telegram". Create with @BotFather.
token = ""
```

### Agent command examples

| Agent | `agent_cmd` | `agent_args` |
|-------|-------------|--------------|
| Kiro CLI | `kiro-cli` | `["acp"]` |
| Claude Agent CLI | `claude` | check your install; may be `["acp"]` or similar |
| Gemini CLI | `gemini` | check your install |
| Codex | `codex` | check your install |

Confirm a working invocation by running it manually; a healthy ACP agent prints
nothing to stdout until it receives a JSON-RPC message on stdin.

## Expose via an existing Cloudflare Tunnel

If you already have `cloudflared` running (Proxmox LXC, Docker, systemd unit,
whatever), add one ingress rule to its config:

```yaml
ingress:
  # ... your existing rules above ...
  - hostname: racp.example.com
    service: http://<host-running-racp>:7842
  # keep the catch-all last
  - service: http_status:404
```

Route the hostname to the tunnel once from the machine that owns the tunnel
credentials:

```sh
cloudflared tunnel route dns <your-tunnel-name> racp.example.com
```

Reload `cloudflared`. That is it. WebSocket upgrades are forwarded by default,
so `/ws` needs no special flags.

## Put Cloudflare Access in front (strongly recommended)

Once a public hostname points at `racp`, anyone who finds the URL can drive
your local agent. Treat this as non-optional:

1. Cloudflare Zero Trust → **Access → Applications → Add application → Self-hosted**.
2. Application domain: `racp.example.com`.
3. Policy: allow only your email, passkey, or IdP identity.

Access injects a signed `Cf-Access-Jwt-Assertion` header on every request.
Validating it in `racp` is a TODO (see *Known gaps*).

## Wire protocol

### Browser ↔ racp (over the WebSocket at `/ws`)

All frames are JSON text.

**Browser → racp:**

```json
{ "type": "prompt", "text": "hello" }
{ "type": "permission_response", "id": <original id>, "optionId": "allow_once" }
{ "type": "cancel" }
```

**racp → browser:**

```json
{ "type": "ready", "sessionId": "sess_...", "resumed": true | false }
{ "type": "append", "role": "user" | "agent" | "sys", "text": "..." }
{ "type": "prompt_done" }
{ "type": "permission_request", "id": <original id>, "title": "...", "options": [...] }
{ "type": "error", "message": "..." }
```

The browser appends `{role}`-classed text into the scrollback; roles drive
colour. `prompt_done` re-enables the input after a turn completes.
`permission_request` renders an inline card with one button per option; the
user's click sends back a `permission_response` with the matching
`optionId`, which racp forwards to the agent. `cancel` triggers
`session/cancel` on the agent. `ready` fires once per (re)connect and carries
the ACP `sessionId`; the browser persists it so reconnects and reloads pass
`?session=<id>` and the server can `session/load` to resume history.

The WS upgrade URL also accepts `?cwd=<absolute-path>` to scope the session
to a different working directory than `racp`'s own process cwd. When the
browser prompts on `+`, it passes the result here; absent or empty means
fall back to `racp`'s cwd.

### Cross-device UI state

The browser persists the open-tabs list, history, and numeric counter via
two endpoints:

- `GET /state` — returns the current state JSON, or `{}` if nothing stored.
- `PUT /state` — atomically replaces the state (writes to `.tmp` + rename).

Backing file: `~/.racp/state.json`. Any browser hitting this racp sees the
same tabs and history — useful when you move between laptop and phone
through the same tunnel. Actual conversation content stays with the agent
(Kiro at `~/.kiro/sessions/cli/`); racp only stores labels, cwds, and
ACP session ids.

### racp ↔ agent (over stdio, ACP JSON-RPC 2.0)

Methods currently exercised:

- `initialize` — negotiates protocol version and capabilities on spawn.
- `session/new` — creates one session per browser connection. `cwd` is
  `racp`'s current working directory.
- `session/prompt` — forwards each user message.
- `session/request_permission` — forwarded to the browser as a
  `permission_request` event; racp waits for the user's click and replies to
  the agent with the selected `optionId`.

Notifications handled (`session/update` with `sessionUpdate` variants):

- `agent_message_chunk` → appended as `agent` text in the UI.
- `agent_thought_chunk` → appended as `sys` text with a `(thinking)` prefix.
  Kiro's ACP agent does not emit these at the time of writing; reasoning-model
  agents may.
- `tool_call` / `tool_call_update` → appended as `sys` lines
  `[title — status]`.

Kiro also emits `_kiro.dev/*` extension notifications (slash-command
availability, MCP init status, compaction progress, etc.). `racp` ignores them
today; wire them up in `handle_agent_message` if you want them surfaced.

## Running the binary directly

```sh
cargo build --release
./target/release/racp           # run
./target/release/racp init      # re-run setup
```

Stderr carries `racp`'s own logs and, prefixed with `[agent]`, the agent's
stderr. For Kiro-side debugging:

```sh
KIRO_LOG_LEVEL=debug ./target/release/racp
```

(the env var is inherited by the spawned `kiro-cli acp` child).

## Known gaps (pick up here)

All marked with `TODO:` in the source.

1. **Auth enforcement.** `racp` trusts everything that reaches the WebSocket
   upgrade. When fronted by Cloudflare Access, validate the
   `Cf-Access-Jwt-Assertion` header (JWKS at
   `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`). See
   `ws_upgrade` in `src/main.rs`.
2. **Telegram transport.** `run_telegram` is a stub. Planned shape: long-poll
   `getUpdates`, one ACP agent per Telegram chat, stream chunks as
   `editMessageText` throttled to ~1/s, inline keyboard for permission
   prompts. Per-user-token model (BotFather) keeps `racp` out of the data
   path.
3. **Kiro extensions (`_kiro.dev/*`).** Not forwarded. Useful candidates:
   slash-command availability (could become a UI affordance), MCP OAuth URL
   (needs user redirect), compaction status (status-bar hint).
4. **Streamable HTTP remote transport.** ACP's draft RFD defines an HTTP/WS
   remote transport with `Acp-Connection-Id` and `Acp-Session-Id` headers;
   today `racp` is purely a local stdio client. Once the RFD stabilises and
   agents support it, `racp` can become a thin remote adapter too.

## Development guide

### Build, check, lint

```sh
cargo check
cargo build --release
cargo clippy --all-targets -- -D warnings   # team should gate on this
```

Rust edition 2021, MSRV follows the current stable toolchain.

### Where to add things

| Change | File and function |
|--------|-------------------|
| New `session/update` variant | `handle_agent_message` in `src/main.rs` |
| Permission UI | split `session/request_permission` handling; extend browser ↔ racp protocol |
| Auth middleware | wrap `Router` in `run_cloudflared` or apply to the `/ws` route |
| New transport (telegram, matrix, ...) | mirror `run_cloudflared`; add variant to the `Transport` enum |
| UI tweaks | `src/ui.html` — no build step, `cargo run` rebuilds |

### Testing

There are no tests yet. Practical coverage to add:

- **Config round-trip.** Property test that `Config` survives
  `toml::to_string_pretty` → `toml::from_str`.
- **JSON-RPC routing.** Unit test the reader task: feed it a mix of
  responses (by id), notifications, and malformed lines, assert the right
  routing.
- **End-to-end smoke.** Spawn `racp` with `agent_cmd = "bash"` and
  `agent_args = ["-c", "<echo canned JSON-RPC>"]` as a fake agent.

### Debugging

- `cargo run --release` prints bind address on stderr and forwards the
  child's stderr with `[agent]` prefix.
- Run the agent manually first to confirm the invocation works:
  `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | kiro-cli acp`
  should respond with JSON.
- Browser devtools → Network → WS view shows every frame in both directions.

## Troubleshooting

**`failed to spawn \`kiro-cli\``**
 `agent_cmd` not on `$PATH`. Use `which kiro-cli` and put the absolute path in
 `~/.racp/config.toml`.

**Browser connects, no response after send**
 Check the `[agent]` lines on `racp`'s stderr. Most often the agent is
 prompting for an auth/setup step that only appears in interactive TTYs.
 Finish setup by running the agent once interactively first.

**Cloudflare hostname returns 502**
 The `cloudflared` machine cannot reach the `racp` machine. Check
 `service: http://<host>:7842` in the ingress rule resolves and the port is
 open on the `racp` host.

**WebSocket closes immediately**
 Cloudflare Access policy is rejecting the upgrade. Hit the hostname in a
 browser first to satisfy Access, then retry.

## Licence and ownership

Internal project. Licence TBD before any external distribution.
