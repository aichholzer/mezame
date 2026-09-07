//! The WebSocket half of the transport: the upgrade, the per-attach
//! loop, and the client command set.
//!
//! Ownership and concurrency:
//!
//! - `ws_upgrade` decides the session id before the handshake, so a value
//!   Mezame would never bind a hub to is refused with no socket
//!   established.
//! - `handle_ws` splits the socket into a sink owned by a writer task and
//!   a stream polled by the attach loop. Sends to the browser go through
//!   an unbounded channel, so no handler contends on the sink.
//! - `run_attach_loop` is one attach: it forwards this browser's commands
//!   to its hub, forwards the hub's broadcast to this socket, and evicts a
//!   peer that has gone silent. Everything about the turn itself belongs
//!   to the hub and its Backend.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration, Instant, MissedTickBehavior};

/// How often the server sends a WebSocket `Ping` to each attached
/// browser. A live peer answers with a `Pong` (or sends any other
/// frame), which resets the liveness clock.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);

/// How long a socket may go with no inbound frame at all before we
/// treat it as dead and break the attach loop. Must be a comfortable
/// multiple of `HEARTBEAT_INTERVAL` so a single dropped pong or a brief
/// network stall does not evict a live browser. At 60s a dead peer is
/// reaped after ~2-3 missed pings; combined with the hub's 30s grace
/// window the session is released ~90s after the peer vanishes.
///
/// This is the fix for half-open sockets (laptop sleep, Wi-Fi drop, a
/// reverse proxy silently dropping an idle upstream): such a socket
/// stays `ESTABLISHED` forever and `stream.next()` yields nothing, so
/// without a heartbeat the attach loop blocks indefinitely, the
/// `AttachedHub` never drops, the grace timer never arms, and the session
/// is never reclaimed. See GitHub issue #4.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);

/// Ceiling on one inbound WebSocket message, 32 MiB.
///
/// The composer sends a prompt as one text frame holding its blocks, with
/// attachments inline as base64. The browser allows 20 MB of attachments
/// per prompt (`MAX_TOTAL_BYTES` in `ui/src/lib/attachments.ts`), which
/// base64 renders as a little under 27 MiB, and the headroom above that
/// holds the text and the JSON around it. A frame announcing more than
/// this ends the connection with a protocol error before its payload is
/// read; the library default of 64 MiB no longer applies. The browser
/// reconnects to the same session, which is untouched.
pub const MAX_WS_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Mint a session id: 16 bytes of OS entropy rendered as 32 lowercase
/// hexadecimal characters.
///
/// The output matches the form [`is_session_id`] accepts. 128 bits puts
/// the collision probability across the 20,000 ids two process runs mint
/// somewhere around 1e-31, which is what makes the uniqueness guarantee
/// hold across a restart: a browser holds its session id over one, and a
/// per-process counter could mint an id another browser already has.
///
/// The panic on entropy failure is deliberate. On Unix it means
/// `getrandom(2)` failed, and continuing with a predictable id would be
/// worse than stopping.
pub fn new_session_id() -> String {
    use std::fmt::Write as _;

    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS entropy source");
    bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        // Writing into a String cannot fail.
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The accepted session id form: exactly 32 lowercase hexadecimal
/// characters, which is what [`new_session_id`] mints and nothing else.
///
/// A session id is the one thing gating an attach and a history read, so
/// only an id this process or an earlier run minted is bound to a hub. A
/// client-chosen name would make a session anyone could find by guessing
/// it. The form is ASCII, so the byte length equals the character count,
/// and no separator, dot segment or whitespace can reach a lookup key.
pub fn is_session_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What to do with the `session` query parameter of an upgrade request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDecision {
    /// No id was supplied. Mint one.
    Mint,
    /// The supplied id is accepted, trimmed of surrounding whitespace.
    Accept(String),
    /// The supplied id is not a form Mezame binds a hub to. Refuse the
    /// upgrade.
    Refuse,
}

/// Decide what a `session` query parameter means.
///
/// A pure function, so the upgrade handler and its property test share one
/// implementation rather than agreeing by inspection.
pub fn decide_session(param: Option<&str>) -> SessionDecision {
    match param.map(str::trim) {
        None | Some("") => SessionDecision::Mint,
        Some(id) if is_session_id(id) => SessionDecision::Accept(id.to_string()),
        Some(_) => SessionDecision::Refuse,
    }
}

/// The `/ws` handler. Decides the session id before the handshake, so a
/// value Mezame would never bind a hub to is refused with no WebSocket
/// established and no hub created.
///
/// The `cwd` query parameter is not read at all. A session opens against
/// Mezame's own working directory, whatever a client sends.
pub(crate) async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<crate::http::AppState>>,
) -> Response {
    let session_id = match decide_session(params.get("session").map(String::as_str)) {
        SessionDecision::Mint => new_session_id(),
        SessionDecision::Accept(id) => id,
        SessionDecision::Refuse => {
            return (StatusCode::BAD_REQUEST, "invalid session id").into_response();
        }
    };
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            if let Err(e) = handle_ws(socket, state, session_id).await {
                eprintln!("WebSocket session ended: {e:?}");
            }
        })
}

/// Serialise a JSON value into a WS text frame.
fn text_msg(value: Value) -> Message {
    Message::Text(value.to_string())
}

async fn handle_ws(
    ws: WebSocket,
    state: Arc<crate::http::AppState>,
    session_id: String,
) -> Result<()> {
    let (mut sink, mut stream) = ws.split();
    let (to_ws_tx, mut to_ws_rx) = mpsc::unbounded_channel::<Message>();

    // Writer task: drain the outbound channel into the WS sink. Exits when
    // the channel closes or the sink errors.
    let writer = tokio::spawn(async move {
        while let Some(msg) = to_ws_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Attach to the hub for this session id, building one if none is
    // registered. The only failure is the working-directory lookup the
    // `ready` template needs; log it, tell the browser, and close.
    let mut attached = match state.hubs.attach_or_create(&session_id).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Session {session_id}: could not attach: {e:?}");
            let _ = to_ws_tx.send(text_msg(
                json!({ "type": "error", "message": format!("{e}") }),
            ));
            drop(to_ws_tx);
            let _ = writer.await;
            return Ok(());
        }
    };

    // `ready` first, then the `session_info` snapshot when there is one.
    // `buildId` is one value per process, so the handler stamps it rather
    // than the hub.
    let mut ready = attached.snapshot_ready.clone();
    if let Some(map) = ready.as_object_mut() {
        map.insert(
            "buildId".into(),
            Value::String(env!("MEZAME_BUILD_ID").to_string()),
        );
    }
    let _ = to_ws_tx.send(text_msg(ready));
    if let Some(info) = attached.snapshot_session_info.clone() {
        let _ = to_ws_tx.send(text_msg(info));
    }

    // The receiver `subscribe` took, not a fresh one: a `prompt_done`
    // that landed between the subscribe and here has to reach this
    // attach, or a composer this attach locked on `busy` never unlocks.
    let outbound = attached.take_outbound();
    let commands = attached.commands.clone();
    let attach_id = attached.attach_id;

    run_attach_loop(
        &mut stream,
        &to_ws_tx,
        outbound,
        commands,
        attach_id,
        HEARTBEAT_INTERVAL,
        HEARTBEAT_TIMEOUT,
    )
    .await;

    // Drop `attached` first so the counter decrements before the writer
    // closes. The grace timer arms here if this was the last subscriber.
    drop(attached);
    drop(to_ws_tx);
    let _ = writer.await;
    Ok(())
}

/// The per-WebSocket attach loop, extracted from `handle_ws` so it can be
/// driven by integration tests with a fake stream, in particular a silent
/// one, to prove the half-open eviction path runs. Generic over the
/// stream so a test supplies an mpsc-backed or a never-yielding stream
/// with no real socket.
///
/// Three branches. The frame branch parses this browser's commands and
/// forwards them to the hub. The broadcast branch writes the hub's events
/// to this socket, dropping any frame stamped for another attach. The
/// heartbeat branch pings the peer and returns when it has been silent
/// past `heartbeat_timeout`, which is the only thing that ends the loop
/// for a half-open socket. The caller drops its attach when this
/// returns.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn run_attach_loop<S, E>(
    stream: &mut S,
    to_ws_tx: &mpsc::UnboundedSender<Message>,
    mut outbound: tokio::sync::broadcast::Receiver<Arc<Value>>,
    commands: mpsc::Sender<crate::hub::HubCommand>,
    attach_id: u64,
    heartbeat_interval: Duration,
    heartbeat_timeout: Duration,
) where
    S: Stream<Item = std::result::Result<Message, E>> + Unpin,
{
    // Heartbeat: ping the browser on an interval and evict the socket
    // if it goes silent past `heartbeat_timeout`. `last_seen` is bumped
    // by ANY inbound frame (text, pong, ping, binary). A chatty live
    // browser is never evicted, and an idle-but-alive one is kept up by
    // its pong replies. A half-open socket sends nothing and trips the
    // timeout. The loop then returns, the caller drops its attach, the
    // subscriber count falls, and the session is eventually reclaimed.
    // See issue #4.
    let mut heartbeat = interval(heartbeat_interval);
    // A missed tick (the task was busy) fires once and realigns. No
    // burst of catch-up ticks.
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick completes immediately. Skipping it holds the first
    // ping back until the connection has settled.
    heartbeat.tick().await;
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            ws_msg = stream.next() => {
                // Any inbound frame proves the peer is alive.
                last_seen = Instant::now();
                let text = match ws_msg {
                    None => break,
                    Some(Err(_)) => break,
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(_)) => continue,
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("Discarded a text frame that is not JSON");
                        continue;
                    }
                };
                if let Some(cmd) = parse_browser_command(&v, attach_id) {
                    if commands.send(cmd).await.is_err() {
                        // Hub owner gone; nothing more to do.
                        break;
                    }
                }
            }
            _ = heartbeat.tick() => {
                // Evict a peer that has been silent too long: a
                // half-open socket never yields on `stream.next()`,
                // so this arm is the only thing that ends the loop
                // for it.
                if last_seen.elapsed() >= heartbeat_timeout {
                    break;
                }
                // Otherwise prod it. A live peer answers with a Pong
                // (handled by the stream arm above, which bumps
                // `last_seen`). The send goes through the writer task;
                // if that channel is gone the connection is already
                // tearing down.
                if to_ws_tx.send(Message::Ping(Vec::new())).is_err() {
                    break;
                }
            }
            evt = outbound.recv() => {
                match evt {
                    Ok(value) => {
                        // Drop targeted broadcasts that are not for
                        // this attach. The hub stamps `_target` on a
                        // permission request with the attach id of the
                        // browser that started the turn; every other
                        // attach receives the broadcast and skips it
                        // here, so nobody renders a card they were not
                        // asked to answer. Untargeted events fall
                        // through to the sink.
                        if let Some(target) = value.get("_target").and_then(Value::as_u64) {
                            if target != attach_id {
                                continue;
                            }
                        }
                        let _ = to_ws_tx.send(text_msg((*value).clone()));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Slow subscriber; let the next recv pick up
                        // the current head. Nothing to send to the
                        // browser: the sender already dropped these
                        // events from the queue, and surfacing a
                        // "you missed N frames" notice would just
                        // confuse the user.
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// Translate a parsed browser frame into a `HubCommand`.
///
/// `None` means the frame is discarded: no event is emitted, no Backend
/// method is invoked, and the attach stays open. Four faults share that
/// one outcome, and they are indistinguishable to a client on purpose. An
/// `error` frame would raise a notice for something the user never
/// composed, and closing the attach would evict a browser over one bad
/// frame during a version skew. One line goes to stderr so whoever reads
/// the service log can tell a malformed command from an unknown one.
///
/// Unknown fields on a recognised command are ignored. Block members
/// reach the Backend unchecked: the block vocabulary is a statement about
/// what a client sends, not a server check.
fn parse_browser_command(v: &Value, attach_id: u64) -> Option<crate::hub::HubCommand> {
    let Some(command_type) = v.get("type").and_then(Value::as_str) else {
        eprintln!("Discarded a frame whose `type` is absent or is not a string");
        return None;
    };
    let parsed = match command_type {
        "prompt" => {
            v.get("blocks")
                .and_then(Value::as_array)
                .map(|blocks| crate::hub::HubCommand::Prompt {
                    blocks: blocks.clone(),
                    attach_id,
                })
        }
        "permission_response" => {
            let id = v
                .get("id")
                .filter(|id| id.is_string() || id.is_number())
                .cloned();
            let option_id = v.get("optionId").and_then(Value::as_str);
            match (id, option_id) {
                (Some(id), Some(option_id)) => Some(crate::hub::HubCommand::PermissionResponse {
                    id,
                    option_id: option_id.to_string(),
                }),
                _ => None,
            }
        }
        "cancel" => Some(crate::hub::HubCommand::Cancel),
        "set_model" => v.get("modelId").and_then(Value::as_str).map(|model_id| {
            crate::hub::HubCommand::SetModel {
                model_id: model_id.to_string(),
            }
        }),
        _ => None,
    };
    if parsed.is_none() {
        // Debug-formatted: the value is the peer's, and a raw write would
        // let it put line breaks and control characters into the log.
        eprintln!("Discarded a {command_type:?} frame");
    }
    parsed
}
