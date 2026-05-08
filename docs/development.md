# Development

Same prerequisites as a normal build: Rust toolchain, Node.js 24+, and an ACP-capable agent on `$PATH` if you want to exercise the full path. See the main README for install details.

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

# terminal 2: Vite with HMR on :5173, proxies /ws, /state, /history
cd ui
npm run dev
```

Browse `http://127.0.0.1:5173`. The embedded bundle is only relevant when you run the release binary directly.

## Where to add things

| Change                                | File and function                                                                                            |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| New `session/update` variant          | `handle_agent_message` in `src/ws.rs`                                                                        |
| New browser to Okiro message type     | WS `select!` arms in `handle_ws` in `src/ws.rs`                                                              |
| New Okiro to browser message type     | Emit from the WS handler; type in `ui/src/types.ts`; handle in `handleMessage` in `ui/src/hooks/useOkiro.ts` |
| Auth middleware                       | wrap `Router` in `run_cloudflared` (`src/http.rs`) or apply to the `/ws` route                               |
| New transport (telegram, matrix, ...) | add a variant to `TransportConfig` in `src/config.rs` and an arm in `main.rs`; implement a sibling module    |
| UI tweak                              | edit under `ui/src/`; `npm run dev` for HMR or full `cargo build` for the embedded path                      |

## Testing

There are no tests yet. Practical coverage to add:

- **Config round-trip.** Property test that `Config` survives `serde_json::to_string_pretty` to `serde_json::from_str`.
- **JSON-RPC routing.** Unit test the reader task: feed it a mix of responses (by id), notifications, and malformed lines, assert the right routing.
- **End-to-end smoke.** Spawn Okiro with `agent_cmd = "bash"` and `agent_args = ["-c", "<echo canned JSON-RPC>"]` as a fake agent.
- **History parser.** Feed `parse_kiro_history` a representative `.jsonl` fragment; assert timestamps propagate from `Prompt` to the subsequent `AssistantMessage` entries.

## Debugging

- `cargo run --release` prints the bind address on stderr and forwards the child's stderr with `[agent]` prefix.
- `OKIRO_DEBUG_ACP=1` shows every inbound JSON line with `[acp<-]`.
- Run the agent manually first to confirm the invocation works: `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | kiro-cli acp` should respond with JSON.
- Browser devtools, Network, WS view shows every frame in both directions.
