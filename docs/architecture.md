# Architecture and layout

## Diagram

```mermaid
flowchart LR
  browser[Browser]
  mezame["Mezame"]
  agent["Agent<br/>(kiro-cli acp,<br/>claude, gemini,<br/>codex, ...)"]
  transport["Transport layer"]

  browser <-- WS --> mezame
  mezame <-- "STDIO JSON-RPC" --> agent
  mezame --- transport
```

- One ACP session = one agent subprocess, owned by a hub. Many browser WebSockets can attach to the same hub at once, so the same conversation stays in sync across phone, laptop, and desktop. The agent is spawned when the first browser attaches and stays warm for a short grace window (30s) after the last browser detaches, so a reload or transient drop reattaches to the running agent rather than respawning it. See `src/hub.rs`.
- Mezame binds loopback by default; `mezame init` also offers `0.0.0.0` for trusted-LAN setups. Public reachability can be delegated to an existing Cloudflare Tunnel on your network.
- The web UI is a React + Tailwind v4 app under `ui/`. The `build.rs` step runs the Vite build; the compiled bundle is baked into the binary via `rust-embed` so the release binary stays self-contained.

## File layout

```
Mezame/
├── Cargo.toml
├── Cargo.lock
├── CHANGELOG.md
├── LICENSE
├── build.rs                    # runs `npm ci` + `npm run build` in ui/
├── assets/                     # logo (Mezame.png) and source artwork (Mezame.af)
├── docs/                       # long-form documentation (wire protocol, etc.)
├── src/
│   ├── main.rs                 # thin CLI shim; calls mezame::run()
│   ├── lib.rs                  # CLI entry (run/help/version), module wiring, transport dispatch
│   ├── config.rs               # on-disk config and interactive setup
│   ├── agent.rs                # ACP subprocess wrapper and JSON-RPC framing
│   ├── session.rs              # session resume and stale-lock recovery
│   ├── hub.rs                  # multi-attach session hub: one agent, many browsers
│   ├── http.rs                 # cloudflared transport, UI assets, /state, /history
│   ├── ws.rs                   # per-WS attach loop and agent-message dispatch
│   └── unix.rs                 # tiny Unix FFI helpers (kill, setsid)
├── ui/                         # React UI (Vite, TS, Tailwind v4, shadcn)
│   ├── index.html
│   ├── package.json            # UI version lives here
│   ├── vite.config.ts
│   └── src/
│       ├── App.tsx
│       ├── main.tsx
│       ├── index.css
│       ├── types.ts            # wire-protocol and state types
│       ├── hooks/useMezame.ts   # store, WS lifecycle, state sync
│       ├── features/           # SideBar, LogPane, InputRow, ...
│       ├── components/         # CopyButton, BotIcon + shadcn primitives
│       └── lib/                # utils, time helpers
```

## Configuration reference

`~/.mezame/config.json`:

```json
{
  "transports": [{ "kind": "cloudflared", "bind": "127.0.0.1:9510" }],
  "agent_cmd": "kiro-cli",
  "agent_args": ["acp"]
}
```

- `transports`: list of transport entries. Each entry is internally tagged by `kind`. Only `"cloudflared"` is implemented today; running more than one entry at once is not yet supported, so keep the list at a single element. The list shape is future-proofing for adding Telegram and others later (see Roadmap).
- `transports[].kind = "cloudflared"`: serves HTTP + WebSocket on `bind`, for an external tunnel.
- `transports[].bind` (cloudflared only): local bind address. Default is loopback; `mezame init` offers `0.0.0.0:9510` if you want LAN reach. Mezame has no auth of its own today, so anything non-loopback relies on Cloudflare Access, or your LAN being trusted.
- `agent_cmd`: command to launch the ACP agent. Either a bare name, resolved via `$PATH`, or an absolute path. For Kiro use `kiro-cli` with args `["acp"]`.
