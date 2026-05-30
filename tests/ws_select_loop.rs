//! Regression tests for the WebSocket select loop in
//! `mezame::ws::run_select_loop`. These cover the disconnect and
//! agent-exit paths that previously leaked the agent subprocess.
//!
//! The tests build a fake `Agent` from in-memory streams via
//! `Agent::from_io` and feed it a fake browser-message stream backed by
//! a tokio mpsc channel. No real subprocess, no real WS handshake.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message};
use mezame::agent::{from_io, Agent};
use mezame::ws::run_select_loop;
use serde_json::Value;
use tokio::io::duplex;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Wrap a `tokio::sync::mpsc::Receiver` as a `Stream` with the same
/// `Result<Message, Infallible>` shape `run_select_loop` expects. Using
/// `Infallible` as the error type keeps the tests honest: when we want
/// to simulate a transport error we use a separate stream that yields
/// `Err`, never this helper.
fn channel_stream(
    rx: mpsc::UnboundedReceiver<Message>,
) -> impl futures_util::Stream<Item = Result<Message, std::convert::Infallible>> + Unpin {
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|msg| (Ok(msg), rx))
    }))
}

async fn build_loop_pieces() -> (
    Arc<Agent>,
    mpsc::UnboundedReceiver<Value>,
    tokio::io::DuplexStream,
    tokio::io::DuplexStream,
) {
    // Two duplex pipes:
    //   - server_to_agent / agent_stdin: server writes JSON-RPC
    //     framed messages here; tests can read from `agent_stdin` to
    //     assert what the loop sent to the agent.
    //   - agent_stdout / server_reader: tests write JSON-RPC lines to
    //     `agent_stdout` to simulate agent notifications.
    let (server_to_agent, agent_stdin) = duplex(8 * 1024);
    let (agent_stdout, server_reader) = duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);
    (Arc::new(agent), updates_rx, agent_stdin, agent_stdout)
}

#[tokio::test]
async fn breaks_on_stream_close_none_and_runs_shutdown() {
    let (agent, mut updates_rx, _agent_stdin, _agent_stdout) = build_loop_pieces().await;
    let (browser_tx, browser_rx) = mpsc::unbounded_channel();
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel();

    // Drop the browser sender immediately: the stream will yield None
    // on its first poll, simulating a peer that closed the socket
    // without sending a Close frame.
    drop(browser_tx);
    let mut stream = channel_stream(browser_rx);

    let mut suppress = false;
    let agent_for_loop = agent.clone();
    let loop_done = timeout(
        Duration::from_secs(2),
        run_select_loop(
            &mut stream,
            &to_ws_tx,
            agent_for_loop,
            &mut updates_rx,
            "session-123",
            &mut suppress,
        ),
    )
    .await;
    assert!(
        loop_done.is_ok(),
        "loop did not exit within 2s; the disconnect bug is back"
    );

    agent.shutdown(Some("session-123")).await;
    assert!(
        agent.shutdown_complete(),
        "shutdown did not run after stream closed"
    );
}

#[tokio::test]
async fn breaks_on_close_frame_and_runs_shutdown() {
    let (agent, mut updates_rx, _agent_stdin, _agent_stdout) = build_loop_pieces().await;
    let (browser_tx, browser_rx) = mpsc::unbounded_channel();
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel();

    browser_tx
        .send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: "bye".into(),
        })))
        .unwrap();
    drop(browser_tx);

    let mut stream = channel_stream(browser_rx);
    let mut suppress = false;
    let agent_for_loop = agent.clone();
    let loop_done = timeout(
        Duration::from_secs(2),
        run_select_loop(
            &mut stream,
            &to_ws_tx,
            agent_for_loop,
            &mut updates_rx,
            "session-close",
            &mut suppress,
        ),
    )
    .await;
    assert!(loop_done.is_ok(), "loop did not exit on Close frame");

    agent.shutdown(Some("session-close")).await;
    assert!(agent.shutdown_complete());
}

#[tokio::test]
async fn breaks_on_transport_error_and_runs_shutdown() {
    let (agent, mut updates_rx, _agent_stdin, _agent_stdout) = build_loop_pieces().await;
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel();

    // A stream that yields a single Err and then ends. The loop must
    // break on the first Err.
    #[derive(Debug)]
    struct FakeError;
    impl std::fmt::Display for FakeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake transport error")
        }
    }
    impl std::error::Error for FakeError {}

    let mut stream = futures_util::stream::iter(vec![Result::<Message, FakeError>::Err(FakeError)]);

    let mut suppress = false;
    let agent_for_loop = agent.clone();
    let loop_done = timeout(
        Duration::from_secs(2),
        run_select_loop(
            &mut stream,
            &to_ws_tx,
            agent_for_loop,
            &mut updates_rx,
            "session-err",
            &mut suppress,
        ),
    )
    .await;
    assert!(loop_done.is_ok(), "loop did not exit on transport error");

    agent.shutdown(Some("session-err")).await;
    assert!(agent.shutdown_complete());
}

#[tokio::test]
async fn breaks_on_agent_exit_and_runs_shutdown() {
    // No agent stdout means the reader task in `from_io` sees EOF
    // immediately, drops `updates_tx`, and the receiver's recv() will
    // yield None on the next poll.
    let (server_to_agent, agent_stdin) = duplex(1024);
    let (_eof_writer, server_reader) = duplex(1024); // we drop the writer below
    drop(agent_stdin);
    drop(_eof_writer); // signals EOF to the from_io reader
    let (agent, mut updates_rx) = from_io(server_to_agent, server_reader);
    let agent = Arc::new(agent);

    let (_browser_tx, browser_rx) = mpsc::unbounded_channel();
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel();
    let mut stream = channel_stream(browser_rx);

    let mut suppress = false;
    let agent_for_loop = agent.clone();
    let loop_done = timeout(
        Duration::from_secs(2),
        run_select_loop(
            &mut stream,
            &to_ws_tx,
            agent_for_loop,
            &mut updates_rx,
            "session-agent-exit",
            &mut suppress,
        ),
    )
    .await;
    assert!(
        loop_done.is_ok(),
        "loop did not exit when the agent's stdout ended"
    );

    agent.shutdown(Some("session-agent-exit")).await;
    assert!(agent.shutdown_complete());
}

#[tokio::test]
async fn permission_response_is_forwarded_to_agent_stdin() {
    use tokio::io::AsyncReadExt;

    let (agent, mut updates_rx, mut agent_stdin, _agent_stdout) = build_loop_pieces().await;
    let (browser_tx, browser_rx) = mpsc::unbounded_channel();
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel();

    // Browser replies to permission request id=42 by selecting "allow".
    let permission_reply = serde_json::json!({
        "type": "permission_response",
        "id": 42,
        "optionId": "allow_once"
    });
    browser_tx
        .send(Message::Text(permission_reply.to_string()))
        .unwrap();
    // Then close the browser so the loop exits cleanly.
    browser_tx
        .send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: "bye".into(),
        })))
        .unwrap();
    drop(browser_tx);

    let mut stream = channel_stream(browser_rx);
    let mut suppress = false;
    let agent_for_loop = agent.clone();
    timeout(
        Duration::from_secs(2),
        run_select_loop(
            &mut stream,
            &to_ws_tx,
            agent_for_loop,
            &mut updates_rx,
            "session-perm",
            &mut suppress,
        ),
    )
    .await
    .expect("loop did not exit");

    // Read whatever the loop wrote to the agent. Use a small read
    // window; the permission reply is one line of JSON.
    let mut buf = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(1), agent_stdin.read(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed");
    let written = std::str::from_utf8(&buf[..n]).expect("utf8");
    let line = written.lines().next().expect("at least one line");
    let parsed: Value = serde_json::from_str(line).expect("agent stdin was not valid JSON");

    assert_eq!(parsed["jsonrpc"], "2.0");
    assert_eq!(parsed["id"], 42);
    assert_eq!(parsed["result"]["outcome"]["outcome"], "selected");
    assert_eq!(parsed["result"]["outcome"]["optionId"], "allow_once");

    agent.shutdown(Some("session-perm")).await;
    assert!(agent.shutdown_complete());
}

// ---------- browser-command branches ----------
//
// The tests above cover the loop's exit paths (stream close, transport
// error, agent exit) and the permission-response forward. The block
// below exercises the remaining browser-message arms in the WS-frame
// branch: `prompt` (both the `blocks` and legacy `text` shapes),
// `cancel`, `set_mode`, and `set_model`. Each arm spawns a task that
// writes a JSON-RPC frame to the agent's stdin; we drive one browser
// message followed by a Close frame, let the loop exit, then read the
// frame the spawned task wrote and assert its method and params.

/// Drive a single browser message through the loop, then a Close frame
/// so the loop exits cleanly, and return the first JSON-RPC frame the
/// loop's spawned task wrote to the agent's stdin.
///
/// `_agent_stdout` is held for the lifetime of the call: dropping it
/// would signal EOF to the `from_io` reader, which could fire the
/// `updates_rx` `None => break` arm before the browser message is
/// processed. Keeping it alive guarantees the WS-frame branch wins.
async fn first_agent_frame_for(browser_msg: Message) -> Value {
    use tokio::io::AsyncReadExt;

    let (agent, mut updates_rx, mut agent_stdin, _agent_stdout) = build_loop_pieces().await;
    let (browser_tx, browser_rx) = mpsc::unbounded_channel();
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel();

    browser_tx.send(browser_msg).unwrap();
    browser_tx
        .send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: "bye".into(),
        })))
        .unwrap();
    drop(browser_tx);

    let mut stream = channel_stream(browser_rx);
    let mut suppress = false;
    let agent_for_loop = agent.clone();
    timeout(
        Duration::from_secs(2),
        run_select_loop(
            &mut stream,
            &to_ws_tx,
            agent_for_loop,
            &mut updates_rx,
            "session-cmd",
            &mut suppress,
        ),
    )
    .await
    .expect("loop did not exit");

    let mut buf = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(1), agent_stdin.read(&mut buf))
        .await
        .expect("read timed out")
        .expect("read failed");
    let written = std::str::from_utf8(&buf[..n]).expect("utf8");
    let line = written.lines().next().expect("at least one line");
    let parsed = serde_json::from_str(line).expect("agent stdin was not valid JSON");

    agent.shutdown(Some("session-cmd")).await;
    parsed
}

#[tokio::test]
async fn prompt_with_text_is_forwarded_as_session_prompt() {
    let msg = Message::Text(
        serde_json::json!({ "type": "prompt", "text": "what's the time" }).to_string(),
    );
    let frame = first_agent_frame_for(msg).await;

    assert_eq!(frame["method"], "session/prompt");
    assert_eq!(frame["params"]["sessionId"], "session-cmd");
    // The legacy `text` shape is wrapped into a single text block.
    assert_eq!(frame["params"]["prompt"][0]["type"], "text");
    assert_eq!(frame["params"]["prompt"][0]["text"], "what's the time");
}

#[tokio::test]
async fn prompt_with_blocks_array_is_forwarded_verbatim() {
    let blocks = serde_json::json!([
        { "type": "text", "text": "look at this" },
        { "type": "image", "mimeType": "image/png", "data": "abc" }
    ]);
    let msg = Message::Text(
        serde_json::json!({ "type": "prompt", "blocks": blocks.clone() }).to_string(),
    );
    let frame = first_agent_frame_for(msg).await;

    assert_eq!(frame["method"], "session/prompt");
    // The blocks array passes through untouched, attachments included.
    assert_eq!(frame["params"]["prompt"], blocks);
}

#[tokio::test]
async fn cancel_message_sends_session_cancel_notification() {
    let msg = Message::Text(serde_json::json!({ "type": "cancel" }).to_string());
    let frame = first_agent_frame_for(msg).await;

    assert_eq!(frame["method"], "session/cancel");
    assert_eq!(frame["params"]["sessionId"], "session-cmd");
    // A notification carries no id.
    assert!(frame.get("id").is_none(), "cancel must be a notification");
}

#[tokio::test]
async fn set_mode_message_is_forwarded_with_mode_id() {
    let msg = Message::Text(
        serde_json::json!({ "type": "set_mode", "modeId": "kiro_planner" }).to_string(),
    );
    let frame = first_agent_frame_for(msg).await;

    assert_eq!(frame["method"], "session/set_mode");
    assert_eq!(frame["params"]["sessionId"], "session-cmd");
    assert_eq!(frame["params"]["modeId"], "kiro_planner");
}

#[tokio::test]
async fn set_model_message_is_forwarded_with_model_id() {
    let msg = Message::Text(
        serde_json::json!({ "type": "set_model", "modelId": "claude-3-5-sonnet" }).to_string(),
    );
    let frame = first_agent_frame_for(msg).await;

    assert_eq!(frame["method"], "session/set_model");
    assert_eq!(frame["params"]["sessionId"], "session-cmd");
    assert_eq!(frame["params"]["modelId"], "claude-3-5-sonnet");
}

#[tokio::test]
async fn agent_update_is_forwarded_to_the_browser_sink() {
    // Exercises the `Some(msg) => handle_agent_message` arm of the
    // agent-updates branch (the existing tests only cover the agent-exit
    // `None => break` path). We write a `session/update` frame to the
    // agent's stdout; the loop pulls it off the updates channel, runs it
    // through `handle_agent_message`, and emits an `append` event to the
    // browser sink.
    use tokio::io::AsyncWriteExt;

    let (agent, mut updates_rx, _agent_stdin, mut agent_stdout) = build_loop_pieces().await;
    let (browser_tx, browser_rx) = mpsc::unbounded_channel();
    let (to_ws_tx, mut to_ws_rx) = mpsc::unbounded_channel();

    // Inject an agent notification.
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "text": "hi there" }
            }
        }
    });
    agent_stdout
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .expect("write agent frame");

    let mut stream = channel_stream(browser_rx);
    let mut suppress = false;
    let agent_for_loop = agent.clone();
    let loop_handle = tokio::spawn(async move {
        run_select_loop(
            &mut stream,
            &to_ws_tx,
            agent_for_loop,
            &mut updates_rx,
            "session-fwd",
            &mut suppress,
        )
        .await;
    });

    // The forwarded event should land on the browser sink.
    let event = timeout(Duration::from_secs(2), to_ws_rx.recv())
        .await
        .expect("event within 2s")
        .expect("channel open");
    let Message::Text(text) = event else {
        panic!("expected a text frame");
    };
    let value: Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(value["type"], "append");
    assert_eq!(value["role"], "agent");
    assert_eq!(value["text"], "hi there");

    // Close the browser so the loop exits and the spawned task joins.
    drop(browser_tx);
    let _ = timeout(Duration::from_secs(2), loop_handle).await;
    agent.shutdown(Some("session-fwd")).await;
}
