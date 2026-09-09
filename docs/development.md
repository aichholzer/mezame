# Development

Same prerequisites as a normal build, on Linux or macOS: a stable Rust
toolchain and Node.js 24 or newer with `npm` on `PATH`. See the README for
install details.

## Build, check, lint

```sh
cargo check
cargo build --release
cargo clippy --all-targets -- -D warnings   # gate on this in CI
cargo +1.94.1 check --all-targets --locked  # the compiler floor, as CI's msrv job runs it
```

The floor is the `rust-version` in `Cargo.toml`, which the AWS SDK crates set;
`rustup toolchain install 1.94.1` once, and `MEZAME_SKIP_UI_BUILD=1` keeps the
check to Rust.

Inside `ui/`:

```sh
cd ui
npm ci
npm run build   # emits ui/dist/ (local only; cargo build produces its own copy in $OUT_DIR)
```

## Development loop

Two terminals:

```sh
# terminal 1: Rust on :9510
cargo run --release

# terminal 2: Vite with HMR on :5173, proxies /ws and the HTTP endpoints
cd ui
npm run dev
```

Browse `http://127.0.0.1:5173`. The embedded bundle is only relevant when you run the release binary directly.

Do not add `changeOrigin` to the proxy entries in `ui/vite.config.ts`: the
server refuses an upgrade or a write whose `Origin` names a different host or
port from `Host`, and Vite rewrites `Host` when that flag is on. Browse the dev
server by `127.0.0.1` or `localhost`, which the `Host` allowlist serves without
configuration.

## Where to add things

| Change                                | File and function                                                                                            |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| What produces a turn                  | implement `Backend` in `src/backend.rs`; `build_hub` in `src/hub.rs` picks the one a session gets             |
| New browser to Mezame message type     | `parse_browser_command` in `src/ws.rs` (parse it) and `handle_command` in `src/hub.rs` (act on it)          |
| New Mezame to browser message type     | Stream it from a Backend, or emit it from the hub loop in `src/hub.rs`; type in `ui/src/types.ts`; handle in `handleMessage` in `ui/src/hooks/useMezame.ts` |
| New transcript entry shape            | `EntryBody` in `src/backend.rs`; `entries_from_row` in `src/history.rs`; the history branch of `loadHistory` in `ui/src/hooks/useMezame.ts` |
| New table or column                   | a new numbered file under `src/store/migrations/` (never an edit of an applied one), a row type and trait method in `src/store/mod.rs`, the implementation in `src/store/sqlite.rs` |
| Auth middleware                       | `guard_request` in `src/guard.rs` is the one layer in front of every route (`.layer(...)` in `build_router`, `src/http.rs`); an identity check goes beside it, after the `Host` and `Origin` checks |
| New transport (telegram, matrix, ...) | add a variant to `TransportConfig` in `src/config.rs` and an arm in `run` (`src/lib.rs`); implement a sibling module |
| UI tweak                              | edit under `ui/src/`; `npm run dev` for HMR or full `cargo build` for the embedded path                      |

## Testing

The suite lives in `tests/` (Rust integration tests) and `tests/ui/` plus
`ui/src/**` (Vitest). Run the Rust side with `cargo test --all-targets` and
the UI side with `npm test` in `ui/`, which type-checks the suites against
`ui/src/types.ts` (`tsconfig.test.json`) and then runs vitest. CI also enforces
a coverage floor via
`cargo llvm-cov` (see `.github/workflows/ci.yml`), passes `--locked` so a
`Cargo.toml` edit without its `Cargo.lock` fails on every push, and fails the
`docs` job if `cargo package --list` names a repository-only file.

`.github/workflows/container.yml` builds the image, sets it up through
`mezame init` with the flags (the admin's password on standard input), serves
it, logs in with `curl` and reads `/`, `/state` and `/me` through the
published port. It runs when a file the image depends on changes, weekly,
and on demand; the schedule fires only on the default branch. Locally,
`tests/container_files.rs` pins the shape of the Dockerfile, `compose.yaml`
and the build-context allowlist on every `cargo test`. The base images are
pinned by digest; the Dockerfile's header has the recipe for reading a current
digest from the registry without Docker.

The datastore in tests is `SqliteStore::open_in_memory()`: the real
implementation, the real migrations, no file. The store runs on one thread
of its own and the async side reaches it over a channel, so no SQLite handle
ever crosses an await; keep SQL inside `src/store/` and reach the rest of
the crate through the `Store` trait. The binary suites (`tests/cli_*.rs`)
are the exception: they run `mezame` as a child process against a temporary
`HOME`, where a real file is the point. `FailingStore` and `CountingStore`
in `tests/support/mod.rs` wrap a real store to script failures and count
calls.

Most integration tests drive the hub through `ScriptedBackend` in
`tests/support/mod.rs`: a Backend whose every answer the test supplies up
front, and whose invocation log the test reads back while a turn is still
open. Its sibling `ScriptedProvider` does the same one layer down, for the
turn loop: a Provider whose stream the test scripts as `TurnEvent`s, or holds
open until released, or fails before the first event, and which records every
request it was handed so a test can read the messages, the model and the
settings back. Neither reaches a process, a socket or a file, and both are
compiled into the test binaries that declare `mod support;` and into nothing
else.

Nothing in `cargo test` needs AWS credentials. The Bedrock code is covered by
feeding the normaliser events built with the SDK's own builders
(`tests/provider_bedrock.rs`), so the request shape, the stop reasons, the
usage figures and the error classes are pinned without a network. Three
`#[ignore]` cases in `tests/live_bedrock.rs` do call a real model, by hand:

```sh
MEZAME_LIVE_BEDROCK_MODEL=global.anthropic.claude-sonnet-5 \
  cargo test --test live_bedrock -- --ignored --nocapture
```

`MEZAME_LIVE_BEDROCK_REGION` and `MEZAME_LIVE_BEDROCK_PROFILE` narrow where
they run; with the model variable unset each case prints why it skipped and
passes. `ci.yml` runs no `--ignored` test and holds no AWS secret.

Notable coverage already in place:

- **Config paths and load.** `tests/config_paths.rs` covers the paths under
  `~/.mezame` and `load_config` including the error branches;
  `tests/config_v2.rs` the version gate and the v2 keys; `tests/config_fs.rs`
  the owner-only directory and file writers; `tests/cli_init.rs` the whole
  `mezame init` flag path on the binary; `tests/cli_users.rs` the user
  commands and the password-change epoch through a served binary.
- **The seam.** `tests/backend.rs` covers the `EchoBackend`, the echo text
  derivation, the session id form, and the upgrade decision. Session id
  uniqueness is bounded past one process run, so that case re-executes its own
  test binary twice.
- **The provider.** `tests/provider_bedrock.rs` feeds the normaliser
  builder-made stream events and checks the request builder, the thinking
  rule per model id, the cache point, the block conversion and the error
  classifier. `tests/prompt.rs` and `tests/conversation.rs` cover the system
  prompt and the conversation budget, restore included.
- **The loop.** `tests/turn_loop.rs` drives `LoopBackend` through
  `ScriptedProvider`: the mapping and limits, cancel before and during the
  stream, the idle timeout, refusals and filtered replies, the stop and usage
  ordering, and the log line. `tests/persistence.rs` covers what the loop
  writes to the store, the rebuild of a session from its rows, and `/history`
  served from them. `tests/store.rs` and `tests/store_crypto.rs` cover the
  store itself and the key. `tests/cli_binary.rs` starts the binary over a
  datastore the flags set up, with no credentials, and reads the `Backend:`
  and `Datastore:` lines. `tests/auth.rs`, `tests/http_auth.rs` and
  `tests/http_sessions.rs` cover the cookie, the login and the per-account
  session routes.
- **Hub plumbing.** `tests/hub.rs` drives the multi-attach hub: broadcast
  fan-out, `_target` stamping, the grace counter and its capped in-flight hold,
  the frames that end a turn, and the mid-turn second-prompt drop.
- **The socket.** `tests/ws_heartbeat.rs` covers half-open eviction, targeted
  forwarding, the transport-error and `Close` exits, the eviction of a peer
  that stops reading, and the writer's write timeout; `tests/ws_commands.rs`
  the four discarded-frame faults and the exits on a closed stream, inbox or
  broadcast channel plus the lag that is not an exit; and `tests/ws_upgrade.rs`
  the three upgrade arms, the message and text ceilings, the session cap and
  the `Host` and `Origin` refusals over a real socket, which is the only way to
  reach the extractor axum's upgrade needs.
- **The request checks.** `tests/guard.rs` drives `RequestPolicy` directly; the
  guard cases in `tests/http_routes.rs` and `tests/ws_upgrade.rs` cover the 421
  and 403 answers over the router and over a real socket.
- **The container.** `tests/container_files.rs` pins the Dockerfile, the
  compose file and the build-context allowlist as text.
- **Invariants.** `tests/properties.rs` holds seventeen `proptest`
  properties at 100 cases each: the nine of alpha.1 (broadcast fidelity,
  targeted delivery, turn ordering, the in-flight trajectory, grace and
  shutdown, session ids, the echo agreement, the serialisation shape, and
  the `busy` pairing), five for the Bedrock line (the normaliser's text is
  the text the builders were fed, a request built from any conversation
  alternates roles from `user`, the thinking rule is total over model ids
  and agrees with its table, the conversation and the transcript evict
  together, and the system prompt's assembly is a function of its inputs),
  and three for this alpha (a conversation written through the store and
  loaded back is the same conversation, any single-byte change to a cookie
  fails its check, and the login limiter admits exactly ten per window).
  Each is tagged with the design property it validates. The async ones run
  on a paused clock, which is what keeps them cheap.

## Debugging

- `cargo run --release` prints the bind address on stderr.
- A discarded browser frame writes one line to stderr naming its `type`, or
  naming the frame as unparseable. A prompt dropped because a turn was already
  in flight writes one naming the session.
- A refused `Host` or `Origin` writes one line to stderr, once per distinct
  value and at most 64 values per run (`src/guard.rs`); the response body names
  the value refused. A failed datastore write inside a turn is reported once
  per session with the operation named; the turn resolves and the session
  runs on in memory.
- Browser devtools, Network, WS view shows every frame in both directions.
- `curl -b 'mezame_session=<cookie>' 'http://127.0.0.1:9510/history?session=<id>'`
  shows what a reload would seed the log from; the cookie is in the browser's
  devtools under Application, Cookies.

## Releasing

`.github/workflows/publish.yml` publishes to crates.io. It is
`workflow_dispatch` only: run it from the Actions tab, on `main`.

Before dispatching, three things have to line up, and the workflow refuses
the run if any of them does not:

- `Cargo.toml` and `ui/package.json` agree on the version.
- `CHANGELOG.md` has a `## [version]` section. Its body becomes the
  GitHub release notes.
- That version is not on crates.io already.

The workflow runs the whole of `ci.yml` first, then packages the crate,
compiles the packaged sources, publishes, and creates a GitHub release
tagged with the bare version number.

`CARGO_REGISTRY_TOKEN` is the only secret it needs beyond the automatic
`GITHUB_TOKEN`.
