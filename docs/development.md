# Development

Same prerequisites as a normal build: Rust toolchain, Node.js 22+, and an ACP-capable agent on `$PATH` if you want to exercise the full path. See the main README for install details.

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
| New `session/update` variant          | `handle_agent_message` in `src/ws.rs`                                                                        |
| New browser to Mezame message type     | `parse_browser_command` in `src/ws.rs` (parse it) and `handle_command` in `src/hub.rs` (act on it)          |
| New Mezame to browser message type     | Emit from the hub loop in `src/hub.rs`; type in `ui/src/types.ts`; handle in `handleMessage` in `ui/src/hooks/useMezame.ts` |
| Auth middleware                       | wrap `Router` in `build_router`/`run_cloudflared` (`src/http.rs`) or apply to the `/ws` route                |
| New transport (telegram, matrix, ...) | add a variant to `TransportConfig` in `src/config.rs` and an arm in `run` (`src/lib.rs`); implement a sibling module |
| UI tweak                              | edit under `ui/src/`; `npm run dev` for HMR or full `cargo build` for the embedded path                      |

## Testing

The suite lives in `tests/` (Rust integration tests) and `tests/ui/` plus
`ui/src/**` (Vitest). Run the Rust side with `cargo test --all-targets` and
the UI side with `npm test` in `ui/`. CI also enforces a coverage floor via
`cargo llvm-cov` (see `.github/workflows/test.yml`).

Notable coverage already in place:

- **Config paths and load.** `tests/config_paths.rs` covers `config_path`,
  `state_path`, and `load_config` including the error branches.
- **JSON-RPC routing.** `tests/agent_jsonrpc.rs` feeds the reader task a mix
  of responses (by id), notifications, and malformed lines and asserts the
  routing.
- **Hub plumbing.** `tests/hub.rs` drives the multi-attach hub (broadcast,
  targeting, grace counter) via `register_for_test`.
- **WS dispatch.** `tests/ws_handle_agent_message.rs`, `ws_negotiate_session.rs`,
  and `ws_select_loop.rs` cover the agent-message dispatch table, the
  handshake, and the browser-command branches.
- **History parser.** `tests/http_parse_kiro_history.rs` feeds
  `parse_kiro_history` representative `.jsonl` fragments and asserts
  timestamps propagate from `Prompt` to the following `AssistantMessage`
  entries.

## Debugging

- `cargo run --release` prints the bind address on stderr and forwards the child's stderr with `[agent]` prefix.
- `MEZAME_DEBUG_ACP=1` shows every inbound JSON line with `[acp<-]`.
- Run the agent manually first to confirm the invocation works: `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | kiro-cli acp` should respond with JSON.
- Browser devtools, Network, WS view shows every frame in both directions.
