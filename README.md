![Mezame](https://github.com/aichholzer/mezame/blob/1e7610c7d345279ff32cc8eebbd0eef35bb5a2b7/assets/Mezame.png)

# Mezame!

[![Tests](https://github.com/aichholzer/mezame/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/aichholzer/mezame/actions/workflows/test.yml)
[![codecov](https://codecov.io/gh/aichholzer/mezame/graph/badge.svg?token=UV3BE0RQ0U)](https://codecov.io/gh/aichholzer/mezame)
[![Latest version](https://img.shields.io/crates/v/mezame.svg)](https://crates.io/crates/mezame)
![License](https://img.shields.io/crates/l/mezame.svg)<br />
![OSX](https://img.shields.io/badge/-OSX-black)
![Linux](https://img.shields.io/badge/-Linux-green)

_Wake your agent up from anywhere. Anytime._

A small bridge that puts a browser-based chat UI in front of a locally running ACP agent. Today Mezame is tailored primarily for [Kiro&trade; CLI][kiro-cli]; any other agent that speaks the [Agent Client Protocol][acp] over stdio (Claude Agent CLI, Gemini CLI, Codex) should reach the core loop, but several features rely on Kiro-specific extensions and on-disk layouts. See [Kiro specifics](#kiro-specifics) below for what is tailored and what is generic.

[acp]: https://agentclientprotocol.com
[kiro-cli]: https://kiro.dev/cli/

The name is Japanese for "awakening" (**目覚め**), the moment your agent comes back online from across town.

## What it does

Mezame is an **ACP client**. It spawns your configured agent binary as a child process, speaks `JSON-RPC 2.0` with it over `stdio`, and bridges the conversation to a browser over WebSockets.

## What Mezame is not

Clarity matters here, because "AI-adjacent tool" covers a lot of ground.

- **Mezame is not an agent.** It does not reason, plan, call tools, or make any decisions of its own.
- **Mezame does not talk to an LLM.** It has no model, no inference code, no prompt engineering. It carries bytes between a browser and a subprocess; that is the entire job.
- **Mezame has no credentials of its own.** It does not hold API keys, OAuth tokens, or AWS credentials. It never authenticates to any provider.
- **Mezame is useless on its own.** It requires an ACP-speaking agent (e.g. Kiro CLI) to be installed, authenticated, and working locally. All intelligence, access, billing, and policy live with that agent. Mezame only exposes a conversation surface to a browser you control.
- **Mezame does not modify your agent's files.** It reads `~/.kiro/sessions/cli/<id>.jsonl` to replay history on resume, and it declares `fs.readTextFile: false` and `fs.writeTextFile: false` at ACP `initialize`, so the agent cannot ask Mezame to touch the filesystem either. The sole exception is stale-lockfile cleanup under `~/.kiro/sessions/cli/`: if a `.lock` points at a dead PID, Mezame removes it so the next `session/load` succeeds. Mezame's own config and state live in `~/.mezame/`.

**Put plainly:** if you uninstall the agent, Mezame has nothing to show you.

## Kiro specifics

Today Mezame is built and tested against Kiro CLI. A few features assume Kiro's conventions and will degrade on other agents until per-agent adapters land (roadmap):

- Chat history rehydration on resume reads `~/.kiro/sessions/cli/<id>.jsonl`.
- Slash-command autocomplete consumes the `_kiro.dev/commands/available` notification.
- Stale-lock recovery targets Kiro's lockfile convention.
- The Kiro child inherits `KIRO_LOG_LEVEL` for agent-side tracing.

The rest (prompt, stream, cancel, permissions, modes, models, attachments when the agent advertises them) is standard ACP. Claude Agent CLI, Gemini CLI, Codex, and others should connect and drive the core loop; expect missing slash-command autocomplete and no history replay on resume until we ship the per-agent adapters.

> **Bug reports from non-Kiro users are especially welcome.**

## Why Mezame

There are plenty of tools that let you drive an LLM from the couch. Most of them fall into one of two shapes:

1. **Direct-to-provider gateways.** They talk to Bedrock, Anthropic, OpenAI, or Google themselves. You hand them API keys, provision users, manage roles and quotas, and store credentials somewhere they can reach. The tool becomes another auth surface you have to look after.
2. **Remote UIs for a local agent CLI.** These front a locally-running agent process, which is closer to what I wanted. Some of them still ask you to generate access tokens, configure users, or wire up a separate auth flow between the browser and the bridge itself.

Mezame deliberately does less than either. It is an **ACP pipe**, nothing more:

- Your local agent (Kiro, Claude, Gemini, Codex, whatever...) already handles its own login, its own model choice, its own credentials, and its own session storage. That solved problem stays solved.
- Mezame never sees an API key. It does not know what the agent authenticates to, and does not want to.
- Access control for the browser is pushed to the edge: bind an address in your network, put a Cloudflare Tunnel in front, let Cloudflare Access gate the hostname with your existing identity provider. You already trust that stack with the rest of your self-hosted tools.

**The net effect:** if your agent is signed in locally, you can talk to it from anywhere your tunnel reaches. If it is not, Mezame has nothing useful to offer. That is the whole trade.

## Features

## Features

- Multiple concurrent sessions per browser, each backed by its own agent.
- Share a session across devices: open the same conversation on your phone, laptop, and desktop. Everything stays in sync.
- Model and mode switches propagate to every connected browser.
- Reasoning models show their thought process in a collapsible block. Survives reloads.
- Tool calls render as expandable cards with arguments and output.
- Sessions persist across reloads and restarts; stale lockfiles are recovered automatically.
- Recently-closed history with one-click restore.
- Per-session working directory override via `?cwd=<path>`.
- Cancel the active turn from any connected browser.
- Auto-reconnect with exponential back-off on WebSocket drops.

## Install

```sh
cargo install mezame
mezame init
mezame
```

Then point a browser at `http://127.0.0.1:9510` (or whatever IP and port you set) to run locally, or at your public hostname once your tunnel is wired.

### Prerequisites

- **Rust** toolchain, stable. [rustup][rustup] or your distro's package manager.
- **Node.js** 22 or newer with `npm` on `PATH`. Installing Mezame builds the embedded React UI as part of `build.rs`; the install fails fast if `node`/`npm` are missing.
- **An ACP-capable agent** on `PATH`: Kiro CLI, Claude Agent CLI, Gemini CLI, Codex, etc. `mezame init` probes for known CLIs and offers them in a menu.

[rustup]: https://rustup.rs
[nvm]: https://github.com/nvm-sh/nvm

`cargo install` will install the binary to `~/.cargo/bin/mezame`. The UI bundle is baked into the binary. No need for Node.js at runtime.

## Docs

- [Architecture and layout](./docs/architecture.md): diagram, file layout, config reference.
- [Running as a service](./docs/service.md): systemd (Linux) and launchd (macOS) units so Mezame starts at boot.
- [Cloudflare Tunnel and Access](./docs/cloudflare.md): step-by-step for exposing Mezame publicly and gating it with an identity provider.
- [Wire protocol](./docs/wire-protocol.md): browser to Mezame and Mezame to agent message catalogue.
- [Development](./docs/development.md): build, dev loop, where to add things, debugging.

## Build from source

Alternative to `cargo install`. Useful when you're iterating on Mezame itself or want to run a branch. Same prerequisites apply.

```sh
git clone https://github.com/aichholzer/mezame
cd mezame
cargo build --release
./target/release/mezame           # run
./target/release/mezame init      # re-run setup
```

`cargo build` invokes the UI build as part of `build.rs`. The UI is compiled inside `$OUT_DIR` (cargo's per-crate build directory) so the source tree stays clean. First build seeds `node_modules` via `npm ci` and is the slow one; subsequent builds are cache hits and mostly free. Set `MEZAME_SKIP_UI_BUILD=1` to skip the UI build when iterating on Rust only; the binary will ship without a UI.

## Run with Docker

A lightweight Alpine-based [`Dockerfile`](./Dockerfile) and [`compose.yaml`](./compose.yaml) are included if you'd rather not install Rust, Node, and Kiro CLI on the host. The image bundles Rust, Node.js, Kiro CLI, and Mezame.

First-run setup, one-off:

```sh
docker compose run --rm setup
# inside the container:
kiro-cli login --use-device-flow
mezame init
exit
```

Then:

```sh
docker compose up -d
```

Kiro credentials, session history, and Mezame config are persisted in named volumes, so you only authenticate once. See the comments in `compose.yaml` for the full flow.

Stderr carries Mezame's own logs and, prefixed with `[agent]`, the agent's stderr. Useful env vars:

- `KIRO_LOG_LEVEL=debug`, passed through to the spawned `kiro-cli acp` child for Kiro-side tracing.
- `MEZAME_DEBUG_ACP=1`, dump every inbound ACP line from the agent to Mezame's stderr with an `[acp<-]` prefix. Helpful when wiring new Kiro extensions.
- `MEZAME_SKIP_UI_BUILD=1`, tell `build.rs` not to run the UI build. The binary will be missing its UI; useful only for Rust-only iteration when the UI is unchanged from a previous build.

## Known gaps

1. **Auth enforcement.** Mezame trusts everything that reaches the WebSocket upgrade. When fronted by Cloudflare Access, validate the `Cf-Access-Jwt-Assertion` header (JWKS at `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`). See `ws_upgrade` in `src/ws.rs` and the backlog in `todo.md`.
2. **Remaining Kiro extensions.** Compaction and clear status notifications are still dropped. Slash commands, the commands catalogue, and MCP OAuth URL requests are surfaced.
3. **Attachment rehydration on resume.** Prompts with images or embedded resources are sent correctly on the live path, but when the browser reconnects and Mezame replays history via Kiro's on-disk JSONL, only text turns are rendered. The parser in `parse_kiro_history` (`src/http.rs`) only knows about user/agent text today. Extending it requires knowing the shape Kiro uses for non-text prompt blocks in its JSONL, which has not been inspected yet. Until then, attachments in historical turns will appear as plain text (or be missing entirely) after a resume.

## Roadmap

None of these ship today, and none block the core loop.

1. **Per-agent adapters.** Today Mezame's history parser, slash-command handling, and lockfile recovery all target Kiro. The plan is to extract these behind a small adapter trait so Claude Agent CLI, Gemini CLI, and Codex can supply their own equivalents (or opt out). Until then, non-Kiro agents reach the core loop but miss history replay and slash commands.
2. **Telegram transport.** `run_telegram` is a stub and the option is hidden from `mezame init`. Planned shape: long-poll `getUpdates`, one ACP agent per Telegram chat, stream chunks as `editMessageText` throttled to ~1/s, inline keyboard for permission prompts. Per-user-token model (BotFather) keeps Mezame out of the data path.
3. **Streamable HTTP remote transport.** ACP's draft RFD defines an HTTP/WS remote transport with `Acp-Connection-Id` and `Acp-Session-Id` headers; today Mezame is purely a local stdio client. Once the RFD stabilises and agents support it, Mezame can become a thin remote adapter too.

## Troubleshooting

**`failed to spawn kiro-cli`**
`agent_cmd` not on `$PATH`. Use `which kiro-cli` and put the absolute path in `~/.mezame/config.json`.

**`cargo build` fails with "npm not found"**
`build.rs` requires `node` and `npm`. Install Node.js and retry. `MEZAME_SKIP_UI_BUILD=1` lets the Rust build complete without Node, but the resulting binary will be missing its UI.

**Browser connects, no response after send**
Check the `[agent]` lines on Mezame's stderr. Most often the agent is prompting for an auth/setup step that only appears in interactive TTYs. Finish setup by running the agent once interactively first.

**`[previous session X could not be resumed (...)]`**
The browser tried to resume via `session/load` and Kiro refused. Either the session file is gone from `~/.kiro/sessions/cli/`, or a live process still holds the lock. Mezame retries with exponential back-off (~1.25 s budget) and steals stale-PID locks automatically. If you see this message persistently, there is a genuine conflict: check `ps` for another Kiro process holding that session, or delete the `.lock` file manually.

**Cloudflare hostname returns 502**
The `cloudflared` machine cannot reach the Mezame machine. Check `service: http://<host>:9510` in the ingress rule resolves and the port is open on the Mezame host.

**WebSocket closes immediately**
Cloudflare Access policy is rejecting the upgrade. Hit the hostname in a browser first to satisfy Access, then retry.

## Licence

[MIT](./LICENSE).

## Support

RTFM, then RTFC... If you are still stuck or just need an additional feature, file an [issue](https://github.com/aichholzer/mezame/issues).

## Trademarks

Kiro&trade; is a trademark of Amazon.com, Inc. or its affiliates. All other product and company names referenced in this documentation (Claude, Gemini, Codex, Cloudflare, and others) are trademarks or registered trademarks of their respective owners. Use of these names is for identification purposes only and does not imply endorsement.

Mezame is an independent, third-party open-source project and is not affiliated with, endorsed by, sponsored by, or reviewed by Amazon Web Services, Inc., Amazon.com, Inc., or any of their affiliates.

> **Not affiliated with AWS or the Kiro product.** Mezame is an independent, third-party open-source client. "Kiro" and "Kiro CLI" are used here only to identify the agent Mezame talks to; any use of those marks is purely nominative. Mezame is not built, endorsed, or reviewed by Amazon Web Services.

<div align="center">
✌🏼
</div>