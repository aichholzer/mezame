![Mezame](https://raw.githubusercontent.com/aichholzer/mezame/0b37a3051b6d9a7853ffcdf3c07602215f4e85c6/assets/Mezame.png)

[![CI](https://github.com/aichholzer/mezame/actions/workflows/ci.yml/badge.svg)](https://github.com/aichholzer/mezame/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/aichholzer/mezame/graph/badge.svg?token=UV3BE0RQ0U)](https://codecov.io/gh/aichholzer/mezame)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/aichholzer/mezame?utm_source=badge)
[![Latest version](https://img.shields.io/crates/v/mezame.svg)](https://crates.io/crates/mezame)
![License](https://img.shields.io/crates/l/mezame.svg)<br />
![macOS](https://img.shields.io/badge/-macOS-black)
![Linux](https://img.shields.io/badge/-Linux-green)

_Wake your agent up from anywhere. Anytime._

One binary you install on a machine you already own. It serves a chat UI to
your browser, holds the conversation, and does the work. Open the same session
on your phone and your laptop at once and both show the same thing.

The name is Japanese for "awakening" (**目覚め**), the moment your agent comes
back online from across town.

## What it does

Mezame is an **agent harness**. It serves a browser UI, keeps one session per
conversation, and runs a turn when you send a prompt. Several browsers can
attach to one session at the same time; a session survives a reload and a
reconnect.

## What Mezame is not

"AI-adjacent tool" covers a lot of ground, and clarity matters here.

- **Mezame is not a hosted service.** You install it on a machine you control.
  There is no account to create and nothing of yours leaves that machine except
  to whichever provider you configure.
- **Mezame has no authentication of its own.** Every request that reaches the
  socket is trusted. Access control is pushed to the edge: bind an address in
  your own network and put something in front of it that already knows who you
  are.
- **Mezame is not multi-user.** One installation serves one person's sessions.
  The session list and the settings are shared across every browser that can
  reach it, deliberately, so your phone and your desktop stay in sync.
- **Mezame does not touch your files on its own.** It reads and writes
  `~/.mezame/` and nothing else.

## Why Mezame

There are plenty of tools that let you drive a model from the couch. Most of
them fall into one of two shapes:

1. **Hosted front ends.** Somebody else runs the server, holds the
   conversation, and sets the terms. Convenient until you want the transcript
   on your own disk or a model they do not offer.
2. **Local tools with no way in from outside.** A terminal on the machine
   itself, and nothing when you are not sitting at it.

Mezame is the third shape: your machine, your conversation, reachable from
anywhere your network lets you in. The install is three commands and there is
no fourth:

```sh
cargo install mezame
mezame init
mezame
```

`mezame init` asks one question, the address to bind. Access control for the
browser is pushed to the edge: bind an address in your network, put a
Cloudflare Tunnel in front, and let Cloudflare Access gate the hostname with
your existing identity provider. You already trust that stack with the rest of
your self-hosted tools.

## Features

What this build does today:

- Several sessions per browser, each its own conversation, in tabs.
- One session on several devices: open the same conversation on a phone and a
  laptop, and every turn lands on both as it happens.
- A session survives a reload or a reconnect within a 30-second grace window,
  and the transcript is served back on attach. A turn still running when the
  last browser leaves keeps running, for up to 30 minutes.
- Recently-closed history with one-click restore.
- Auto-reconnect with exponential back-off on WebSocket drops.
- Idle sessions release their resources 30 seconds after the last browser
  leaves.

What the browser already renders, waiting on a provider to feed it: model
selection shared across every attached browser, reasoning in a collapsible
block, tool calls as expandable cards with arguments and output, and
permission prompts. See [Roadmap](#roadmap).

## Install

```sh
cargo install --git https://github.com/aichholzer/mezame --branch feature/harness
mezame init
mezame
```

Then point a browser at `http://127.0.0.1:9510` (or whatever address and port
you set) to run locally, or at your public hostname once your tunnel is wired.

`cargo install mezame` installs the 0.13 release from crates.io until the
harness line merges. That release is a different program: it drives a separate
agent process rather than doing the work itself. Use the branch command above
for the harness.

### Prerequisites

- **Rust** toolchain, stable. [rustup][rustup] or your distro's package
  manager.
- **Node.js** 24 or newer with `npm` on `PATH`. Installing Mezame builds the
  embedded React UI as part of `build.rs`; the install fails fast if `node` or
  `npm` is missing or too old.

[rustup]: https://rustup.rs

`cargo install` puts the binary at `~/.cargo/bin/mezame`. The UI bundle is
baked into it. No need for Node.js at run time.

## Docs

- [Architecture and layout](./docs/architecture.md): diagram, file layout,
  config reference.
- [Running as a service](./docs/service.md): systemd (Linux) and launchd
  (macOS) units so Mezame starts at boot.
- [Cloudflare Tunnel and Access](./docs/cloudflare.md): step-by-step for
  exposing Mezame publicly and gating it with an identity provider.
- [Wire protocol](./docs/wire-protocol.md): the catalogue of messages between a
  browser and Mezame.
- [Development](./docs/development.md): build, dev loop, where to add things,
  debugging.

## Build from source

Alternative to `cargo install`. Useful when you're iterating on Mezame itself
or want to run a branch. Same prerequisites apply.

```sh
git clone https://github.com/aichholzer/mezame
cd mezame
git switch feature/harness
cargo build --release
./target/release/mezame           # run
./target/release/mezame init      # re-run setup
```

`cargo build` invokes the UI build as part of `build.rs`. The UI is compiled
inside `$OUT_DIR` (cargo's per-crate build directory) so the source tree stays
clean. The first build seeds `node_modules` via `npm ci` and is the slow one;
later builds are cache hits and mostly free. Set `MEZAME_SKIP_UI_BUILD=1` to
skip the UI build when iterating on Rust only; the binary then ships without a
UI.

## Run with Docker

A lightweight Alpine-based [`Dockerfile`](./Dockerfile) and
[`compose.yaml`](./compose.yaml) are included if you'd rather not install Rust
and Node.js on the host. The image is built from this repository's source.

First-run setup, one-off:

```sh
docker compose run --rm setup
```

That runs `mezame init` interactively. **Choose `0.0.0.0:9510` at the bind
prompt.** The default, `127.0.0.1:9510`, binds loopback inside the container,
and a published port then answers nothing.

Then:

```sh
docker compose up -d
```

The configuration is persisted in a named volume, so you answer that prompt
once. See the comments in `compose.yaml` for the full flow.

`compose.yaml` publishes the port on the host's loopback only,
`127.0.0.1:9510`. Mezame has no authentication of its own, so that is the
default. To reach it from other machines on a network you trust, change the
mapping to `"0.0.0.0:9510:9510"`, knowing that on Linux Docker's own firewall
rules bypass `ufw` and the port opens on every network the host is on. A
Cloudflare Tunnel running on the host reaches the loopback mapping as it is.

Stderr carries Mezame's own logs. One environment variable is worth knowing:

- `MEZAME_SKIP_UI_BUILD=1` tells `build.rs` not to run the UI build. The binary
  is then missing its UI; useful only for Rust-only iteration.

## Known gaps

1. **No provider.** The 0.14.0-alpha.1 build answers every prompt with an echo
   of what you typed and talks to no provider. It exists to prove the
   transport: connect two browsers to one session, send a prompt, and watch it
   land on both. The provider loop is the next release on this line.
2. **Auth enforcement.** Mezame trusts everything that reaches the WebSocket
   upgrade. When fronted by Cloudflare Access, validate the
   `Cf-Access-Jwt-Assertion` header (JWKS at
   `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`). See
   `ws_upgrade` in `src/ws.rs`.
3. **A transcript lives as long as its session.** Nothing is written to disk,
   so a reload after the grace period shows an empty log and a restart loses
   every conversation. Durable storage is planned.

## Roadmap

None of these ship today, and none block the core loop.

1. **The provider loop.** Streaming turns against a real provider, with model
   selection, cancellation, and token accounting reported per turn. The seam it
   drops into already exists: one trait, six operations.
2. **Durable storage and accounts.** A local database for transcripts,
   settings, and credentials, and a login so an installation can serve more
   than one person.
3. **Tools and workspaces.** File reads and writes, shell commands, and an
   approval flow in the browser, scoped to a directory you nominate.
4. **Telegram transport.** `run_telegram` is a stub and the option is hidden
   from `mezame init`. Planned shape: long-poll `getUpdates`, one session per
   chat, stream chunks as `editMessageText` throttled to about one per second,
   inline keyboard for approvals.

## Troubleshooting

**`cargo build` fails with "npm not found"**
`build.rs` requires `node` and `npm`. Install Node.js 24 or newer and retry.
`MEZAME_SKIP_UI_BUILD=1` lets the Rust build complete without Node.js, but the
resulting binary is missing its UI.

**`cargo build` fails naming a Node.js version**
The UI build needs Node.js 24 or newer. Check `node --version` and upgrade.

**No config at `~/.mezame/config.json`**
Run `mezame init`. It writes the file after one prompt. Run with no terminal
attached (under a service manager, or in a container without a TTY), it exits
non-zero and writes nothing.

**Browser connects, the composer is read-only**
A turn is in flight on that session, started here or on another device. It
unlocks when that turn ends.

**Cloudflare hostname returns 502**
The `cloudflared` machine cannot reach the Mezame machine. Check that
`service: http://<host>:9510` in the ingress rule resolves and that the port is
open on the Mezame host.

**WebSocket closes immediately**
Cloudflare Access policy is rejecting the upgrade. Hit the hostname in a
browser first to satisfy Access, then retry.

## Licence

[MIT](./LICENSE).

## Support

RTFM, then RTFC... If you are still stuck or just need an additional feature, file an [issue](https://github.com/aichholzer/mezame/issues).

## Trademarks

Product and company names referenced in this documentation are trademarks or
registered trademarks of their respective owners. Use of these names is for
identification purposes only and does not imply endorsement.

Mezame is an independent, third-party open-source project and is not affiliated
with, endorsed by, sponsored by, or reviewed by Amazon Web Services, Inc.,
Amazon.com, Inc., or any of their affiliates.

<div align="center">
✌🏼
</div>
