//! Integration tests for `mezame::ws::negotiate_session`. Exercises the
//! ACP-handshake-and-session-setup prelude that runs at the top of
//! `handle_ws` against a synthetic agent built from `Agent::from_io`.
//!
//! What we are NOT testing here: the WS select loop, prompt forwarding,
//! permission round-trips. Those have their own integration files.

use std::time::Duration;

use axum::extract::ws::Message;
use mezame::agent::{from_io, Agent};
use mezame::ws::{negotiate_session, NegotiationOutcome};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Spawn a fake agent backed by a duplex pipe. Each item in `responses`
/// is consumed in order: a request from the production code lands, the
/// next item is matched by method name, and the response is framed and
/// written back. Items annotated `Err` reply with a JSON-RPC error.
fn spawn_fake_agent(responses: Vec<FakeReply>) -> Agent {
    let (server_to_agent, agent_stdin) = tokio::io::duplex(8 * 1024);
    let (agent_stdout, server_reader) = tokio::io::duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);
    // Tests under this file do not consume the updates channel.
    std::mem::forget(updates_rx);

    tokio::spawn(async move {
        let mut stdin = BufReader::new(agent_stdin);
        let mut stdout = agent_stdout;
        let mut iter = responses.into_iter();
        loop {
            let mut line = String::new();
            if stdin.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let req: Value = match serde_json::from_str(line.trim_end()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = req["id"].clone();
            let next = match iter.next() {
                Some(r) => r,
                None => break,
            };
            let frame = match next {
                FakeReply::Ok(result) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }),
                FakeReply::Err(msg) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32000, "message": "Internal", "data": msg },
                }),
            };
            stdout
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        }
    });

    agent
}

enum FakeReply {
    Ok(Value),
    Err(String),
}

fn drain_outbound(rx: &mut mpsc::UnboundedReceiver<Message>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let Message::Text(t) = msg {
            if let Ok(v) = serde_json::from_str(&t) {
                out.push(v);
            }
        }
    }
    out
}

const BUILD_ID: &str = "test-build-id";

#[tokio::test]
async fn fresh_session_emits_ready_and_session_info() {
    let agent = spawn_fake_agent(vec![
        FakeReply::Ok(json!({
            "agentCapabilities": {
                "promptCapabilities": { "image": true, "embeddedContext": false }
            }
        })),
        FakeReply::Ok(json!({
            "sessionId": "new-sid",
            "modes": { "currentModeId": "default", "availableModes": [] },
            "models": null
        })),
    ]);

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let outcome = timeout(
        Duration::from_secs(2),
        negotiate_session(&agent, &tx, None, Some("/tmp".into()), BUILD_ID),
    )
    .await
    .expect("negotiate within 2s")
    .expect("negotiate succeeds");

    assert_eq!(outcome.session_id, "new-sid");
    assert!(
        !outcome.suppress_session_updates,
        "fresh session should not suppress updates"
    );

    let frames = drain_outbound(&mut rx);
    let ready = frames.iter().find(|f| f["type"] == "ready").expect("ready");
    assert_eq!(ready["sessionId"], "new-sid");
    assert_eq!(ready["resumed"], false);
    assert_eq!(ready["cwd"], "/tmp");
    assert_eq!(ready["buildId"], BUILD_ID);
    assert_eq!(ready["promptCapabilities"]["image"], true);

    let info = frames
        .iter()
        .find(|f| f["type"] == "session_info")
        .expect("session_info");
    assert_eq!(
        info["info"]["modes"]["currentModeId"], "default",
        "modes payload should pass through"
    );
}

#[tokio::test]
async fn resume_path_emits_ready_with_resumed_true_and_suppresses_updates() {
    let agent = spawn_fake_agent(vec![
        FakeReply::Ok(json!({
            "agentCapabilities": { "promptCapabilities": {} }
        })),
        // Omit modes/models entirely so extract_session_info returns
        // None and no session_info frame should be emitted.
        FakeReply::Ok(json!({ "sessionId": "resumed-sid" })),
    ]);

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let outcome = timeout(
        Duration::from_secs(2),
        negotiate_session(
            &agent,
            &tx,
            Some("resumed-sid".into()),
            Some("/tmp".into()),
            BUILD_ID,
        ),
    )
    .await
    .expect("negotiate within 2s")
    .expect("negotiate succeeds");

    assert_eq!(outcome.session_id, "resumed-sid");
    assert!(
        outcome.suppress_session_updates,
        "resume should suppress updates"
    );

    let frames = drain_outbound(&mut rx);
    let ready = frames.iter().find(|f| f["type"] == "ready").expect("ready");
    assert_eq!(ready["resumed"], true);
    // No session_info because both modes and models were null.
    assert!(
        frames.iter().all(|f| f["type"] != "session_info"),
        "no session_info expected when modes/models are null"
    );
}

#[tokio::test]
async fn resume_failure_falls_back_to_new_session_and_emits_sys_notice() {
    let agent = spawn_fake_agent(vec![
        FakeReply::Ok(json!({
            "agentCapabilities": { "promptCapabilities": {} }
        })),
        // Six attempts hit the stale-lock retry budget; we exhaust
        // them with a non-recoverable error to fall through fast.
        FakeReply::Err("session not found on disk".into()),
        FakeReply::Ok(json!({ "sessionId": "fallback-sid" })),
    ]);

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let outcome = timeout(
        Duration::from_secs(2),
        negotiate_session(
            &agent,
            &tx,
            Some("missing-sid".into()),
            Some("/tmp".into()),
            BUILD_ID,
        ),
    )
    .await
    .expect("negotiate within 2s")
    .expect("negotiate succeeds via fallback");

    assert_eq!(outcome.session_id, "fallback-sid");
    assert!(
        !outcome.suppress_session_updates,
        "fallback to new session should not suppress updates"
    );

    let frames = drain_outbound(&mut rx);
    // Sys-line warning that the resume failed should land before ready.
    let warn = frames
        .iter()
        .find(|f| f["type"] == "append" && f["role"] == "sys")
        .expect("sys append for failed resume");
    let text = warn["text"].as_str().expect("sys text").to_string();
    assert!(
        text.contains("Starting a new one"),
        "unexpected fallback text: {text}"
    );
    let ready = frames.iter().find(|f| f["type"] == "ready").expect("ready");
    assert_eq!(ready["sessionId"], "fallback-sid");
    assert_eq!(ready["resumed"], false);
}

#[tokio::test]
async fn missing_session_id_in_new_response_returns_an_error() {
    let agent = spawn_fake_agent(vec![
        FakeReply::Ok(json!({
            "agentCapabilities": { "promptCapabilities": {} }
        })),
        // session/new returns no sessionId field.
        FakeReply::Ok(json!({})),
    ]);

    let (tx, _rx) = mpsc::unbounded_channel::<Message>();
    let res = timeout(
        Duration::from_secs(2),
        negotiate_session(&agent, &tx, None, Some("/tmp".into()), BUILD_ID),
    )
    .await
    .expect("negotiate within 2s");

    let err = match res {
        Ok(_) => panic!("missing sessionId should error"),
        Err(e) => e,
    };
    assert!(
        format!("{err:#}").contains("session id"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn omitted_prompt_capabilities_default_to_empty_object() {
    // Agent is allowed to skip `agentCapabilities` entirely. The
    // browser expects an empty object so its capability checks
    // (caps.image, caps.embeddedContext) cleanly evaluate to false.
    let agent = spawn_fake_agent(vec![
        FakeReply::Ok(json!({})),
        FakeReply::Ok(json!({ "sessionId": "abc" })),
    ]);

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let _: NegotiationOutcome = timeout(
        Duration::from_secs(2),
        negotiate_session(&agent, &tx, None, Some("/tmp".into()), BUILD_ID),
    )
    .await
    .expect("negotiate within 2s")
    .expect("negotiate succeeds");

    let frames = drain_outbound(&mut rx);
    let ready = frames.iter().find(|f| f["type"] == "ready").expect("ready");
    assert!(ready["promptCapabilities"].is_object());
    assert_eq!(
        ready["promptCapabilities"].as_object().unwrap().len(),
        0,
        "default promptCapabilities should be an empty object"
    );
}
