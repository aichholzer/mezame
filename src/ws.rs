//! Per-WebSocket session loop.
//!
//! Ownership and concurrency:
//!
//! - The WS is split into a `sink` (owned by a writer task) and a `stream`
//!   (polled directly in the select loop).
//! - Sends to the browser go through an unbounded mpsc so handlers never
//!   contend on the sink directly.
//! - The agent subprocess is spawned once per session and wrapped in `Arc`
//!   so both the select loop and spawned prompt tasks can call into it.
//! - Prompts are run in their own spawned tasks so a long-running
//!   `session/prompt` does not block the select loop from draining
//!   `session/update` notifications.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::Response,
};
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::agent::{spawn_agent, Agent};
use crate::config::Config;
use crate::session::{extract_session_info, short_reason, try_load_session};

const PROTOCOL_VERSION: u32 = 1;

/// Open a fresh ACP session and pull out the bits we forward to the
/// browser. Returns `(sessionId, modes/models payload)`. Used both as
/// the primary path when the browser does not request a resume, and as
/// the fallback when `session/load` fails.
async fn start_new_session(agent: &Agent, cwd: &str) -> Result<(String, Option<Value>)> {
    let result = agent
        .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
        .await
        .context("Failed to start new session")?;
    let sid = result
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Session creation returned no session id"))?
        .to_string();
    Ok((sid, extract_session_info(&result)))
}

/// Spawn a task that runs `fut` to completion; on error, push a typed
/// `error` event to the browser prefixed with `error_prefix`.
///
/// Used for fire-and-forget agent calls triggered by browser messages
/// (`set_mode`, `set_model`, `permission_response`, etc.). The select
/// loop must keep pumping while the call is in flight, so we do not
/// `.await` the future inline. Errors are not propagated back through
/// the loop, only surfaced to the browser as a UI notice.
fn spawn_with_error_report(
    to_ws: mpsc::UnboundedSender<Message>,
    error_prefix: &'static str,
    fut: impl Future<Output = Result<()>> + Send + 'static,
) {
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            let _ = to_ws.send(text_msg(json!({
                "type": "error",
                "message": format!("{error_prefix}: {e}")
            })));
        }
    });
}

pub(crate) async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(cfg): State<Arc<Config>>,
) -> Response {
    // `/ws?session=<acp-session-id>` asks Mezame to call `session/load` on the
    // agent instead of `session/new`. Absent = always new session.
    // `/ws?cwd=<path>` overrides the working directory for this session;
    // absent or empty = Mezame's own process cwd.
    let resume = params.get("session").cloned();
    let cwd_override = params
        .get("cwd")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_ws(socket, cfg, resume, cwd_override).await {
            eprintln!("WebSocket session ended: {e:?}");
        }
    })
}

/// Serialise a JSON value into a WS text frame. The terminology split keeps
/// `handle_agent_message` free of `Message::Text(...)` noise.
fn text_msg(value: Value) -> Message {
    Message::Text(value.to_string())
}

async fn handle_ws(
    ws: WebSocket,
    cfg: Arc<Config>,
    resume_session_id: Option<String>,
    cwd_override: Option<String>,
) -> Result<()> {
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

    // The browser's sticky status banner shows "connecting..." until the
    // `ready` message arrives, so we no longer echo a startup line into
    // the log. This keeps the log free of protocol chatter.

    // If spawn fails (bad agent_cmd, missing binary, ...) tell the browser
    // and close cleanly. Do NOT return an Err here: the writer task still
    // needs to drain the error message before we exit.
    let (agent, mut updates_rx) = match spawn_agent(&cfg).await {
        Ok((a, rx)) => (Arc::new(a), rx),
        Err(e) => {
            let _ = to_ws_tx.send(text_msg(
                json!({ "type": "error", "message": format!("{e}") }),
            ));
            drop(to_ws_tx);
            let _ = writer.await;
            return Ok(());
        }
    };

    // ACP handshake. `initialize` advertises no filesystem capabilities
    // because Mezame does not back `fs/read_text_file` etc. today; the agent
    // is expected to use its own tools for file I/O.
    //
    // The agent responds with its own `agentCapabilities`, including
    // `promptCapabilities` (image, audio, embeddedContext). We capture
    // the prompt capabilities to forward to the UI so it can decide
    // whether to surface image paste/drop and file upload.
    let initialize_result = agent
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false }
                }
            }),
        )
        .await
        .context("Failed to initialize agent")?;
    let prompt_capabilities = initialize_result
        .get("agentCapabilities")
        .and_then(|c| c.get("promptCapabilities"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Session setup. If the browser supplied a resume id, try `session/load`
    // first; on failure fall back to `session/new`. ACP's session/load
    // replays past messages via session/update notifications on the same
    // stream, which the select loop will forward to the browser as the
    // history rehydrates.
    //
    // `cwd` comes from the browser's `?cwd=<path>` query param if provided;
    // otherwise we use Mezame's own process cwd.
    let cwd_str = match cwd_override {
        Some(c) => c,
        None => std::env::current_dir()?.to_string_lossy().to_string(),
    };

    let (session_id, resumed, session_info) = match resume_session_id {
        Some(sid) => match try_load_session(&agent, &sid, &cwd_str).await {
            Ok(value) => (sid, true, extract_session_info(&value)),
            Err(err_str) => {
                eprintln!("Session load failed: {err_str}. Falling back to a new session.");
                let _ = to_ws_tx.send(text_msg(json!({
                    "type": "append",
                    "role": "sys",
                    "text": format!(
                        "\n[{} — Starting a new one.]\n",
                        short_reason(&err_str)
                    )
                })));
                let (sid, info) = start_new_session(&agent, &cwd_str).await?;
                (sid, false, info)
            }
        },
        None => {
            let (sid, info) = start_new_session(&agent, &cwd_str).await?;
            (sid, false, info)
        }
    };

    // Tell the browser which session id it is bound to so it can persist it
    // for reconnect, and whether this was a resume (so it can clear stale
    // log before the replay lands). The `cwd` is the actual path the agent
    // session was opened with, so the UI can display it even when no
    // `?cwd=` override was supplied. `buildId` is a unique-per-build token
    // so the UI can detect a stale bundle and force a reload.
    let _ = to_ws_tx.send(text_msg(json!({
        "type": "ready",
        "sessionId": session_id,
        "resumed": resumed,
        "cwd": cwd_str,
        "promptCapabilities": prompt_capabilities,
        "buildId": env!("MEZAME_BUILD_ID")
    })));

    // Send the `modes` and `models` payload (if present in either
    // session/new or session/load result) so the UI can render its
    // mode/model selectors and the current selections.
    if let Some(info) = session_info {
        let _ = to_ws_tx.send(text_msg(json!({
            "type": "session_info",
            "info": info
        })));
    }

    // After a successful `session/load`, Kiro replays past messages via
    // `session/update` notifications. The browser instead fetches the
    // history via `/history` (with real per-turn timestamps from the
    // on-disk `.jsonl`), so forwarding Kiro's replay would produce
    // duplicates. Drop `session/update` events until the first user-sent
    // prompt after resume; permission/tool requests still flow through.
    let mut suppress_session_updates = resumed;

    run_select_loop(
        &mut stream,
        &to_ws_tx,
        agent.clone(),
        &mut updates_rx,
        &session_id,
        &mut suppress_session_updates,
    )
    .await;

    // Cooperative shutdown of the agent subprocess. Sends `session/cancel`,
    // closes stdin, and waits briefly for exit so Kiro can release its
    // session lock. `kill_on_drop` stays on as a safety net in case the
    // agent doesn't honour EOF within the timeout.
    agent.shutdown(Some(&session_id)).await;

    // Closing the outbound channel unblocks the writer task. The agent
    // child is killed on drop of the `Agent` (see `kill_on_drop(true)` in
    // `spawn_agent`) if shutdown timed out above.
    drop(to_ws_tx);
    let _ = writer.await;
    Ok(())
}

/// Drive the per-session select loop until either the WS stream or the
/// agent updates channel ends. Extracted from `handle_ws` so integration
/// tests can build a fake stream and a fake agent and exercise the same
/// logic without spinning up axum or spawning a real subprocess.
///
/// The loop never returns an error; transport-level failures cause it
/// to break out so the caller can run cooperative shutdown.
///
/// The pattern matching here is deliberately exhaustive on the
/// `Option<Result<Message, _>>` returned by `stream.next()`. An earlier
/// version used a `Some(Ok(...))` guard, which silently disabled the
/// branch on stream close or transport error and prevented shutdown
/// from running. See the `Fixed` entry for 0.8.7 in CHANGELOG.md.
pub async fn run_select_loop<S, E>(
    stream: &mut S,
    to_ws_tx: &mpsc::UnboundedSender<Message>,
    agent: Arc<Agent>,
    updates_rx: &mut mpsc::UnboundedReceiver<Value>,
    session_id: &str,
    suppress_session_updates: &mut bool,
) where
    S: Stream<Item = std::result::Result<Message, E>> + Unpin,
{
    loop {
        tokio::select! {
            // User → agent: messages from the browser. Match the full
            // Option<Result<Message, _>> here rather than relying on a
            // `Some(Ok(...))` pattern guard. With a guard, a closed
            // stream (`None`) or a transport error (`Some(Err(_))`)
            // disables this select branch silently while the other
            // branch keeps delivering agent updates. The `else => break`
            // arm only fires when ALL branches are disabled, so the
            // loop would never exit and `agent.shutdown()` would never
            // run. Result: leaked agent subprocess + stale Kiro session
            // lockfile on every browser disconnect during a long turn.
            ws_msg = stream.next() => {
                let text = match ws_msg {
                    None => break,                              // peer closed the socket
                    Some(Err(_)) => break,                      // transport error
                    Some(Ok(Message::Close(_))) => break,       // clean close frame
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(_)) => continue,                    // ping/pong/binary
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue
                };

                match v.get("type").and_then(Value::as_str) {
                    Some("prompt") => {
                        // Browser sends a prompt as either a plain `text`
                        // string (legacy path) or a full ACP-shaped
                        // `blocks` array. The blocks path is how we carry
                        // attachments (image, audio, resource) alongside
                        // the user's text. The server does no validation
                        // beyond "must be an array"; the agent will reject
                        // block types it did not advertise support for.
                        let prompt_blocks: Vec<Value> = if let Some(blocks) = v.get("blocks").and_then(Value::as_array) {
                            blocks.clone()
                        } else if let Some(user_text) = v.get("text").and_then(Value::as_str) {
                            vec![json!({ "type": "text", "text": user_text })]
                        } else {
                            continue;
                        };
                        if prompt_blocks.is_empty() {
                            continue;
                        }

                        // First live prompt after resume: stop hiding
                        // `session/update` events. From here on everything
                        // the agent emits is genuinely new.
                        *suppress_session_updates = false;

                        // Run `session/prompt` in its own task so the select
                        // loop keeps pumping `session/update` notifications
                        // while the agent is working. When the request
                        // resolves we tell the browser the turn is over (or
                        // surface the error).
                        let agent = agent.clone();
                        let to_ws = to_ws_tx.clone();
                        let sid = session_id.to_string();
                        tokio::spawn(async move {
                            let res = agent
                                .request(
                                    "session/prompt",
                                    json!({
                                        "sessionId": sid,
                                        "prompt": prompt_blocks
                                    })
                                )
                                .await;
                            if let Err(e) = res {
                                let _ = to_ws.send(text_msg(json!({ "type": "error", "message": format!("{e}") })));
                            }
                            let _ = to_ws.send(text_msg(json!({ "type": "prompt_done" })));
                        });
                    }
                    Some("permission_response") => {
                        // Browser replied to a `session/request_permission`
                        // we forwarded earlier. The `id` must match the one
                        // we forwarded; we pass it straight back to the
                        // agent so it can unblock.
                        let Some(id) = v.get("id").cloned() else {
                            continue;
                        };
                        let option_id = v
                            .get("optionId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let agent = agent.clone();
                        spawn_with_error_report(
                            to_ws_tx.clone(),
                            "Permission reply failed",
                            async move {
                                agent
                                    .respond(
                                        id,
                                        json!({
                                            "outcome": {
                                                "outcome": "selected",
                                                "optionId": option_id
                                            }
                                        }),
                                    )
                                    .await
                            },
                        );
                    }
                    Some("cancel") => {
                        // ACP `session/cancel` is a notification (no id, no
                        // response expected). The agent is responsible for
                        // stopping whatever tool or turn is in flight and
                        // eventually resolving the outstanding
                        // `session/prompt` request, which is what unblocks
                        // the browser's "busy" state.
                        let agent = agent.clone();
                        let sid = session_id.to_string();
                        tokio::spawn(async move {
                            let _ = agent
                                .notify(
                                    "session/cancel",
                                    json!({ "sessionId": sid })
                                )
                                .await;
                        });
                    }
                    Some("set_mode") => {
                        // Kiro calls them "modes" but the available ids are
                        // agent configs (`kiro_default`, `kiro_planner`,
                        // `kiro_guide`). Forward as `session/set_mode`.
                        let Some(mode_id) = v.get("modeId").and_then(Value::as_str) else {
                            continue;
                        };
                        let mode_id = mode_id.to_string();
                        let agent = agent.clone();
                        let sid = session_id.to_string();
                        spawn_with_error_report(
                            to_ws_tx.clone(),
                            "Failed to change agent mode",
                            async move {
                                agent
                                    .request(
                                        "session/set_mode",
                                        json!({ "sessionId": sid, "modeId": mode_id }),
                                    )
                                    .await?;
                                Ok(())
                            },
                        );
                    }
                    Some("set_model") => {
                        let Some(model_id) = v.get("modelId").and_then(Value::as_str) else {
                            continue;
                        };
                        let model_id = model_id.to_string();
                        let agent = agent.clone();
                        let sid = session_id.to_string();
                        spawn_with_error_report(
                            to_ws_tx.clone(),
                            "Failed to change model",
                            async move {
                                agent
                                    .request(
                                        "session/set_model",
                                        json!({ "sessionId": sid, "modelId": model_id }),
                                    )
                                    .await?;
                                Ok(())
                            },
                        );
                    }
                    _ => continue
                }
            }
            // Agent → user: notifications and server-initiated requests.
            // Same caveat as the stream branch: a guarded `Some(...)`
            // pattern would silently disable the branch when the agent
            // exits. Match the full `Option<Value>` so we can break out
            // of the loop and run cooperative shutdown.
            agent_msg = updates_rx.recv() => {
                match agent_msg {
                    Some(msg) => handle_agent_message(to_ws_tx, msg, *suppress_session_updates).await,
                    None => break, // agent stdout reader exited; child is gone or going
                }
            }
            else => break
        }
    }
}

/// Translate an agent-originated message into browser-facing events.
///
/// `suppress_session_updates` is set by the WS handler during a resume
/// window: the browser seeds its log from `/history` instead of the ACP
/// replay, so forwarding `session/update` events would duplicate every
/// replayed chunk. Server-initiated requests (permission prompts) are
/// still forwarded — they only occur for live tool calls, not replay.
///
/// Currently understood:
///
/// - `session/update`:
///     - `agent_message_chunk`   → append as `agent` text
///     - `agent_thought_chunk`   → append as `sys` with a `(thinking)` prefix
///     - `tool_call` / `tool_call_update` → append `[title — status]`
/// - `session/request_permission` → forwarded to the browser as a
///   `permission_request` event.
/// - `_kiro.dev/commands/available` → trimmed and forwarded as a
///   `commands` event (just the `commands` + `prompts` arrays; the big
///   `tools` catalogue is dropped to keep the WS frame small).
/// - `_kiro.dev/mcp/oauth_request` → forwarded as `mcp_oauth_request`
///   so the browser can render an inline card with an Open button.
///
/// Everything else is silently dropped, including Kiro's other
/// `_kiro.dev/*` extension notifications.
async fn handle_agent_message(
    tx: &mpsc::UnboundedSender<Message>,
    msg: Value,
    suppress_session_updates: bool,
) {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "_kiro.dev/commands/available" => {
            // Kiro re-emits this notification as its catalogue warms up
            // (MCP servers load, etc.). We treat each emission as the
            // full current catalogue; last-wins semantics on the browser.
            if let Some(params) = msg.get("params") {
                let commands = params
                    .get("commands")
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                let prompts = params
                    .get("prompts")
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                let _ = tx.send(text_msg(json!({
                    "type": "commands",
                    "commands": commands,
                    "prompts": prompts
                })));
            }
        }
        "_kiro.dev/mcp/oauth_request" => {
            // An MCP server wants the user to authorise at a URL out of
            // band. We surface the request so the browser can render a
            // card with an "Open" button. Kiro re-emits while waiting,
            // so we forward an `id` (when present) and let the browser
            // de-dup. Field shapes are best-effort: we accept either
            // `serverName` / `name`, and `url` / `authUrl`.
            if let Some(params) = msg.get("params") {
                let server_name = params
                    .get("serverName")
                    .or_else(|| params.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("MCP server")
                    .to_string();
                let url = params
                    .get("url")
                    .or_else(|| params.get("authUrl"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if url.is_empty() {
                    // Without a URL there is nothing actionable; drop
                    // silently rather than rendering a dead card.
                    return;
                }
                let id = params
                    .get("id")
                    .or_else(|| params.get("requestId"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let _ = tx.send(text_msg(json!({
                    "type": "mcp_oauth_request",
                    "id": id,
                    "serverName": server_name,
                    "url": url
                })));
            }
        }
        "session/update" => {
            if suppress_session_updates {
                return;
            }
            let update = msg
                .get("params")
                .and_then(|p| p.get("update"))
                .cloned()
                .unwrap_or(Value::Null);
            let kind = update
                .get("sessionUpdate")
                .and_then(Value::as_str)
                .unwrap_or("");
            match kind {
                "agent_message_chunk" => {
                    if let Some(text) = update
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                    {
                        let _ = tx.send(text_msg(
                            json!({ "type": "append", "role": "agent", "text": text }),
                        ));
                    }
                }
                "user_message_chunk" => {
                    // Only emitted during `session/load` replay, so this
                    // does not double-render live prompts (the browser
                    // already echoes those locally).
                    if let Some(text) = update
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                    {
                        let _ = tx.send(text_msg(json!({
                            "type": "append",
                            "role": "user",
                            "text": format!("> {text}\n")
                        })));
                    }
                }
                "agent_thought_chunk" => {
                    // Reasoning tokens. Kiro does not currently emit these,
                    // but leave the handler in place so reasoning-model
                    // agents light up the UI with `(thinking)` lines.
                    if let Some(text) = update
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                    {
                        let _ = tx.send(text_msg(json!({ "type": "append", "role": "sys", "text": format!("(thinking) {text}") })));
                    }
                }
                "tool_call" | "tool_call_update" => {
                    // Forward the full structured payload to the browser.
                    // Both `tool_call` and `tool_call_update` emit the
                    // same WS event type; the UI dedupes by `toolCallId`
                    // and mutates the existing row in place on updates.
                    //
                    // Fields are passed through as-is so the UI can
                    // render whatever the agent supplied (title, status,
                    // kind, input args, output content blocks, and file
                    // locations touched).
                    let tool_call_id = update.get("toolCallId").cloned().unwrap_or(Value::Null);
                    if tool_call_id.is_null() {
                        // Nothing to key on; fall back to a sys line so
                        // the user at least knows something happened.
                        let title = update
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let status = update.get("status").and_then(Value::as_str).unwrap_or("");
                        let line = if status.is_empty() {
                            format!("\n[{title}]\n")
                        } else {
                            format!("\n[{title}: {status}]\n")
                        };
                        let _ = tx.send(text_msg(
                            json!({ "type": "append", "role": "sys", "text": line }),
                        ));
                        return;
                    }
                    let _ = tx.send(text_msg(json!({
                        "type": "tool_call",
                        "toolCallId": tool_call_id,
                        "title": update.get("title").cloned().unwrap_or(Value::Null),
                        "status": update.get("status").cloned().unwrap_or(Value::Null),
                        "kind": update.get("kind").cloned().unwrap_or(Value::Null),
                        "rawInput": update.get("rawInput").cloned().unwrap_or(Value::Null),
                        "content": update.get("content").cloned().unwrap_or(Value::Null),
                        "locations": update.get("locations").cloned().unwrap_or(Value::Null)
                    })));
                }
                _ => {}
            }
        }
        "session/request_permission" => {
            // Forward to the browser. The reply comes back as a
            // `permission_response` browser message, handled in the WS
            // select loop (see `handle_ws`). JSON-RPC id is passed through
            // unchanged so we can respond to the agent with it.
            if let Some(params) = msg.get("params") {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let title = params
                    .get("toolCall")
                    .and_then(|tc| tc.get("title").or_else(|| tc.get("name")))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let options = params
                    .get("options")
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                let _ = tx.send(text_msg(json!({
                    "type": "permission_request",
                    "id": id,
                    "title": title,
                    "options": options
                })));
            }
        }
        _ => {
            // Unhandled method: Kiro extensions like `_kiro.dev/commands/available`
            // land here. Add arms as needed.
        }
    }
}
