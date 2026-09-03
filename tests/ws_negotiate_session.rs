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
    // The ready frame must carry the id the browser asked to resume. The
    // client keeps it pinned and its durable pointer survives the
    // throwaway fallback id. This is the server half of the fix for the
    // vanishing/overwritten-session bug.
    assert_eq!(
        ready["resumeFailedFor"], "missing-sid",
        "fallback ready must report the original id the resume was for"
    );
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

#[tokio::test(start_paused = true)]
async fn resume_of_live_locked_session_refuses_instead_of_clobbering() {
    // session/load keeps returning the stale-lock error and the lock
    // cannot be stolen: no dead PID to reap, and no lockfile for this
    // fixture id. try_load_session exhausts its retry budget and
    // surfaces "active in another process". negotiate_session must NOT
    // paper over that by starting a fresh session, which would discard
    // the live one. It returns an error, and the client reconnects once
    // the owner releases the lock.
    let mut replies = vec![FakeReply::Ok(json!({
        "agentCapabilities": { "promptCapabilities": {} }
    }))];
    // One reply per load attempt in try_load_session's retry budget.
    for _ in 0..6 {
        replies.push(FakeReply::Err(
            "Session is active in another process (pid 999999)".into(),
        ));
    }
    let agent = spawn_fake_agent(replies);

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let res = timeout(
        Duration::from_secs(5),
        negotiate_session(
            &agent,
            &tx,
            Some("live-elsewhere-sid".into()),
            Some("/tmp".into()),
            BUILD_ID,
        ),
    )
    .await
    .expect("negotiate within 5s");

    let err = match res {
        Ok(_) => panic!("resume of a live-locked session must not succeed via a new session"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.contains("active in another process"),
        "error should explain the session is held elsewhere: {err}"
    );
    assert!(
        err.contains("live-elsewhere-sid"),
        "error should name the contended session id: {err}"
    );

    // No fresh session started: no `ready`, no `session_info`, and no
    // "Starting a new one" fallback notice.
    let frames = drain_outbound(&mut rx);
    assert!(
        frames.iter().all(|f| f["type"] != "ready"),
        "no ready frame on a refused resume: {frames:?}"
    );
    assert!(
        frames.iter().all(|f| f["type"] != "session_info"),
        "no session_info on a refused resume: {frames:?}"
    );
    let has_fallback_notice = frames.iter().any(|f| {
        f["text"]
            .as_str()
            .is_some_and(|t| t.contains("Starting a new one"))
    });
    assert!(
        !has_fallback_notice,
        "must not emit the fallback notice: {frames:?}"
    );
}
