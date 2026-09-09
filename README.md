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

Mezame is an **agent harness**. It serves a browser UI behind a login, keeps
one session per conversation, and runs each prompt as a turn against an
Anthropic model on Amazon Bedrock, streaming the answer and the model's
reasoning back as they arrive. Several browsers can attach to one session at
the same time, conversations live in a local datastore, and a session survives
a reload, a reconnect and a restart of the server.

## What Mezame is not

"AI-adjacent tool" covers a lot of ground, and clarity matters here.

- **Mezame is not a hosted service.** You install it on a machine you control.
  There is no account to create and nothing of yours leaves that machine except
  to whichever provider you configure.
- **Mezame knows who is asking.** Every request carries a signed session
  cookie, and everything but the sign-in form answers 401 without one; a
  socket opened without one is closed with its own code, and the browser
  shows the login. Mezame also checks that every request names a host it
  serves and that a WebSocket upgrade or a write comes from a page it served
  itself, so a hostile page cannot ride your browser into a loopback Mezame.
  Sessions belong to the account that opened them: another account's session
  answers 404 wherever it is named. The login is the floor, not the whole
  posture: on a plain-HTTP LAN bind the cookie crosses the network readable,
  so a public hostname belongs behind TLS, which a Cloudflare Tunnel gives
  you for free, and an identity layer at the edge remains a good second
  factor.
- **Mezame is multi-user, minimally.** Accounts are created from the
  terminal, each sees only its own sessions and settings, and every browser
  signed in to one account stays in sync. There are two roles, `admin` and
  `user`, which nothing distinguishes yet, and no interface for sharing a
  provider credential between accounts; those arrive in a later phase.
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

`mezame init` asks for the address to bind, the admin account, and the
Bedrock model, region and profile; the credentials are the ones your AWS CLI
already has. The login gates every request, and a Cloudflare Tunnel in front
gives a public hostname TLS and, with Access, a second identity layer from
your existing provider.

## Features

What this build does today:

- Runs each turn against an Anthropic model on Amazon Bedrock through
  `ConverseStream`, with the answer streaming into the log and the model's
  reasoning into a collapsible block as they arrive. The Claude 4.6 line and
  later think adaptively; the models before it take a token budget.
- Keeps the conversation across turns with a prompt-cache checkpoint on the
  latest message, so a long conversation is billed mostly as cache reads. Each
  answer carries its token counts (input, output, cached, written) under the
  bubble.
- Lets you pick among the models listed in the config, shared across every
  attached browser; cancel a turn mid-stream; and attach images and documents
  to a prompt.
- A login in the browser, and accounts from the terminal: `mezame user add`,
  `mezame user list`, `mezame passwd`. Each account sees only its own
  sessions and settings, on every device it signs in from.
- Conversations live in a local SQLite datastore, so a reload past the grace
  window or a restart of the server serves the conversation back, token
  counts included, and the next turn continues it. The Bedrock region and
  profile are encrypted in the same file under a locally generated master
  key.
- Several sessions per browser, each its own conversation, in tabs; one
  session list per account, served by the server, the same on every device.
- One session on several devices: open the same conversation on a phone and a
  laptop, and every turn lands on both as it happens.
- A session survives a reload, a reconnect and a restart, and the transcript
  is served back on attach. A turn still running when the last browser leaves
  keeps running, for up to 30 minutes.
- Recently-closed history with one-click restore.
- Auto-reconnect with exponential back-off on WebSocket drops.
- Idle sessions release their resources 30 seconds after the last browser
  leaves.

What the browser already renders, waiting on a later alpha to feed it: tool
calls as expandable cards with arguments and output, and permission prompts.
See [Roadmap](#roadmap).

## Install

```sh
cargo install --git https://github.com/aichholzer/mezame --branch feature/harness
mezame init
mezame
```

`mezame init` asks its questions in order: the address to bind; the admin
username and password (masked, entered twice, skipped when the datastore
already holds a user); and the Bedrock model id, AWS region and AWS profile,
each optional. An empty model keeps the echo backend, which returns what you
type and reaches no provider; an empty region or profile leaves the SDK's
defaults in force. The same setup with no prompt, for a service unit or a
container started before setup:

```sh
echo 'the password' | mezame init --bind 127.0.0.1:9510 --admin alice --password-stdin \
  --model global.anthropic.claude-sonnet-5 --region us-east-1 --profile work
```

A setting no flag names keeps its current value. The model goes to a profile
row in the datastore, and the region and profile to one encrypted credential
row beside it; the file holds server settings alone. A hand-edited
`~/.mezame/config.json` at version 2, with a second model for the picker:

```json
{
  "version": 2,
  "transports": [{ "kind": "cloudflared", "bind": "127.0.0.1:9510" }],
  "datastore": { "backend": "sqlite" },
  "models": ["global.anthropic.claude-haiku-4-5-20251001-v1:0"]
}
```

`models` is the list the browser's picker offers besides the profile's own
model; `init` writes none. The thinking mode, its budget and the reply
ceiling are not settable in this alpha: the mode follows the model (the
Claude 4.6 line and later think adaptively, the models before it take a
budget), the budget is 4096 tokens and the ceiling 16384, until a later
phase adds a profile editor.

Accounts are managed from the terminal:

```sh
mezame user add carol            # asks for the password twice; --password-stdin for scripts
mezame user list                 # name, role, creation date
mezame passwd carol              # a new password; signs carol out everywhere
```

Under `~/.mezame` live three files, each owner-only: `config.json`,
`mezame.db` (the datastore: users, sessions and their messages, the Bedrock
profile and its encrypted credential) and `master.key`, 32 random bytes the
credential and cookie keys derive from. The key protects a copied
`mezame.db` and nothing more: whoever can read `~/.mezame` whole, or is
root on the machine, has both files. Back the two up together; the
datastore cannot be opened without its key.

Mezame holds no credentials. The AWS SDK finds them where the AWS CLI does:
`~/.aws/credentials` and `~/.aws/config` under the account Mezame runs as,
including SSO profiles after `aws sso login`, or the `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN` variables, or
`AWS_BEARER_TOKEN_BEDROCK` for a Bedrock API key. The model must be enabled for
the account in the Bedrock console under Model access, in the region the config
names, and the credentials need `bedrock:InvokeModelWithResponseStream` on it.
Startup prints a `Backend:` line naming the model in force and nothing of
the region or profile, or `Backend: echo` when no model is set, then a
`Datastore:` line naming the file and its user count.

This alpha assumes a fresh install. There is no migration from 0.13 or from an
earlier alpha: remove `~/.mezame` left by an earlier version first.

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

Mezame is built and tested on Linux and macOS. Windows is not supported: there
is no Windows CI and no Windows binary, and `cargo install` fails at the UI
build because `build.rs` spawns `npm` by name while Node.js for Windows ships it
only as `npm.cmd`.

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
and a published port then answers nothing. Without a terminal:

```sh
echo 'the password' | docker compose run -T --rm setup mezame init \
  --bind 0.0.0.0:9510 --admin alice --password-stdin --model global.anthropic.claude-sonnet-5
```

Credentials reach the container from the host: `compose.yaml` passes
`AWS_PROFILE`, `AWS_REGION`, `AWS_DEFAULT_REGION`, `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` and `AWS_BEARER_TOKEN_BEDROCK`
through from your environment when they are set, and carries a commented
read-only mount of `~/.aws` at `/home/mezame/.aws` for a profile. Export the
variables, or uncomment the mount, before `docker compose up`. A read-only
mount stops the SDK from refreshing an SSO token as it nears expiry, so with an
`sso-session` profile run `aws sso login` on the host again when turns start
failing for credentials, or mount `~/.aws/sso/cache` writable.

Then:

```sh
docker compose up -d --build
```

Keep `--build` after pulling a new version: `up -d` alone reuses the image
already built on this machine. The configuration is persisted in a named
volume, so you answer that prompt once. See the comments in `compose.yaml` for
the full flow.

The container runs as user `mezame` (uid 1000) with its config at
`/home/mezame/.mezame`, on a read-only root filesystem with every capability
dropped. Start from a fresh volume: one created by an earlier version is not
carried over (`docker compose down -v` removes it).

The image declares a health check that fetches `/` on `127.0.0.1:9510` inside
the container; `docker ps` shows `healthy` once Mezame answers. It confirms the
process is serving, not that you chose `0.0.0.0:9510` at the prompt: a loopback
bind is healthy inside the container and unreachable on the published port. If
you bound another port, override `healthcheck` in `compose.yaml`.

`compose.yaml` publishes the port on the host's loopback only,
`127.0.0.1:9510`: the login gates who gets in, and loopback keeps the cookie
off the network. To reach it from other machines on a network you trust,
change the mapping to `"0.0.0.0:9510:9510"`, knowing that the cookie then
crosses that network over plain HTTP and that on Linux Docker's own firewall
rules bypass `ufw`, so the port opens on every network the host is on. A
Cloudflare Tunnel running on the host reaches the loopback mapping as it is
and brings TLS.

Stderr carries Mezame's own logs. One environment variable is worth knowing:

- `MEZAME_SKIP_UI_BUILD=1` tells `build.rs` not to run the UI build. The binary
  is then missing its UI; useful only for Rust-only iteration.

## Known gaps

1. **One provider, no tools.** This build talks to Amazon Bedrock and
   nothing else; a second provider arrives with the provider seam's next
   tenant. The model can read and write nothing but the conversation: no
   file access, no shell, no web, so a prompt that asks for those gets an
   answer in words.
2. **Accounts are coarse.** There is a login, per-account sessions and two
   roles, and passwords are managed from the terminal alone. Nothing yet
   distinguishes an admin from a user, and there is no interface for
   sharing a provider credential between accounts. On a plain-HTTP bind the
   cookie crosses the network readable, so a public hostname belongs behind
   TLS; validating Cloudflare Access's `Cf-Access-Jwt-Assertion` header as
   a second factor at the edge remains on the list.
3. **What persists, and what does not.** Conversations, sessions, users,
   settings and the Bedrock credential live in the datastore and survive a
   restart. Still memory-only: a turn in flight when the server stops is
   cut short, and notices (the grey `sys` lines) are not stored, so a
   reload shows the conversation without them.

## Roadmap

None of these ship today, and none block the core loop.

1. **A second provider.** The Anthropic API directly, then others, behind
   the same `Provider` trait, each credential a row in the datastore as the
   Bedrock one already is.
2. **A profile editor and grants.** The model, thinking mode, budget and
   reply ceiling editable per profile in the browser, and provider
   credentials shareable between accounts.
3. **Tools and workspaces.** File reads and writes, shell commands, and an
   approval flow in the browser, scoped to a directory you nominate.
4. **Telegram transport.** Not implemented: `TransportConfig` carries a
   commented-out `Telegram` variant and `mezame init` offers no such option.
   Planned shape: long-poll `getUpdates`, one session per chat, stream chunks
   as `editMessageText` throttled to about one per second, inline keyboard for
   approvals.

## Troubleshooting

**`cargo build` fails with "npm not found"**
`build.rs` requires `node` and `npm`. Install Node.js 24 or newer and retry.
`MEZAME_SKIP_UI_BUILD=1` lets the Rust build complete without Node.js, but the
resulting binary is missing its UI.

**`cargo build` fails naming a Node.js version**
The UI build needs Node.js 24 or newer. Check `node --version` and upgrade.

**No config at `~/.mezame/config.json`**
Run `mezame init`. It writes the file after its questions. With no terminal
attached (a service manager, `docker compose up -d`) it exits non-zero, writes
nothing, and the manager restarts it until the file exists; the log names the
way out. Run `mezame init --bind ADDR --admin NAME --password-stdin` once, as
the account the service runs under, and start it again.

**The server refuses to start naming `config.json` and a version**
An earlier release wrote the file. Run `mezame init`; it rewrites the file at
version 2 and keeps the hosts.

**The server refuses to start naming a `bedrock` section**
The model, region and profile live in the datastore now. Run
`mezame init --model ID [--region R] [--profile P]`; it writes the rows and
drops the section from the file.

**A forgotten password**
`mezame passwd NAME` sets a new one from the terminal, as the account Mezame
runs under. There is no reset flow in the browser.

**Logged out on every device after a password change**
By design: a password change ends every session cookie the account had. Sign
in again with the new password.

**The server refuses to start naming `master.key` and a mode or length**
The key must be a regular file of exactly 32 bytes, readable by its owner
alone. `chmod 600 ~/.mezame/master.key` fixes the mode. A file of another
length is not the key: restore the real one from backup.

**`mezame.db` exists but `master.key` is missing**
Nothing sealed in the datastore can be opened without its key, so the server
stops rather than quietly making a new one. Restore `~/.mezame` from its
backup; or run `mezame init` to create a new key and re-enter the Bedrock
credential, which drops the credential rows the old key sealed and says how
many.

**A model, region or profile changed with `mezame init` is not in force**
The running server reads the profile and its credential once, at startup.
Restart it.

**Browser connects, the composer is read-only**
A turn is in flight on that session, started here or on another device. It
unlocks when that turn ends.

**The turn fails with "AWS refused the credentials"**
The keys or the session token the SDK found were rejected at AWS's front
door: a mistyped secret, an expired session token, or a profile whose
credentials have lapsed. The message is fixed on purpose: AWS's own text can
quote the signed request, token included. Check the variables or the profile
the `Backend:` line names.

**The turn fails with "The model could not read the reasoning recorded earlier"**
The model signs its reasoning against the conversation that produced it and
refused a replay whose prefix had changed (an evicted turn, or a model switch
on an account that enforces the binding). Mezame drops the reasoning and
retries once on its own; this message means the retry failed too. Send the
message again; the conversation continues without the earlier reasoning.

**The turn fails with "Bedrock refused the request"**
Access was denied. Either the model is not enabled for this account in this
region (Bedrock console, Model access), or the credentials the SDK found lack
`bedrock:InvokeModelWithResponseStream` on it, or the SDK picked up a different
profile than you meant. The startup lines no longer name the region or
profile; re-run `mezame init` to see and keep the current values, and
`aws sts get-caller-identity --profile <name>` shows who the credentials are.

**The turn fails naming "on-demand throughput"**
The model id names a version Bedrock does not serve on demand in this region.
Use the cross-region id (the `global.` or `us.` prefix, as the model card
lists it), or a version that has on-demand throughput, and set it with
`mezame init --model <id>`; then restart.

**The turn fails with "No AWS region is set"**
Neither the config, `AWS_REGION` nor the profile names a region, so the SDK
has nowhere to send the request. Startup does not check this; the first turn
does. Run `mezame init --region us-east-1` (or yours) and restart, or export
`AWS_REGION`.

**The turn fails with "No AWS credentials were found"**
Startup does not resolve credentials; the first turn does, and the SDK's chain
came up empty under the account Mezame runs as. Run
`aws configure` or `aws sso login` as that account, or set `AWS_PROFILE` or the
`AWS_ACCESS_KEY_ID` variables in its environment; for a service unit see
[Running as a service](./docs/service.md).

**Cloudflare hostname returns 502**
The `cloudflared` machine cannot reach the Mezame machine. Check that
`service: http://<host>:9510` in the ingress rule resolves and that the port is
open on the Mezame host.

**WebSocket closes immediately**
Cloudflare Access policy is rejecting the upgrade. Hit the hostname in a
browser first to satisfy Access, then retry.

**A new tab stays on "reconnecting" and the server answers `/ws` with 503**
Mezame holds at most 128 live sessions, each kept for 30 seconds after its
last browser leaves. Something is opening sessions faster than they expire;
existing tabs keep working, and the new one connects once a slot frees.

**Every request answers 421**
Mezame serves only hostnames it has been told about: IP addresses,
`localhost`, `.localhost` and `.local` names, and the host part of `bind`. Any
other name you reach it by, a tunnel or proxy hostname, a router-assigned name,
a `.lan` or `.home.arpa` name, a Tailscale MagicDNS name, goes under `hosts` in
the transport entry of `~/.mezame/config.json`; then restart Mezame. Step 5 of
the Cloudflare guide shows the shape.

**The browser gets 403 on the WebSocket or when saving state**
The page was served from a host and port other than the one it is talking to: a
proxy that rewrites `Host`, or a dev server on another port. List the page's
hostname under `hosts`, or serve the page from the host it talks to.

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
