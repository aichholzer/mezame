# Development

Same prerequisites as a normal build: a stable Rust toolchain and Node.js 24
or newer with `npm` on `PATH`. See the README for install details.

## Build, check, lint

```sh
cargo check
cargo build --release
cargo clippy --all-targets -- -D warnings   # gate on this in CI
```

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

# terminal 2: Vite with HMR on :5173, proxies /ws and /state
cd ui
npm run dev
```

Browse `http://127.0.0.1:5173`. The embedded bundle is only relevant when you run the release binary directly.

## Where to add things

| Change                                | File and function                                                                                            |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| What produces a turn                  | implement `Backend` in `src/backend.rs`; `build_hub` in `src/hub.rs` picks the one a session gets             |
| New browser to Mezame message type     | `parse_browser_command` in `src/ws.rs` (parse it) and `handle_command` in `src/hub.rs` (act on it)          |
| New Mezame to browser message type     | Stream it from a Backend, or emit it from the hub loop in `src/hub.rs`; type in `ui/src/types.ts`; handle in `handleMessage` in `ui/src/hooks/useMezame.ts` |
| New transcript entry shape            | `EntryBody` in `src/backend.rs`; the history branch of `loadHistory` in `ui/src/hooks/useMezame.ts`           |
| Auth middleware                       | wrap `Router` in `build_router`/`run_cloudflared` (`src/http.rs`) or apply to the `/ws` route                |
| New transport (telegram, matrix, ...) | add a variant to `TransportConfig` in `src/config.rs` and an arm in `run` (`src/lib.rs`); implement a sibling module |
| UI tweak                              | edit under `ui/src/`; `npm run dev` for HMR or full `cargo build` for the embedded path                      |

## Testing

The suite lives in `tests/` (Rust integration tests) and `tests/ui/` plus
`ui/src/**` (Vitest). Run the Rust side with `cargo test --all-targets` and
the UI side with `npm test` in `ui/`. CI also enforces a coverage floor via
`cargo llvm-cov` (see `.github/workflows/ci.yml`), passes `--locked` so a
`Cargo.toml` edit without its `Cargo.lock` fails on every push, and fails the
`docs` job if `cargo package --list` names a repository-only file.

`.github/workflows/container.yml` builds the image, writes a config through
`mezame init --bind`, serves it and reads `/` and the request checks through
the published port. It runs when a file the image depends on changes, weekly,
and on demand; the schedule fires only on the default branch. Locally,
`tests/container_files.rs` pins the shape of the Dockerfile, `compose.yaml`
and the build-context allowlist on every `cargo test`. The base images are
pinned by digest; the Dockerfile's header has the recipe for reading a current
digest from the registry without Docker.

Most integration tests drive the hub through `ScriptedBackend` in
`tests/support/mod.rs`: a Backend whose every answer the test supplies up
front, and whose invocation log the test reads back while a turn is still
open. It reaches no process, no socket and no file, and it is compiled into
the test binaries that declare `mod support;` and into nothing else.

Notable coverage already in place:

- **Config paths and load.** `tests/config_paths.rs` covers `config_path`,
  `state_path`, and `load_config` including the error branches;
  `tests/config_compat.rs` pins that a 0.13.x file loads unchanged and serves
  its bind; `tests/config_fs.rs` covers the owner-only directory and file
  writers; `tests/cli_init.rs` covers `mezame init --bind`; and
  `tests/http_state_writes.rs` covers `/state` under concurrent and failing
  writes.
- **The seam.** `tests/backend.rs` covers the shipped `EchoBackend`, the echo
  text derivation, the session id form, and the upgrade decision. Session id
  uniqueness is bounded past one process run, so that case re-executes its own
  test binary twice.
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
- **Invariants.** `tests/properties.rs` holds nine `proptest` properties at 100
  cases each: broadcast fidelity, targeted delivery, turn ordering, the
  in-flight trajectory, grace and shutdown, session ids, the echo agreement,
  the serialisation shape, and the `busy` pairing. Each is tagged with the
  design property it validates. The async ones run on a paused clock, which is
  what keeps them cheap.

## Debugging

- `cargo run --release` prints the bind address on stderr.
- A discarded browser frame writes one line to stderr naming its `type`, or
  naming the frame as unparseable. A prompt dropped because a turn was already
  in flight writes one naming the session.
- Browser devtools, Network, WS view shows every frame in both directions.
- `curl 'http://127.0.0.1:9510/history?session=<id>'` shows what a reload would
  seed the log from.

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
