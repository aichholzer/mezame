//! racp — bridge an ACP-compliant local agent to a remote UI.
//!
//! # Overview
//!
//! racp is an **ACP client**. For each incoming browser WebSocket it spawns a
//! fresh agent subprocess (configured via `agent_cmd` + `agent_args`), sends
//! `initialize` followed by `session/new`, then forwards each user message as
//! `session/prompt` and streams `session/update` notifications back to the
//! browser as terminal-style text.
//!
//! One WebSocket connection = one agent subprocess = one ACP session. When the
//! browser disconnects the agent is killed (via `kill_on_drop(true)`).
//!
//! # Transports
//!
//! - `cloudflared`: HTTP + WebSocket on `127.0.0.1:<bind>` fronted by an
//!   existing Cloudflare Tunnel. The terminal-style UI is embedded via
//!   `include_str!("ui.html")` so the binary is self-contained.
//! - `telegram`: stub. See [`run_telegram`]; schema is stable so implementing
//!   it later does not break existing configs.
//!
//! # Wire shapes
//!
//! Browser ↔ racp (JSON over WS):
//!   { type: "prompt", text: string }                           // client→server
//!   { type: "append", role: "user"|"agent"|"sys", text: str }  // server→client
//!   { type: "prompt_done" }                                    // server→client
//!   { type: "error", message: string }                         // server→client
//!
//! racp ↔ agent (JSON-RPC 2.0 over stdio): standard ACP. See the README for
//! the list of methods and `session/update` variants we actually handle, and
//! which ones Kiro emits vs ignores.
//!
//! # Known gaps
//!
//! Search for `TODO:` to find the extension points. The README has the same
//! list with more context for new contributors.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State
    },
    response::{Html, Response},
    routing::get,
    Router
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

const PROTOCOL_VERSION: u32 = 1;
const UI_HTML: &str = include_str!("ui.html");

// ---------- config ----------
//
// On-disk config lives at `~/.racp/config.toml`. Schema changes are breaking
// for existing users, so add fields with `#[serde(default)]` rather than
// reshuffling. The `Transport` enum gates which `run_*` function main() calls.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    transport: Transport,
    bind: String,
    agent_cmd: String,
    #[serde(default)]
    agent_args: Vec<String>,
    #[serde(default)]
    telegram: TelegramConfig
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Transport {
    Cloudflared,
    Telegram
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TelegramConfig {
    #[serde(default)]
    token: String
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".racp/config.toml"))
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).context("parsing config.toml")?;
    Ok(cfg)
}

fn prompt_line(msg: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn init_config() -> Result<Config> {
    println!();
    println!("Which transport?");
    println!("  1) cloudflared  (serve a terminal-like web UI; front with your tunnel)");
    println!("  2) telegram     (long-poll a Telegram bot)  [not yet implemented]");
    let transport = loop {
        match prompt_line("> ")?.as_str() {
            "1" | "cloudflared" => break Transport::Cloudflared,
            "2" | "telegram" => break Transport::Telegram,
            _ => println!("pick 1 or 2")
        }
    };

    let bind = {
        let s = prompt_line("bind address [127.0.0.1:7842]: ")?;
        if s.is_empty() {
            "127.0.0.1:7842".to_string()
        } else {
            s
        }
    };

    let agent_cmd = {
        let s = prompt_line("ACP agent command (e.g. kiro-cli, claude, gemini, codex): ")?;
        if s.is_empty() {
            bail!("agent command is required");
        }
        s
    };

    let args_raw = prompt_line("agent args (space-separated) []: ")?;
    let agent_args: Vec<String> = args_raw.split_whitespace().map(str::to_string).collect();

    let telegram = if transport == Transport::Telegram {
        let token = prompt_line("telegram bot token: ")?;
        TelegramConfig { token }
    } else {
        TelegramConfig::default()
    };

    let cfg = Config {
        transport,
        bind,
        agent_cmd,
        agent_args,
        telegram
    };

    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
    println!("wrote {}", path.display());
    println!();
    Ok(cfg)
}

// ---------- entry ----------
//
// Kept synchronous on purpose: init_config() reads stdin and we do not want a
// tokio runtime blocking a thread on that. We only build the runtime once we
// know which transport to run.

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str);

    if sub == Some("init") {
        init_config()?;
        return Ok(());
    }

    let cfg = if config_path()?.exists() {
        load_config()?
    } else {
        eprintln!("no config at {}", config_path()?.display());
        eprintln!("let's set one up:");
        init_config()?
    };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        match cfg.transport {
            Transport::Cloudflared => run_cloudflared(cfg).await,
            Transport::Telegram => run_telegram(cfg).await
        }
    })
}

// ---------- cloudflared transport ----------
//
// axum serves the embedded UI at `/` and accepts WS upgrades at `/ws`. We bind
// on loopback only; public reachability is the Cloudflare Tunnel's job.
//
// TODO(auth): validate the `Cf-Access-Jwt-Assertion` header on /ws before
// allowing the upgrade. The header is injected by Cloudflare Access; its
// signing keys are at
//   https://<team>.cloudflareaccess.com/cdn-cgi/access/certs

async fn run_cloudflared(cfg: Config) -> Result<()> {
    let shared = Arc::new(cfg.clone());
    let app = Router::new()
        .route("/", get(serve_ui))
        .route("/ws", get(ws_upgrade))
        .with_state(shared);

    let listener = TcpListener::bind(&cfg.bind).await?;
    eprintln!("racp listening on http://{} (front with your Cloudflare Tunnel)", cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_ui() -> Html<&'static str> {
    Html(UI_HTML)
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(cfg): State<Arc<Config>>) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_ws(socket, cfg).await {
            eprintln!("ws session ended: {e:?}");
        }
    })
}

/// Per-WebSocket session.
///
/// Ownership and concurrency:
///
/// - The WS is split into a `sink` (owned by a writer task) and a `stream`
///   (polled directly in the select loop).
/// - Sends to the browser go through an unbounded mpsc so handlers never
///   contend on the sink directly.
/// - The agent subprocess is spawned once per session and wrapped in `Arc`
///   so both the select loop and spawned prompt tasks can call into it.
/// - Prompts are run in their own spawned tasks so a long-running
///   `session/prompt` does not block the select loop from draining
///   `session/update` notifications.
async fn handle_ws(ws: WebSocket, cfg: Arc<Config>) -> Result<()> {
    let (mut sink, mut stream) = ws.split();
    let (to_ws_tx, mut to_ws_rx) = mpsc::unbounded_channel::<Message>();

    // Writer task: drain the outbound channel into the WS sink. Exits when
    // the channel is closed (all senders dropped) or the sink errors.
    let writer = tokio::spawn(async move {
        while let Some(msg) = to_ws_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    send_sys(&to_ws_tx, "starting agent...\n")?;

    // If spawn fails (bad agent_cmd, missing binary, ...) tell the browser
    // and close cleanly. Do NOT return an Err here: the writer task still
    // needs to drain the error message before we exit.
    let agent = match spawn_agent(&cfg).await {
        Ok(a) => Arc::new(a),
        Err(e) => {
            let _ = to_ws_tx.send(text_msg(json!({ "type": "error", "message": format!("{e}") })));
            drop(to_ws_tx);
            let _ = writer.await;
            return Ok(());
        }
    };

    // ACP handshake. `initialize` advertises no filesystem capabilities
    // because racp does not back `fs/read_text_file` etc. today — the agent
    // is expected to use its own tools for file I/O.
    agent
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false }
                }
            })
        )
        .await
        .context("initialize")?;

    // `cwd` is racp's current working directory, which is what the agent
    // will see as the project root. For a hosted deployment, consider
    // making this configurable per session (e.g. a workspace parameter
    // coming from the browser).
    let cwd = std::env::current_dir()?;
    let new_session = agent
        .request(
            "session/new",
            json!({
                "cwd": cwd.to_string_lossy(),
                "mcpServers": []
            })
        )
        .await
        .context("session/new")?;
    let session_id = new_session
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("session/new returned no sessionId"))?
        .to_string();

    send_sys(&to_ws_tx, "ready.\n")?;

    // Hand over the single updates receiver produced by `spawn_agent`. Only
    // one task may own it; we own it here, for the life of the session.
    let mut updates_rx = agent.take_updates();

    loop {
        tokio::select! {
            // User → agent: messages from the browser.
            Some(Ok(msg)) = stream.next() => {
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue
                };
                if v.get("type").and_then(Value::as_str) != Some("prompt") {
                    continue;
                }
                let Some(user_text) = v.get("text").and_then(Value::as_str) else {
                    continue;
                };

                // Run `session/prompt` in its own task so the select loop
                // keeps pumping `session/update` notifications while the
                // agent is working. When the request resolves we tell the
                // browser the turn is over (or surface the error).
                let agent = agent.clone();
                let to_ws = to_ws_tx.clone();
                let sid = session_id.clone();
                let user_text = user_text.to_string();
                tokio::spawn(async move {
                    let res = agent
                        .request(
                            "session/prompt",
                            json!({
                                "sessionId": sid,
                                "prompt": [{ "type": "text", "text": user_text }]
                            })
                        )
                        .await;
                    if let Err(e) = res {
                        let _ = to_ws.send(text_msg(json!({ "type": "error", "message": format!("{e}") })));
                    }
                    let _ = to_ws.send(text_msg(json!({ "type": "prompt_done" })));
                });
            }
            // Agent → user: notifications and server-initiated requests.
            Some(agent_msg) = updates_rx.recv() => {
                handle_agent_message(&to_ws_tx, &agent, agent_msg).await;
            }
            else => break
        }
    }

    // Closing the outbound channel unblocks the writer task. The agent
    // child is killed on drop of the `Agent` (see `kill_on_drop(true)` in
    // `spawn_agent`).
    drop(to_ws_tx);
    let _ = writer.await;
    Ok(())
}

/// Translate an agent-originated message into browser-facing events.
///
/// Currently understood:
///
/// - `session/update`:
///     - `agent_message_chunk`   → append as `agent` text
///     - `agent_thought_chunk`   → append as `sys` with a `(thinking)` prefix
///     - `tool_call` / `tool_call_update` → append `[title — status]`
/// - `session/request_permission` → auto-allow the first offered option
///
/// Everything else is silently dropped, including Kiro's `_kiro.dev/*`
/// extension notifications (slash-command catalogue, MCP OAuth URLs, etc.).
/// Wire those up here if you want them surfaced — each has its own
/// `method` name and params shape; see the Kiro CLI ACP docs.
///
/// TODO(permission-ui): replace the auto-allow with a real prompt. The browser
/// protocol needs a new message type carrying the toolCall summary and the
/// option list, and a corresponding response type selecting an option.
async fn handle_agent_message(tx: &mpsc::UnboundedSender<Message>, agent: &Agent, msg: Value) {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "session/update" => {
            let update = msg.get("params").and_then(|p| p.get("update")).cloned().unwrap_or(Value::Null);
            let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
            match kind {
                "agent_message_chunk" => {
                    if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(Value::as_str) {
                        let _ = tx.send(text_msg(json!({ "type": "append", "role": "agent", "text": text })));
                    }
                }
                "agent_thought_chunk" => {
                    // Reasoning tokens. Kiro does not currently emit these,
                    // but leave the handler in place so reasoning-model
                    // agents light up the UI with `(thinking)` lines.
                    if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(Value::as_str) {
                        let _ = tx.send(text_msg(json!({ "type": "append", "role": "sys", "text": format!("(thinking) {text}") })));
                    }
                }
                "tool_call" | "tool_call_update" => {
                    // Single-line "[title — status]" gives users visibility
                    // into long tool runs. Extend this to render structured
                    // tool IO if you want richer UI.
                    let title = update.get("title").and_then(Value::as_str).unwrap_or("tool");
                    let status = update.get("status").and_then(Value::as_str).unwrap_or("");
                    let line = if status.is_empty() {
                        format!("\n[{title}]\n")
                    } else {
                        format!("\n[{title} — {status}]\n")
                    };
                    let _ = tx.send(text_msg(json!({ "type": "append", "role": "sys", "text": line })));
                }
                _ => {}
            }
        }
        "session/request_permission" => {
            // Auto-allow for the sketch. We pick the first offered option,
            // which for most agents is some form of "allow once". If the
            // agent offers only a denial option first, this will deny —
            // that is also a reason to build a real permission UI.
            if let Some(id) = msg.get("id").cloned() {
                let option_id = msg
                    .get("params")
                    .and_then(|p| p.get("options"))
                    .and_then(Value::as_array)
                    .and_then(|opts| opts.first())
                    .and_then(|o| o.get("optionId"))
                    .and_then(Value::as_str)
                    .unwrap_or("allow_once")
                    .to_string();
                if let Err(e) = agent
                    .respond(id, json!({ "outcome": { "outcome": "selected", "optionId": option_id } }))
                    .await
                {
                    let _ = tx.send(text_msg(json!({ "type": "error", "message": format!("permission reply failed: {e}") })));
                } else {
                    let _ = tx.send(text_msg(json!({ "type": "append", "role": "sys", "text": "[permission auto-allowed]\n" })));
                }
            }
        }
        _ => {
            // Unhandled method: Kiro extensions like `_kiro.dev/commands/available`
            // land here. Add arms as needed.
        }
    }
}

// ---------- helpers ----------

/// Serialise a JSON value into a WS text frame. The terminology split keeps
/// `handle_agent_message` free of `Message::Text(...)` noise.
fn text_msg(value: Value) -> Message {
    Message::Text(value.to_string())
}

/// Send a `sys`-role line to the browser. Used for the short status messages
/// racp itself emits (startup, errors, permission auto-allow).
fn send_sys(tx: &mpsc::UnboundedSender<Message>, text: &str) -> Result<()> {
    tx.send(text_msg(json!({ "type": "append", "role": "sys", "text": text })))
        .map_err(|_| anyhow!("ws channel closed"))?;
    Ok(())
}

// ---------- telegram transport (stub) ----------
//
// TODO(telegram): implement. Planned shape:
//   - Long-poll https://api.telegram.org/bot<token>/getUpdates.
//   - One ACP agent per Telegram chat, spawned on first message, torn down
//     on idle timeout.
//   - Stream `agent_message_chunk` output as a single `sendMessage` followed
//     by `editMessageText` calls throttled to ~1/s (Telegram per-chat limit).
//   - Inline keyboard for `session/request_permission` replies.
//   - Per-user bot tokens (created via @BotFather) keep racp out of the
//     shared infrastructure path; long polling requires exactly one process
//     per token.

async fn run_telegram(_cfg: Config) -> Result<()> {
    bail!("telegram transport is not yet implemented; re-run `racp init` and pick cloudflared");
}

// ---------- ACP agent subprocess ----------
//
// One `Agent` wraps one spawned child process and its JSON-RPC framing.
// The stdout reader task (see `spawn_agent`) splits incoming traffic into
// two streams:
//   - **Responses** (messages with `result` or `error` and a known `id`)
//     are delivered to the matching oneshot sender registered by `request`.
//   - **Notifications and server-initiated requests** go out through the
//     `updates_rx` mpsc channel, which the WS handler drains.

/// Handle on the ACP agent subprocess.
///
/// Thread-safety: all mutable state is behind `Mutex`/`Arc`, so the handle
/// can be cloned into spawned tasks (as `Arc<Agent>` in `handle_ws`).
struct Agent {
    /// Stdin to the child; serialised by a Mutex because prompt tasks may
    /// try to write concurrently.
    stdin: Mutex<ChildStdin>,
    /// Monotonic JSON-RPC id generator.
    next_id: AtomicI64,
    /// Map from in-flight request id to the oneshot waiting for its
    /// response. Shared with the reader task that populates responses.
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// Updates receiver, handed out exactly once via `take_updates`.
    updates_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
    /// Owned child so drop kills it (see `kill_on_drop(true)`).
    _child: Mutex<Child>
}

impl Agent {
    /// Extract the updates receiver. Must only be called once per Agent.
    fn take_updates(&self) -> mpsc::UnboundedReceiver<Value> {
        self.updates_rx
            .try_lock()
            .expect("updates_rx already locked")
            .take()
            .expect("updates_rx already taken")
    }

    /// Send a JSON-RPC request and await its response.
    ///
    /// Returns the `result` value on success, or an error if the agent
    /// responded with `error`, closed before replying, or the stdin write
    /// failed. The caller is responsible for cancellation semantics — if
    /// the future is dropped mid-flight, the response will arrive at a
    /// dangling oneshot and be discarded.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        let resp = rx.await.context("agent closed before replying")?;
        if let Some(err) = resp.get("error") {
            bail!("agent error: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Reply to a server-initiated request (e.g. `session/request_permission`).
    async fn respond(&self, id: Value, result: Value) -> Result<()> {
        let line = format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

/// Spawn the configured agent and wire its stdio into the `Agent` handle.
///
/// The child is configured with `kill_on_drop(true)` so that when the
/// returned `Agent` (or an `Arc` containing it) is dropped, the process
/// goes with it — important because each browser session owns its own
/// agent and we do not want orphan children hanging around.
///
/// Two background tasks are spawned here:
///   1. Stderr forwarder — writes each line to our stderr prefixed with
///      `[agent]`, for debugging.
///   2. Stdout reader — newline-delimited JSON decoder that routes
///      responses to their pending oneshots and everything else to the
///      `updates_tx` mpsc.
async fn spawn_agent(cfg: &Config) -> Result<Agent> {
    let mut cmd = Command::new(&cfg.agent_cmd);
    cmd.args(&cfg.agent_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{}`", cfg.agent_cmd))?;

    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    // Stderr forwarder.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[agent] {line}");
        }
    });

    let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();

    // Stdout reader: route responses vs notifications.
    //
    // A response is any message carrying `result` or `error` whose `id`
    // matches a pending request we sent. Everything else — notifications
    // (no id) and server-initiated requests (id but no result/error) — is
    // pushed onto the updates channel for the WS handler to act on.
    let pending_reader = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue // malformed line; skip silently
            };
            let is_response = msg.get("result").is_some() || msg.get("error").is_some();
            if is_response {
                if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                    if let Some(tx) = pending_reader.lock().await.remove(&id) {
                        let _ = tx.send(msg);
                        continue;
                    }
                }
            }
            let _ = updates_tx.send(msg);
        }
    });

    Ok(Agent {
        stdin: Mutex::new(stdin),
        next_id: AtomicI64::new(1),
        pending,
        updates_rx: Mutex::new(Some(updates_rx)),
        _child: Mutex::new(child)
    })
}
