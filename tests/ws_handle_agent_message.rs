//! Tests for the agent-to-browser dispatch table in
//! `mezame::ws::handle_agent_message`. Each branch reshapes a JSON-RPC
//! agent message into a Mezame WS event; this suite asserts the wire
//! shape of every emitted event.

use std::time::Duration;

use axum::extract::ws::Message;
use mezame::ws::handle_agent_message;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Drain whatever `handle_agent_message` pushed onto the channel and
/// deserialise it into a JSON `Value`. Asserts the frame is a Text
/// frame; non-text frames are not used by this code path.
fn recv_event(rx: &mut mpsc::UnboundedReceiver<Message>) -> Value {
    let msg = rx.try_recv().expect("expected a WS event on the channel");
    match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("event was not valid JSON"),
        other => panic!("expected Text frame, got {:?}", other),
    }
}

fn assert_no_event(rx: &mut mpsc::UnboundedReceiver<Message>) {
    // After `dispatch` returns, the sender has been dropped (it was a
    // local in the helper), so the channel reads as Disconnected. The
    // important invariant is that no message landed before the drop;
    // try_recv reports that as either Empty (sender still around) or
    // Disconnected (sender gone). Both are acceptable here.
    match rx.try_recv() {
        Err(mpsc::error::TryRecvError::Empty) => {}
        Err(mpsc::error::TryRecvError::Disconnected) => {}
        Ok(msg) => panic!("expected no event, got {:?}", msg),
    }
}

async fn dispatch(msg: Value, suppress: bool) -> mpsc::UnboundedReceiver<Message> {
    let (tx, rx) = mpsc::unbounded_channel();
    timeout(
        Duration::from_secs(2),
        handle_agent_message(&tx, msg, suppress),
    )
    .await
    .expect("dispatch timed out");
    rx
}

// ---------- session/update sub-kinds ----------

#[tokio::test]
async fn agent_message_chunk_emits_append_with_agent_role() {
    let msg = json!({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "text": "hello" }
            }
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "append");
    assert_eq!(event["role"], "agent");
    assert_eq!(event["text"], "hello");
}

#[tokio::test]
async fn user_message_chunk_emits_append_with_user_role_and_prefix() {
    let msg = json!({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "content": { "text": "what's the weather" }
            }
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "append");
    assert_eq!(event["role"], "user");
    let text = event["text"].as_str().expect("text is a string");
    assert!(
        text.starts_with("> "),
        "user replay should start with '> ', got {text:?}"
    );
    assert!(text.contains("what's the weather"));
}

#[tokio::test]
async fn agent_thought_chunk_emits_thought_event() {
    let msg = json!({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": { "text": "considering options" }
            }
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "thought");
    assert_eq!(event["text"], "considering options");
}

// ---------- tool_call ----------

#[tokio::test]
async fn tool_call_with_id_passes_through_payload() {
    let msg = json!({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-42",
                "title": "Read file",
                "status": "in_progress",
                "kind": "file_read",
                "rawInput": { "path": "/tmp/x" },
                "content": [{ "kind": "text", "data": "ok" }],
                "locations": [{ "path": "/tmp/x", "line": 1 }]
            }
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "tool_call");
    assert_eq!(event["toolCallId"], "tc-42");
    assert_eq!(event["title"], "Read file");
    assert_eq!(event["status"], "in_progress");
    assert_eq!(event["kind"], "file_read");
    assert_eq!(event["rawInput"], json!({ "path": "/tmp/x" }));
    assert_eq!(event["content"], json!([{ "kind": "text", "data": "ok" }]));
    assert_eq!(event["locations"], json!([{ "path": "/tmp/x", "line": 1 }]));
}

#[tokio::test]
async fn tool_call_without_id_falls_back_to_sys_append() {
    let msg = json!({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "tool_call",
                "title": "Untracked tool",
                "status": "completed"
            }
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "append");
    assert_eq!(event["role"], "sys");
    let text = event["text"].as_str().expect("text is a string");
    assert!(text.contains("Untracked tool"), "got {text:?}");
    assert!(text.contains("completed"), "status missing, got {text:?}");
}

#[tokio::test]
async fn tool_call_update_merges_via_same_event_type() {
    // The server emits the same `tool_call` event for both `tool_call`
    // and `tool_call_update`; the browser dedupes by toolCallId. This
    // test asserts the server-side equivalence.
    let msg = json!({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-99",
                "status": "completed"
            }
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "tool_call");
    assert_eq!(event["toolCallId"], "tc-99");
    assert_eq!(event["status"], "completed");
}

// ---------- session/request_permission ----------

#[tokio::test]
async fn permission_request_passes_through_id_title_options() {
    let msg = json!({
        "id": 7,
        "method": "session/request_permission",
        "params": {
            "toolCall": { "title": "Run shell command" },
            "options": [
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
            ]
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "permission_request");
    assert_eq!(event["id"], 7);
    assert_eq!(event["title"], "Run shell command");
    assert_eq!(event["options"].as_array().map(|a| a.len()), Some(2));
    assert_eq!(event["options"][0]["optionId"], "allow");
}

#[tokio::test]
async fn permission_request_falls_back_to_name_when_title_missing() {
    let msg = json!({
        "id": 8,
        "method": "session/request_permission",
        "params": {
            "toolCall": { "name": "shell.exec" },
            "options": []
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["title"], "shell.exec");
}

#[tokio::test]
async fn permission_request_is_emitted_even_during_resume_suppression() {
    let msg = json!({
        "id": 9,
        "method": "session/request_permission",
        "params": {
            "toolCall": { "title": "x" },
            "options": []
        }
    });
    // Suppression flag is true: only `session/update` events should be
    // dropped. Permission requests must still reach the browser.
    let mut rx = dispatch(msg, true).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "permission_request");
}

// ---------- Kiro extensions ----------

#[tokio::test]
async fn kiro_commands_available_drops_tools_catalogue() {
    let msg = json!({
        "method": "_kiro.dev/commands/available",
        "params": {
            "commands": [{ "name": "/clear", "description": "Clear the log" }],
            "prompts": [{ "name": "summarise" }],
            // The big tools catalogue must NOT be forwarded.
            "tools": [{ "name": "bigTool", "schema": "..." }]
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "commands");
    assert_eq!(event["commands"][0]["name"], "/clear");
    assert_eq!(event["prompts"][0]["name"], "summarise");
    assert!(
        event.get("tools").is_none(),
        "tools catalogue must be dropped, got {event:?}"
    );
}

#[tokio::test]
async fn kiro_mcp_oauth_request_passes_through_canonical_fields() {
    let msg = json!({
        "method": "_kiro.dev/mcp/oauth_request",
        "params": {
            "serverName": "github",
            "url": "https://example.com/auth",
            "id": "req-1"
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["type"], "mcp_oauth_request");
    assert_eq!(event["serverName"], "github");
    assert_eq!(event["url"], "https://example.com/auth");
    assert_eq!(event["id"], "req-1");
}

#[tokio::test]
async fn kiro_mcp_oauth_request_resolves_alternative_field_names() {
    // Some agents send `name` and `authUrl` instead of `serverName` and
    // `url`. Both are accepted and resolved to the canonical names.
    let msg = json!({
        "method": "_kiro.dev/mcp/oauth_request",
        "params": {
            "name": "drive",
            "authUrl": "https://accounts.example/oauth",
            "requestId": 42
        }
    });
    let mut rx = dispatch(msg, false).await;
    let event = recv_event(&mut rx);
    assert_eq!(event["serverName"], "drive");
    assert_eq!(event["url"], "https://accounts.example/oauth");
    assert_eq!(event["id"], 42);
}

#[tokio::test]
async fn kiro_mcp_oauth_request_without_url_is_dropped() {
    let msg = json!({
        "method": "_kiro.dev/mcp/oauth_request",
        "params": { "serverName": "github" }
    });
    let mut rx = dispatch(msg, false).await;
    assert_no_event(&mut rx);
}

// ---------- suppression and unknowns ----------

#[tokio::test]
async fn session_update_is_dropped_when_suppression_is_on() {
    let msg = json!({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "text": "replay chunk" }
            }
        }
    });
    let mut rx = dispatch(msg, true).await;
    assert_no_event(&mut rx);
}

#[tokio::test]
async fn unknown_method_is_dropped_silently() {
    let msg = json!({
        "method": "_kiro.dev/something/new",
        "params": {}
    });
    let mut rx = dispatch(msg, false).await;
    assert_no_event(&mut rx);
}

#[tokio::test]
async fn session_update_with_unknown_subkind_is_dropped() {
    let msg = json!({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "future_kind",
                "content": { "text": "x" }
            }
        }
    });
    let mut rx = dispatch(msg, false).await;
    assert_no_event(&mut rx);
}
