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

/// Build an `Agent` from a duplex pipe plus a fake server that reads
/// every JSON-RPC line we send, then writes back whatever the test
/// harness pushes. Returns the agent, the updates receiver, and a
/// channel the test uses to inject "agent → mezame" frames.
fn make_fake_agent() -> (
    Agent,
    mpsc::UnboundedReceiver<Value>,
    mpsc::UnboundedSender<Value>,
) {
    let (server_to_agent, agent_stdin) = duplex(8 * 1024);
    let (agent_stdout, server_reader) = duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);

    let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<Value>();

    // Reader: discards everything coming from the hub side. We do not
    // assert on the wire shape here; the agent_jsonrpc tests cover that.
    tokio::spawn(async move {
        let mut lines = BufReader::new(agent_stdin).lines();
        while let Ok(Some(_)) = lines.next_line().await {}
    });

    // Writer: forwards injected JSON values to the hub's update reader
    // as newline-delimited JSON.
    tokio::spawn(async move {
        let mut stdout = agent_stdout;
        while let Some(value) = inject_rx.recv().await {
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
