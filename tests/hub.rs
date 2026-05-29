//! Integration tests for `mezame::hub` plumbing. Drives the hub with
//! a test-only registration helper that takes a pre-built `Agent`
//! constructed via `Agent::from_io`. We bypass `spawn_agent` so the
//! tests stay deterministic and do not depend on a real ACP-speaking
//! binary on PATH.

use std::sync::Arc;
use std::time::Duration;

use mezame::agent::{from_io, Agent};
use mezame::hub::{HubCommand, HubRegistry};
use serde_json::{json, Value};
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::timeout;

const SESSION_ID: &str = "test-session";

/// Build an `Agent` from a duplex pipe plus a fake server. The
/// returned channels mirror what mezame sees in production:
///
/// - The reader task parses every line mezame writes to the agent's
///   stdin. For requests (those carrying an `id` and a `method`)
///   it auto-replies with a stub `result: {}` so the hub's pending
///   oneshot resolves and prompt-task continuations can fire (e.g.
///   the broadcast of `prompt_done` in the new prompt path).
///   Notifications (no `id`) are ignored, matching cat-style
///   behaviour for one-way messages.
/// - The `inject_tx` channel pushes "agent → mezame" frames the
///   tests want the hub to react to (e.g. `session/update`).
fn make_fake_agent() -> (
    Agent,
    mpsc::UnboundedReceiver<Value>,
    mpsc::UnboundedSender<Value>,
) {
    let (server_to_agent, agent_stdin) = duplex(8 * 1024);
    let (agent_stdout, server_reader) = duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);

    let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<Value>();
    // Internal channel: reader → writer carries auto-reply requests
    // so we keep all stdout writes serialised through one task.
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Value>();

    // Reader: parses every request line, queues a stub reply.
    tokio::spawn(async move {
        let mut lines = BufReader::new(agent_stdin).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            // Notifications carry a method but no id; ignore them.
            // Requests carry an id and a method; auto-reply.
            if value.get("id").is_some() && value.get("method").is_some() {
                let _ = reply_tx.send(json!({
                    "jsonrpc": "2.0",
                    "id": value["id"].clone(),
                    "result": {}
                }));
            }
        }
    });

    // Writer: drains both the inject channel (test-driven) and the
    // reply channel (auto-replies for anything the reader saw),
    // serialised to a single tokio::io::write_all call per frame.
    tokio::spawn(async move {
        let mut stdout = agent_stdout;
        loop {
            let frame = tokio::select! {
                v = inject_rx.recv() => v,
                v = reply_rx.recv() => v,
            };
            let Some(value) = frame else { break };
            let line = format!("{value}\n");
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    (agent, updates_rx, inject_tx)
}

fn ready_event() -> Value {
    json!({
        "type": "ready",
        "sessionId": SESSION_ID,
        "resumed": false,
        "cwd": "/tmp",
        "promptCapabilities": {},
        "buildId": "test"
    })
}

#[tokio::test]
async fn snapshot_replays_to_each_attached_subscriber() {
    let registry = HubRegistry::new();
    let (agent, updates_rx, _inject) = make_fake_agent();
    let session_info = Some(json!({ "type": "session_info", "info": {} }));

    let attached_a = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            session_info.clone(),
        )
        .await;

    assert_eq!(attached_a.snapshot_ready["type"], "ready");
    assert_eq!(attached_a.snapshot_ready["sessionId"], SESSION_ID);
    assert!(attached_a.snapshot_session_info.is_some());

    // A second subscriber attaching to the same hub should see the
    // same snapshot, regardless of when it joins.
    let attached_b = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");
    assert_eq!(attached_b.snapshot_ready, attached_a.snapshot_ready);
    assert_eq!(
        attached_b.snapshot_session_info,
        attached_a.snapshot_session_info
    );
}

#[tokio::test]
async fn agent_updates_broadcast_to_every_subscriber() {
    let registry = HubRegistry::new();
    let (agent, updates_rx, inject) = make_fake_agent();

    let mut attached_a = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;
    let mut attached_b = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    // Inject an agent message. The hub's owner loop should see it on
    // the updates receiver, run it through `handle_agent_message`,
    // then broadcast the resulting event to every subscriber.
    inject
        .send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "text": "hello" }
                }
            }
        }))
        .expect("inject");

    let event_a = timeout(Duration::from_secs(2), attached_a.outbound.recv())
        .await
        .expect("event A within 2s")
        .expect("channel still open");
    let event_b = timeout(Duration::from_secs(2), attached_b.outbound.recv())
        .await
        .expect("event B within 2s")
        .expect("channel still open");

    // Both subscribers see the same broadcasted append event.
    assert_eq!((*event_a)["type"], "append");
    assert_eq!((*event_a)["role"], "agent");
    assert_eq!((*event_a)["text"], "hello");
    assert_eq!(*event_a, *event_b);
}

#[tokio::test]
async fn first_permission_response_wins_silently() {
    // Two subscribers attached, both reply to the same permission id.
    // The first reply should reach the agent (the agent's stdin would
    // normally observe it; we verify via the absence of an error
    // event from the hub). The second reply must not error to the
    // browser; per the simplified stage-1 design we drop duplicates.
    let registry = HubRegistry::new();
    let (agent, updates_rx, _inject) = make_fake_agent();
    let attached_a = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;
    let attached_b = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    let id = json!(42);
    attached_a
        .commands
        .send(HubCommand::PermissionResponse {
            id: id.clone(),
            option_id: "allow".into(),
        })
        .await
        .expect("send A");
    attached_b
        .commands
        .send(HubCommand::PermissionResponse {
            id: id.clone(),
            option_id: "reject".into(),
        })
        .await
        .expect("send B");

    // Give the loop time to process. Neither subscriber should see a
    // user-facing error broadcast.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut a = attached_a;
    assert!(
        a.outbound.try_recv().is_err(),
        "no error event should be broadcast"
    );
}

#[tokio::test]
async fn prompt_done_is_broadcast_after_session_prompt_resolves() {
    // Regression for the multi-attach bug where a sender's `busy`
    // flag never cleared because the hub forgot to emit
    // `prompt_done` after the agent's `session/prompt` request
    // completed. Without this event the composer reads "Agent is
    // working" indefinitely on the sender (and on any peer browser
    // that subsequently reads the broadcast for cancel / take-over
    // purposes).
    // Send a prompt through the hub. The fixture's reader auto-
    // replies with a stub result, so the hub's pending oneshot for
    // `session/prompt` resolves immediately and the prompt task's
    // continuation broadcasts `prompt_done`.
    let registry = HubRegistry::new();
    let (agent, updates_rx, _inject) = make_fake_agent();
    let mut attached = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![json!({ "type": "text", "text": "hi" })],
        })
        .await
        .expect("send Prompt");

    // Drain broadcast events and assert the user-prompt echo lands
    // first, followed by `prompt_done` once the agent's stub reply
    // resolves.
    let mut saw_user_echo = false;
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(200), attached.outbound.recv()).await {
            Ok(Ok(event)) => {
                let event_type = (*event)["type"].as_str().unwrap_or("");
                if event_type == "append" && (*event)["role"] == "user" {
                    saw_user_echo = true;
                }
                if event_type == "prompt_done" {
                    saw_done = true;
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }
    assert!(saw_user_echo, "should broadcast the user-prompt echo");
    assert!(
        saw_done,
        "hub must emit prompt_done after the agent reply so sender busy clears"
    );
}
