//! Integration tests for `mezame::hub` plumbing. Drives the hub with
//! a test-only registration helper that takes a pre-built `Agent`
//! constructed via `Agent::from_io`. We bypass `spawn_agent` so the
//! tests stay deterministic and do not depend on a real ACP-speaking
//! binary on PATH.

use std::sync::Arc;
use std::time::Duration;

use mezame::agent::{from_io, Agent};
use mezame::config::{Config, TransportConfig};
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
///   `session/prompt` is auto-replied only when
///   `auto_reply_prompt` is true. Tests that need to drive a
///   mid-turn event (like a permission request) before the prompt
///   resolves should set this to false and let the prompt sit
///   open across the injected event.
///   Notifications (no `id`) are ignored, matching cat-style
///   behaviour for one-way messages.
/// - The `inject_tx` channel pushes "agent → mezame" frames the
///   tests want the hub to react to (e.g. `session/update`).
fn make_fake_agent_with(
    auto_reply_prompt: bool,
) -> (
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

    tokio::spawn(async move {
        let mut lines = BufReader::new(agent_stdin).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").is_some() && value.get("method").is_some() {
                let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                if method == "session/prompt" && !auto_reply_prompt {
                    continue;
                }
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

/// Default fixture: auto-replies to every request including
/// `session/prompt`. Used by tests that just need the prompt path
/// to resolve cleanly.
fn make_fake_agent() -> (
    Agent,
    mpsc::UnboundedReceiver<Value>,
    mpsc::UnboundedSender<Value>,
) {
    make_fake_agent_with(true)
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
            attach_id: attached.attach_id,
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

#[tokio::test]
async fn permission_request_is_targeted_at_the_prompter() {
    // Regression for the multi-attach bug where a permission
    // request showed up on every browser, not just the one that
    // started the turn. The hub now stamps targeted broadcasts
    // (permission and oauth requests) with `_target` carrying the
    // attach id of the sender; peer browsers drop them in the WS
    // write loop.
    let registry = HubRegistry::new();
    let (agent, updates_rx, inject) = make_fake_agent_with(false);
    let mut sender = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;
    let _peer = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    // Open a turn so the hub records the sender as the current
    // prompter. Drain the user-echo event so the assertion below
    // looks at the next emission.
    sender
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![json!({ "type": "text", "text": "search" })],
            attach_id: sender.attach_id,
        })
        .await
        .expect("send Prompt");
    // Drain the user-echo broadcast.
    let _ = timeout(Duration::from_millis(200), sender.outbound.recv()).await;

    // Inject a session/request_permission JSON-RPC request from
    // the agent. This is a top-level method, not a session/update
    // sub-kind. The hub forwards it as `permission_request` to the
    // browser.
    inject
        .send(json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "session/request_permission",
            "params": {
                "toolCall": { "title": "Allow web search?" },
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"}
                ]
            }
        }))
        .expect("inject");

    // Wait for the permission_request broadcast and assert it
    // carries `_target` equal to the sender's attach id. Skip the
    // user-echo and prompt_done frames.
    let mut saw_targeted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(200), sender.outbound.recv()).await {
            Ok(Ok(event)) => {
                if (*event)["type"] == "permission_request" {
                    let target = (*event)["_target"]
                        .as_u64()
                        .expect("permission targets are stamped");
                    assert_eq!(target, sender.attach_id);
                    saw_targeted = true;
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }
    assert!(
        saw_targeted,
        "hub must stamp permission_request with the prompter's attach id"
    );
}

#[tokio::test]
async fn cancel_command_forwards_session_cancel_to_agent() {
    // The hub's Cancel arm fires a `session/cancel` notification on
    // the agent. We verify the forwarding by parsing the line that
    // mezame writes to the agent's stdin and checking the method.
    let registry = HubRegistry::new();
    let (server_to_agent, agent_stdin) = tokio::io::duplex(8 * 1024);
    let (agent_stdout, server_reader) = tokio::io::duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);
    drop(agent_stdout); // we never inject anything

    // Capture the first line written to the agent's stdin.
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(agent_stdin).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = line_tx.send(line);
        }
    });

    let attached = registry
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
        .send(HubCommand::Cancel)
        .await
        .expect("send Cancel");

    // Wait for the cancel notification to land on the agent's stdin.
    let line = timeout(Duration::from_secs(2), line_rx.recv())
        .await
        .expect("cancel within 2s")
        .expect("channel still open");
    let value: Value = serde_json::from_str(&line).expect("valid JSON");
    assert_eq!(value["method"], "session/cancel");
    assert_eq!(value["params"]["sessionId"], SESSION_ID);
}

#[tokio::test]
async fn set_mode_broadcasts_updated_session_info() {
    // The hub awaits the agent reply, mutates the cached session_info
    // snapshot, and re-broadcasts it. Peers see the new currentModeId
    // immediately even though they did not initiate the change.
    let registry = HubRegistry::new();
    let (agent, updates_rx, _inject) = make_fake_agent();
    let session_info = Some(json!({
        "type": "session_info",
        "info": {
            "modes": {
                "currentModeId": "kiro_default",
                "availableModes": [
                    { "id": "kiro_default", "name": "Default" },
                    { "id": "kiro_planner", "name": "Planner" }
                ]
            }
        }
    }));

    let mut peer = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            session_info,
        )
        .await;
    let sender = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    sender
        .commands
        .send(HubCommand::SetMode {
            mode_id: "kiro_planner".into(),
        })
        .await
        .expect("send SetMode");

    // The peer should see a session_info broadcast carrying the new
    // currentModeId. Drain a few frames since the snapshot replay
    // lands first on attach.
    let mut saw_update = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(200), peer.outbound.recv()).await {
            Ok(Ok(event)) => {
                if (*event)["type"] == "session_info"
                    && (*event)["info"]["modes"]["currentModeId"] == "kiro_planner"
                {
                    saw_update = true;
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }
    assert!(saw_update, "peer should see updated session_info");
}

#[tokio::test]
async fn set_model_broadcasts_updated_session_info() {
    // Symmetric to set_mode but for the models half.
    let registry = HubRegistry::new();
    let (agent, updates_rx, _inject) = make_fake_agent();
    let session_info = Some(json!({
        "type": "session_info",
        "info": {
            "models": {
                "currentModelId": "claude-3-5-haiku",
                "availableModels": [
                    { "modelId": "claude-3-5-haiku", "name": "Haiku" },
                    { "modelId": "claude-3-5-sonnet", "name": "Sonnet" }
                ]
            }
        }
    }));

    let mut peer = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            session_info,
        )
        .await;
    let sender = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    sender
        .commands
        .send(HubCommand::SetModel {
            model_id: "claude-3-5-sonnet".into(),
        })
        .await
        .expect("send SetModel");

    let mut saw_update = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(200), peer.outbound.recv()).await {
            Ok(Ok(event)) => {
                if (*event)["type"] == "session_info"
                    && (*event)["info"]["models"]["currentModelId"] == "claude-3-5-sonnet"
                {
                    saw_update = true;
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }
    assert!(saw_update, "peer should see updated session_info");
}

#[tokio::test]
async fn empty_prompt_blocks_are_silently_ignored() {
    // The Prompt arm short-circuits when blocks is empty: no echo,
    // no agent request, no prompt_done. This guards against an
    // accidental empty submit from the client.
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
            blocks: vec![],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");

    // Wait briefly to give the hub a chance to broadcast something.
    // It should not.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        attached.outbound.try_recv().is_err(),
        "empty prompt must not produce any broadcast"
    );
}

#[tokio::test]
async fn oauth_request_is_targeted_at_the_prompter() {
    // OAuth requests follow the same targeting rule as permission
    // requests: only the browser that started the turn sees them.
    let registry = HubRegistry::new();
    let (agent, updates_rx, inject) = make_fake_agent_with(false);
    let mut sender = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;

    sender
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![json!({ "type": "text", "text": "auth" })],
            attach_id: sender.attach_id,
        })
        .await
        .expect("send Prompt");
    // Drain user-echo.
    let _ = timeout(Duration::from_millis(200), sender.outbound.recv()).await;

    inject
        .send(json!({
            "jsonrpc": "2.0",
            "method": "_kiro.dev/mcp/oauth_request",
            "params": {
                "serverName": "github",
                "url": "https://example.com/oauth"
            }
        }))
        .expect("inject");

    let mut saw_targeted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(200), sender.outbound.recv()).await {
            Ok(Ok(event)) => {
                if (*event)["type"] == "mcp_oauth_request" {
                    let target = (*event)["_target"]
                        .as_u64()
                        .expect("oauth targets are stamped");
                    assert_eq!(target, sender.attach_id);
                    saw_targeted = true;
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }
    assert!(
        saw_targeted,
        "hub must stamp mcp_oauth_request with the prompter's attach id"
    );
}

/// Minimal config for the `attach_or_create` fast path. The fast path
/// returns before the config is ever read (it only matters when a new
/// hub has to be built via `spawn_agent`), so the values here are
/// placeholders that never get exercised.
fn dummy_config() -> Arc<Config> {
    Arc::new(Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".into(),
        }],
        agent_cmd: "cat".into(),
        agent_args: vec![],
    })
}

#[tokio::test]
async fn attach_or_create_fast_path_reuses_registered_hub() {
    // A hub is already registered for SESSION_ID (via the test helper).
    // `attach_or_create` with that same id as the resume key must take
    // the fast path: look the hub up, subscribe, and return without
    // spawning a fresh agent. We then prove the returned attach shares
    // the live hub by injecting an agent message and seeing it on both
    // the original and the fast-path subscriber.
    let registry = HubRegistry::new();
    let (agent, updates_rx, inject) = make_fake_agent();

    let mut original = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;

    let mut fast = registry
        .attach_or_create(dummy_config(), Some(SESSION_ID.into()), None, "test-build")
        .await
        .expect("fast path attach");

    assert_eq!(fast.session_id, SESSION_ID);
    // Snapshot replay marks every attach as resumed=true.
    assert_eq!(fast.snapshot_ready["resumed"], true);

    inject
        .send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "text": "shared" }
                }
            }
        }))
        .expect("inject");

    let on_original = timeout(Duration::from_secs(2), original.outbound.recv())
        .await
        .expect("original sees event")
        .expect("channel open");
    let on_fast = timeout(Duration::from_secs(2), fast.outbound.recv())
        .await
        .expect("fast-path subscriber sees event")
        .expect("channel open");
    assert_eq!((*on_original)["text"], "shared");
    assert_eq!(*on_original, *on_fast);
}

#[tokio::test]
async fn detach_to_zero_then_reattach_exercises_grace_counter() {
    // Drives the subscriber-count lifecycle: the last detach drops the
    // count to zero (Counter::decrement → GraceEvent::Empty → the hub
    // arms its grace timer via install_cancel), and a fresh attach
    // inside the window climbs back above zero (Counter::increment
    // fires the cancel one-shot and GraceEvent::Refilled, disarming the
    // timer). We assert the hub is still live afterwards by injecting an
    // agent message and seeing it on the re-attached subscriber.
    let registry = HubRegistry::new();
    let (agent, updates_rx, inject) = make_fake_agent();

    let first = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;

    // Drop the only subscriber. The Drop impl spawns the decrement, so
    // give the runtime a moment to run it and let the hub loop process
    // the resulting GraceEvent::Empty and install its cancel handle.
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Re-attach inside the grace window. The hub is still registered
    // (the grace timer has not fired), so this climbs the count back to
    // one and cancels the pending shutdown.
    let mut second = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub still registered inside grace window");

    inject
        .send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "text": "still alive" }
                }
            }
        }))
        .expect("inject");

    let event = timeout(Duration::from_secs(2), second.outbound.recv())
        .await
        .expect("re-attached subscriber sees event")
        .expect("channel open");
    assert_eq!((*event)["text"], "still alive");
}
