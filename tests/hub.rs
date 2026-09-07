//! Integration tests for `mezame::hub`. Every case drives the hub through
//! a `ScriptedBackend`, so the surviving transport behaviour is asserted
//! with no subprocess, no socket and no file.
//!
//! What these cases protect: multi-browser fan-out, subscriber counting,
//! grace teardown, the capped in-flight hold, the `_target` stamp, and the
//! frames that open and close a turn. A regression in any of them stays
//! invisible until a real conversation runs, which is why they are pinned
//! here against a Backend that produces exactly what the test says.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mezame::backend::Backend;
use mezame::hub::{HubCommand, HubRegistry, RegistryFull, MAX_PROMPT_TEXT_BYTES};
use serde_json::{json, Value};
use support::{Invocation, Release, Resolution, ScriptedBackend, ScriptedTurn};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;

const SESSION_ID: &str = "test-session";

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

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// One `append` with `role` `agent`, the shape a Backend streams.
fn agent_append(text: &str) -> Value {
    json!({ "type": "append", "role": "agent", "text": text })
}

/// One `permission_request` under `id`, the shape a Backend streams.
fn permission_request(id: Value) -> Value {
    json!({
        "type": "permission_request",
        "id": id,
        "title": "Allow?",
        "options": [
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
        ]
    })
}

/// Collect broadcast events until one of type `stop_type` arrives or two
/// seconds pass. The stopping event is included.
async fn collect_until(rx: &mut broadcast::Receiver<Arc<Value>>, stop_type: &str) -> Vec<Value> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Ok(event)) => {
                let is_stop = event["type"] == stop_type;
                seen.push((*event).clone());
                if is_stop {
                    break;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_) => continue,
        }
    }
    seen
}

/// The next broadcast event, or `None` within two seconds.
async fn next_event(rx: &mut broadcast::Receiver<Arc<Value>>) -> Option<Value> {
    match timeout(Duration::from_secs(2), rx.recv()).await {
        Ok(Ok(event)) => Some((*event).clone()),
        _ => None,
    }
}

/// Poll until the hub for `session_id` is gone from the registry.
async fn wait_until_unregistered(registry: &HubRegistry, session_id: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if !registry.is_registered_for_test(session_id).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Poll until `wanted` shows up in the invocation log.
async fn wait_for_invocation(backend: &ScriptedBackend, wanted: &Invocation) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if backend.saw(wanted) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn snapshot_replays_to_each_attached_subscriber() {
    // Requirement 7 criterion 2. Both attaches subscribe with no turn in
    // flight, so `busy` is false on each and the two `ready` values agree
    // in every field.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new());
    let session_info = Some(json!({ "type": "session_info", "info": { "models": {} } }));

    let attached_a = registry
        .register_for_test(
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            session_info.clone(),
        )
        .await;

    assert_eq!(attached_a.snapshot_ready["type"], "ready");
    assert_eq!(attached_a.snapshot_ready["sessionId"], SESSION_ID);
    assert_eq!(attached_a.snapshot_ready["busy"], false);
    assert!(attached_a.snapshot_session_info.is_some());

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
    // Requirement 7 criterion 1: every attach receives the same event
    // value, within 2 seconds.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(vec![
        agent_append("hello"),
    ])));

    let mut attached_a = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;
    let mut attached_b = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    attached_a
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("hi")],
            attach_id: attached_a.attach_id,
        })
        .await
        .expect("send Prompt");

    let on_a = collect_until(&mut attached_a.outbound, "prompt_done").await;
    let on_b = collect_until(&mut attached_b.outbound, "prompt_done").await;
    assert_eq!(on_a, on_b, "both attaches see the same sequence");

    let streamed: Vec<&Value> = on_a
        .iter()
        .filter(|e| e["type"] == "append" && e["role"] == "agent")
        .collect();
    assert_eq!(streamed.len(), 1);
    assert_eq!(streamed[0]["text"], "hello");
}

#[tokio::test]
async fn first_permission_response_wins_silently() {
    // Requirement 7 criterion 12: neither answer produces a broadcast, and
    // only the first reaches the Backend. The Backend raises the request
    // first: the hub lets an answer through for a request it broadcast
    // and for nothing else.
    let registry = HubRegistry::new();
    let id = json!(42);
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![
        permission_request(id.clone()),
    ])));
    let mut attached_a = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;
    let attached_b = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    attached_a
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("do it")],
            attach_id: attached_a.attach_id,
        })
        .await
        .expect("send Prompt");
    let raised = collect_until(&mut attached_a.outbound, "permission_request").await;
    assert!(
        raised.iter().any(|e| e["type"] == "permission_request"),
        "the request is broadcast before anyone answers"
    );

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

    let answered = Invocation::PermissionResponse {
        id: id.clone(),
        option_id: "allow".into(),
    };
    assert!(
        wait_for_invocation(&backend, &answered).await,
        "the first answer reaches the Backend"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        attached_a.outbound.try_recv().is_err(),
        "no event is broadcast in response to either answer"
    );
    let rejected = Invocation::PermissionResponse {
        id,
        option_id: "reject".into(),
    };
    assert!(
        !backend.saw(&rejected),
        "the second answer for the same id is dropped"
    );

    backend.release_turn(Release::Ok);
    let tail = collect_until(&mut attached_a.outbound, "prompt_done").await;
    assert!(tail.iter().any(|e| e["type"] == "prompt_done"));
}

#[tokio::test]
async fn prompt_done_is_broadcast_after_session_prompt_resolves() {
    // Requirement 7 criterion 17: the user echo first, every streamed
    // event between, and exactly one `prompt_done` at the end. Without the
    // terminal frame the composer reads "working" forever.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(vec![
        agent_append("one"),
        agent_append("two"),
    ])));
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("hi")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");

    let seen = collect_until(&mut attached.outbound, "prompt_done").await;
    let types: Vec<&str> = seen
        .iter()
        .map(|e| e["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(types, vec!["append", "append", "append", "prompt_done"]);
    assert_eq!(seen[0]["role"], "user");
    assert_eq!(seen[0]["text"], "> hi\n");
    assert_eq!(seen[1]["text"], "one");
    assert_eq!(seen[2]["text"], "two");
    assert_eq!(
        seen.iter().filter(|e| e["type"] == "prompt_done").count(),
        1,
        "exactly one prompt_done"
    );
}

#[tokio::test]
async fn permission_request_is_targeted_at_the_prompter() {
    // Requirement 7 criterion 9. The hub stamps `_target` with the
    // prompter's attach id; the attach loop drops the frame everywhere
    // else, so a peer never renders a card it was not asked to answer.
    let registry = HubRegistry::new();
    let request = json!({
        "type": "permission_request",
        "id": "perm-1",
        "title": "Allow web search?",
        "options": [
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
        ]
    });
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![
        request,
    ])));
    let mut sender = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;
    let mut peer = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    sender
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("search")],
            attach_id: sender.attach_id,
        })
        .await
        .expect("send Prompt");

    let on_sender = collect_until(&mut sender.outbound, "permission_request").await;
    let card = on_sender
        .iter()
        .find(|e| e["type"] == "permission_request")
        .expect("the card is broadcast");
    assert_eq!(
        card["_target"].as_u64(),
        Some(sender.attach_id),
        "stamped with the prompter's attach id"
    );
    assert_eq!(card["title"], "Allow web search?");

    // The peer receives the same stamped frame on the broadcast; the
    // attach loop is what drops it there.
    let on_peer = collect_until(&mut peer.outbound, "permission_request").await;
    assert_eq!(on_peer, on_sender);

    backend.release_turn(Release::Ok);
    let tail = collect_until(&mut sender.outbound, "prompt_done").await;
    assert!(tail.iter().any(|e| e["type"] == "prompt_done"));
}

#[tokio::test]
async fn untargeted_events_carry_no_target_stamp() {
    // Requirement 7 criterion 11: an `append`, a `thought` and a
    // `tool_call` are stamped with nothing.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(vec![
        agent_append("text"),
        json!({ "type": "thought", "text": "thinking" }),
        json!({
            "type": "tool_call",
            "toolCallId": "t1",
            "title": "Read",
            "status": "completed",
            "kind": null,
            "rawInput": {},
            "content": null,
            "locations": null
        }),
    ])));
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("go")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");

    let seen = collect_until(&mut attached.outbound, "prompt_done").await;
    for event in &seen {
        assert!(
            event.get("_target").is_none(),
            "no untargeted event carries a stamp: {event}"
        );
    }
    assert!(seen.iter().any(|e| e["type"] == "thought"));
    assert!(seen.iter().any(|e| e["type"] == "tool_call"));
}

#[tokio::test]
async fn cancel_command_forwards_session_cancel_to_agent() {
    // Requirement 7 criterion 13: the Backend's cancel method is invoked
    // exactly once, and the arm broadcasts nothing.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new());
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Cancel)
        .await
        .expect("send Cancel");

    assert!(
        wait_for_invocation(&backend, &Invocation::Cancel).await,
        "the cancel reaches the Backend"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        backend.count_of(&Invocation::Cancel),
        1,
        "exactly one cancel"
    );
    assert!(
        attached.outbound.try_recv().is_err(),
        "the cancel arm broadcasts nothing"
    );
}

#[tokio::test]
async fn set_model_broadcasts_updated_session_info() {
    // Requirement 7 criterion 19: the returned `info` goes out as a
    // `session_info` frame and is replayed to every later attach.
    let registry = HubRegistry::new();
    let info = json!({
        "models": {
            "currentModelId": "sonnet",
            "availableModels": [
                { "modelId": "haiku", "name": "Haiku" },
                { "modelId": "sonnet", "name": "Sonnet" }
            ]
        }
    });
    let backend = Arc::new(ScriptedBackend::new().set_model_outcome(Ok(info.clone())));

    let mut peer = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;
    let sender = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    sender
        .commands
        .send(HubCommand::SetModel {
            model_id: "sonnet".into(),
        })
        .await
        .expect("send SetModel");

    let frame = next_event(&mut peer.outbound)
        .await
        .expect("the peer sees a frame");
    assert_eq!(frame["type"], "session_info");
    assert_eq!(frame["info"], info);

    // Requirement 7 criterion 19's replay clause: a later attach reads the
    // same value from the snapshot.
    let later = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");
    assert_eq!(
        later.snapshot_session_info,
        Some(json!({ "type": "session_info", "info": info }))
    );
    assert!(backend.saw(&Invocation::SetModel("sonnet".into())));
}

#[tokio::test]
async fn a_refused_model_change_broadcasts_a_sys_notice_and_no_session_info() {
    // Requirement 3 criterion 17. The sender already shows the new
    // selection optimistically; the notice is what tells everyone it did
    // not take.
    let registry = HubRegistry::new();
    let backend = Arc::new(
        ScriptedBackend::new().set_model_outcome(Err("no model is selectable".to_string())),
    );
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::SetModel {
            model_id: "nope".into(),
        })
        .await
        .expect("send SetModel");

    let frame = next_event(&mut attached.outbound)
        .await
        .expect("a frame arrives");
    assert_eq!(frame["type"], "append");
    assert_eq!(frame["role"], "sys");
    assert!(
        frame["text"]
            .as_str()
            .is_some_and(|t| t.contains("no model is selectable")),
        "the notice names the failure, got {frame}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        attached.outbound.try_recv().is_err(),
        "no session_info follows a refused change"
    );
}

#[tokio::test]
async fn empty_prompt_blocks_are_silently_ignored() {
    // Requirement 7 criterion 18: no broadcast within 150ms and no
    // Backend method invoked. Guards an accidental empty submit.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(vec![])));
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        attached.outbound.try_recv().is_err(),
        "an empty prompt must not produce any broadcast"
    );
    assert!(
        backend.invocations().is_empty(),
        "an empty prompt must not reach the Backend"
    );
}

#[tokio::test]
async fn attach_or_create_fast_path_reuses_registered_hub() {
    // Requirement 7 criterion 3: the one-argument attach finds the
    // registered hub, creates no second Backend, and both attaches see
    // every later event.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(vec![
        agent_append("shared"),
    ])));

    let mut original = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    let mut fast = registry
        .attach_or_create(SESSION_ID)
        .await
        .expect("fast path attach");

    assert_eq!(fast.session_id, SESSION_ID);
    assert_eq!(fast.snapshot_ready["resumed"], true);
    assert_eq!(fast.snapshot_ready["busy"], false);

    original
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("hi")],
            attach_id: original.attach_id,
        })
        .await
        .expect("send Prompt");

    let on_original = collect_until(&mut original.outbound, "prompt_done").await;
    let on_fast = collect_until(&mut fast.outbound, "prompt_done").await;
    assert!(on_original
        .iter()
        .any(|e| e["role"] == "agent" && e["text"] == "shared"));
    assert_eq!(on_original, on_fast);
    assert_eq!(
        backend.prompt_count(),
        1,
        "one Backend behind both attaches"
    );
}

#[tokio::test(start_paused = true)]
async fn detach_to_zero_then_reattach_exercises_grace_counter() {
    // Requirement 7 criterion 4, its three clauses: the last detach arms
    // the grace timer, a fresh attach inside the window cancels the
    // pending teardown, and the hub keeps its registry entry and keeps
    // delivering. Under the paused clock with a 50ms window a regression
    // of both cancel paths fires inside the test, not 29 seconds after
    // it ends as it did against the production period.
    let grace = Duration::from_millis(50);
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(vec![
        agent_append("still alive"),
    ])));

    let first = registry
        .register_for_test_with_grace(
            grace,
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;

    // The detach is synchronous; the sleep lets the loop wake and arm,
    // and stays well inside the window.
    drop(first);
    tokio::time::sleep(grace / 2).await;

    let mut second = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub still registered inside the grace window");

    // Three periods later the original deadline has long passed. The
    // reattach must have cancelled it.
    tokio::time::sleep(grace * 3).await;
    assert!(
        registry.is_registered_for_test(SESSION_ID).await,
        "the armed teardown was cancelled by the reattach"
    );
    assert!(
        !backend.saw(&Invocation::Shutdown),
        "a cancelled teardown runs no shutdown"
    );

    second
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("hi")],
            attach_id: second.attach_id,
        })
        .await
        .expect("send Prompt");

    let seen = collect_until(&mut second.outbound, "prompt_done").await;
    assert!(seen
        .iter()
        .any(|e| e["role"] == "agent" && e["text"] == "still alive"));
    assert!(
        registry.is_registered_for_test(SESSION_ID).await,
        "the hub survived the grace window"
    );
    assert!(
        !backend.saw(&Invocation::Shutdown),
        "no shutdown ran while a browser was attached"
    );

    // A cancelled window does not disarm the timer for good: the next
    // detach to zero arms a fresh one that fires.
    drop(second);
    assert!(
        wait_until_unregistered(&registry, SESSION_ID).await,
        "the next detach to zero arms a fresh window that fires"
    );
    assert!(
        wait_for_invocation(&backend, &Invocation::Shutdown).await,
        "the fresh window's teardown runs the shutdown"
    );
}

#[tokio::test]
async fn grace_does_not_cancel_an_in_flight_turn() {
    // Requirement 7 criterion 5. Tearing down here would abort the user's
    // turn, which is what losing focus on a phone mid-turn used to do.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![
        agent_append("working"),
    ])));

    let mut attached = registry
        .register_for_test_with_grace(
            Duration::from_millis(50),
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("long running")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");

    // Sync point: the turn is open and has streamed its event. The user
    // echo goes out before the turn task is spawned and proves nothing
    // about the Backend; the agent frame is sent by the turn itself,
    // after its `prompt` invocation is recorded, so waiting for it holds
    // on any scheduler.
    let echo = next_event(&mut attached.outbound).await.expect("the echo");
    assert_eq!(echo["role"], "user");
    let streamed = next_event(&mut attached.outbound)
        .await
        .expect("the turn's event");
    assert_eq!(streamed["role"], "agent");
    assert_eq!(streamed["text"], "working");
    assert_eq!(backend.prompt_count(), 1);

    // Detach the only browser: the count falls to zero and the 50ms grace
    // timer arms. Several fires pass in the next 300ms.
    drop(attached);
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        !backend.saw(&Invocation::Cancel),
        "grace must not cancel a turn in flight"
    );
    assert!(
        !backend.saw(&Invocation::Shutdown),
        "grace must not shut the Backend down mid-turn"
    );
    assert!(
        registry.is_registered_for_test(SESSION_ID).await,
        "the hub stays registered while a turn is in flight"
    );
}

#[tokio::test]
async fn grace_still_tears_down_once_the_turn_completes() {
    // Requirement 7 criterion 6: the in-flight hold must not defeat
    // reclamation. Once the turn resolves and the last browser is gone,
    // the timer shuts the Backend down and drops the registry entry.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(vec![
        agent_append("quick"),
    ])));

    let mut attached = registry
        .register_for_test_with_grace(
            Duration::from_millis(50),
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("quick")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");
    let seen = collect_until(&mut attached.outbound, "prompt_done").await;
    assert!(seen.iter().any(|e| e["type"] == "prompt_done"));

    drop(attached);
    assert!(
        wait_until_unregistered(&registry, SESSION_ID).await,
        "an idle hub must be torn down once the turn completes"
    );
    // Polled, not read once: the registry entry and the shutdown call
    // are two steps of the teardown, and nothing here fixes their order.
    assert!(
        wait_for_invocation(&backend, &Invocation::Shutdown).await,
        "teardown invokes the Backend's shutdown"
    );
}

#[tokio::test]
async fn grace_reclaims_the_agent_once_the_inflight_hold_is_capped() {
    // Requirement 7 criterion 7. A turn that never resolves would
    // otherwise hold its hub and its Backend for the life of the process.
    // The cap is a multiple of the grace period: a 10ms period puts it at
    // 600ms here.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![])));

    let attached = registry
        .register_for_test_with_grace(
            Duration::from_millis(10),
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("never finishes")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while backend.prompt_count() == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(backend.prompt_count(), 1, "the turn is in flight");

    drop(attached);

    assert!(
        wait_until_unregistered(&registry, SESSION_ID).await,
        "a hold that outlives the cap must still release the session"
    );
    assert!(
        wait_for_invocation(&backend, &Invocation::Shutdown).await,
        "the capped teardown runs the cooperative shutdown"
    );
}

#[tokio::test(start_paused = true)]
async fn a_reattach_mid_hold_starts_the_next_hold_from_zero() {
    // Requirement 7 criterion 8. The cap is measured from the first grace
    // fire of the current hold, not from the first detach the hub ever
    // saw. A hub detached for 40 periods, reattached, and detached for 40
    // more has been detached 80 periods in total and is still inside the
    // cap; a hold that runs past 60 periods on its own is not. Under the
    // paused clock the arithmetic is exact.
    let grace = Duration::from_millis(50);
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![])));
    let first = registry
        .register_for_test_with_grace(
            grace,
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;
    first
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("never finishes")],
            attach_id: first.attach_id,
        })
        .await
        .expect("send Prompt");
    let mut polls = 0;
    while backend.prompt_count() == 0 && polls < 100 {
        tokio::time::sleep(Duration::from_millis(1)).await;
        polls += 1;
    }
    assert_eq!(backend.prompt_count(), 1, "the turn is in flight");

    drop(first);
    tokio::time::sleep(grace * 40).await;
    assert!(
        !backend.saw(&Invocation::Shutdown),
        "40 periods detached is inside the cap"
    );

    let again = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("the hub is still registered");
    tokio::time::sleep(grace).await;
    drop(again);
    tokio::time::sleep(grace * 40).await;
    assert!(
        !backend.saw(&Invocation::Shutdown),
        "the second hold is 40 periods old, whatever the first one was"
    );
    assert!(
        registry.is_registered_for_test(SESSION_ID).await,
        "the hub is still registered 80 periods of detachment later"
    );

    tokio::time::sleep(grace * 22).await;
    assert!(
        wait_for_invocation(&backend, &Invocation::Shutdown).await,
        "a hold that crosses 60 periods on its own releases the session"
    );
    assert!(
        wait_until_unregistered(&registry, SESSION_ID).await,
        "the capped teardown frees the registry entry"
    );
}

#[tokio::test]
async fn a_failing_turn_broadcasts_error_then_prompt_done_and_keeps_the_hub() {
    // Requirement 7 criterion 14. The `prompt_done` is what unlocks every
    // composer on the session, which is why it follows a failure as well
    // as a success, and the hub survives its Backend's bad turn.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::error(
        vec![agent_append("partial")],
        "the turn failed",
    )));

    let mut attached = registry
        .register_for_test_with_grace(
            Duration::from_millis(50),
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("go")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");

    let seen = collect_until(&mut attached.outbound, "prompt_done").await;
    let types: Vec<&str> = seen
        .iter()
        .map(|e| e["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        types,
        vec!["append", "append", "error", "prompt_done"],
        "every streamed event, then error, then prompt_done"
    );
    let error = seen
        .iter()
        .find(|e| e["type"] == "error")
        .expect("an error");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("the turn failed")),
        "the error carries the Backend's text, got {error}"
    );
    assert!(
        registry.is_registered_for_test(SESSION_ID).await,
        "a failing turn does not tear the hub down"
    );
}

#[tokio::test]
async fn a_panicking_turn_broadcasts_error_then_prompt_done() {
    // Requirement 3 criterion 8's panic clause. Without the catch the turn
    // task dies and every composer on the session stays locked.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::panicking(
        vec![agent_append("before the panic")],
        "the turn panicked on purpose",
    )));

    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("go")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");

    let seen = collect_until(&mut attached.outbound, "prompt_done").await;
    let error = seen
        .iter()
        .find(|e| e["type"] == "error")
        .expect("a panic surfaces as an error");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("panicked")),
        "the error names the panic, got {error}"
    );
    assert!(seen.iter().any(|e| e["type"] == "prompt_done"));
    assert!(registry.is_registered_for_test(SESSION_ID).await);
}

#[tokio::test]
async fn a_second_prompt_during_a_turn_is_discarded() {
    // Requirement 7 criterion 20. The client refuses to send while its
    // composer is locked, so this branch is reached by a raced frame or a
    // foreign client. The drop is silent: a notice to the sender alone
    // would need a `_target` on an `append`, which criterion 11 forbids.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![
        agent_append("first turn"),
    ])));

    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("one")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send the first Prompt");

    // Drain the echo and the turn's event, so the turn is provably open.
    let opening = collect_until(&mut attached.outbound, "append").await;
    assert_eq!(opening[0]["role"], "user");
    let streamed = next_event(&mut attached.outbound)
        .await
        .expect("the turn's event");
    assert_eq!(streamed["text"], "first turn");

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("two")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send the second Prompt");

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        attached.outbound.try_recv().is_err(),
        "the second prompt broadcasts nothing, not even its echo"
    );
    assert_eq!(
        backend.prompt_count(),
        1,
        "the second prompt never reaches the Backend"
    );

    // The first turn still finishes normally.
    backend.release_turn(Release::Ok);
    let tail = collect_until(&mut attached.outbound, "prompt_done").await;
    assert_eq!(
        tail.iter().filter(|e| e["type"] == "prompt_done").count(),
        1,
        "exactly one prompt_done, for the turn that ran"
    );
}

#[tokio::test]
async fn ready_busy_reflects_the_inflight_count() {
    // Requirement 7 criterion 21, read at the hub where the value is set,
    // and driven through `take_outbound`, the path the WS handler uses.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![])));

    let attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;
    assert_eq!(
        attached.snapshot_ready["busy"], false,
        "no turn in flight yet"
    );
    let commands = attached.commands.clone();
    let attach_id = attached.attach_id;
    let (mut outbound, _guard) = attached.take_outbound();

    commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("hi")],
            attach_id,
        })
        .await
        .expect("send Prompt");
    let echo = next_event(&mut outbound).await.expect("the echo");
    assert_eq!(echo["role"], "user");

    let mid_turn = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");
    assert_eq!(
        mid_turn.snapshot_ready["busy"], true,
        "an attach landing mid-turn reads busy"
    );

    backend.release_turn(Release::Ok);
    let tail = collect_until(&mut outbound, "prompt_done").await;
    assert!(tail.iter().any(|e| e["type"] == "prompt_done"));

    let after = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");
    assert_eq!(
        after.snapshot_ready["busy"], false,
        "an attach landing after the turn reads idle"
    );
}

#[tokio::test]
async fn concurrent_attaches_for_one_id_build_one_hub() {
    // Requirement 6 criterion 4. Two upgrades naming one id, the second
    // reaching `attach_or_create` before the first has registered, must
    // yield one hub and one Backend with both attaches subscribed.
    //
    // On this single-threaded runtime the first arrival would build,
    // register and subscribe in one poll, and the second would always
    // find the hub on the fast path. So the first arrival is parked inside
    // the build window, with the per-id gate held and nothing registered,
    // and the second arrives while it waits there. Neither attach
    // registers a Backend of its own: this is the production path, so the
    // hub builds an `EchoBackend`.
    let registry = HubRegistry::new();
    let id = "raced-session";

    let (parked_tx, parked_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let first = tokio::spawn({
        let registry = registry.clone();
        async move {
            registry
                .attach_or_create_parked_for_test(id, async move {
                    let _ = parked_tx.send(());
                    let _ = release_rx.await;
                })
                .await
        }
    });
    parked_rx
        .await
        .expect("the first arrival reaches the build window");
    assert!(
        !registry.is_registered_for_test(id).await,
        "the first arrival holds the gate with nothing registered yet"
    );

    let second = tokio::spawn({
        let registry = registry.clone();
        async move { registry.attach_or_create(id).await }
    });
    // Let the second arrival run until it blocks on the gate.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        !registry.is_registered_for_test(id).await,
        "the second arrival built nothing of its own: it waits at the gate"
    );
    assert!(
        !second.is_finished(),
        "the second arrival is parked at the gate, not served"
    );

    release_tx
        .send(())
        .expect("the first arrival is waiting to be released");
    let mut first = first
        .await
        .expect("the first task")
        .expect("the first attach");
    let mut second = second
        .await
        .expect("the second task")
        .expect("the second attach");

    assert_eq!(first.session_id, id);
    assert_eq!(second.session_id, id);
    assert_ne!(
        first.attach_id, second.attach_id,
        "two distinct attaches, not one"
    );
    assert!(registry.is_registered_for_test(id).await);

    // One prompt reaching both attaches proves one hub, and a transcript
    // holding exactly that one exchange proves one Backend.
    first
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("shared")],
            attach_id: first.attach_id,
        })
        .await
        .expect("send Prompt");

    let on_first = collect_until(&mut first.outbound, "prompt_done").await;
    let on_second = collect_until(&mut second.outbound, "prompt_done").await;
    assert_eq!(on_first, on_second, "both attaches see the same sequence");
    assert!(on_first
        .iter()
        .any(|e| e["role"] == "agent" && e["text"] == "shared"));

    let transcript = registry
        .history(id)
        .await
        .expect("the hub reports a transcript");
    assert_eq!(
        transcript.len(),
        2,
        "one user entry and one agent entry, from a single Backend"
    );
}

#[tokio::test]
async fn an_unscripted_turn_resolves_with_an_error_and_keeps_the_hub() {
    // Requirement 5 criterion 9, and the hub's handling of it: the
    // Backend streams nothing, the loop derives an `error` and a
    // `prompt_done`, and the session stays usable.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new());
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("hi")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");

    let seen = collect_until(&mut attached.outbound, "prompt_done").await;
    let types: Vec<&str> = seen
        .iter()
        .map(|e| e["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(types, vec!["append", "error", "prompt_done"]);
    assert!(matches!(
        backend.invocations().first(),
        Some(Invocation::Prompt(_))
    ));
    assert!(registry.is_registered_for_test(SESSION_ID).await);
}

#[tokio::test]
async fn a_scripted_pending_turn_resolves_on_release_and_not_on_a_timer() {
    // Requirement 5 criteria 5 and 6: the turn stays open until the test
    // releases it, and the invocation log is readable while it is
    // unresolved.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![])));
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("wait")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");
    let echo = next_event(&mut attached.outbound).await.expect("the echo");
    assert_eq!(echo["role"], "user");

    // Readable mid-turn, and the turn resolves on no timer of its own.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(backend.prompt_count(), 1);
    assert!(
        attached.outbound.try_recv().is_err(),
        "an unreleased turn produces no terminal frame"
    );

    backend.release_turn(Release::Err("released with an error".into()));
    let seen = collect_until(&mut attached.outbound, "prompt_done").await;
    let types: Vec<&str> = seen
        .iter()
        .map(|e| e["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(types, vec!["error", "prompt_done"]);
    assert!(matches!(
        ScriptedTurn::pending(vec![]).resolution,
        Resolution::Pending
    ));
}

#[tokio::test]
async fn an_answer_for_a_permission_never_raised_reaches_no_backend() {
    // The outstanding set is fed by the Backend's requests alone. An
    // answer for an id nothing raised is dropped: no Backend hears it and
    // the hub remembers nothing about it, which is what keeps one
    // attached peer from growing the hub's memory one id at a time.
    let registry = HubRegistry::new();
    let raised_id = json!("raised");
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![
        permission_request(raised_id.clone()),
    ])));
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    // With no turn open.
    attached
        .commands
        .send(HubCommand::PermissionResponse {
            id: json!("never-raised"),
            option_id: "allow".into(),
        })
        .await
        .expect("send an answer with no turn open");

    // With a turn open that raised a different id.
    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("go")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");
    let raised = collect_until(&mut attached.outbound, "permission_request").await;
    assert!(raised.iter().any(|e| e["type"] == "permission_request"));
    attached
        .commands
        .send(HubCommand::PermissionResponse {
            id: json!("other"),
            option_id: "allow".into(),
        })
        .await
        .expect("send an answer for an id never raised");
    // A number is not the string: the key is the id's JSON rendering.
    attached
        .commands
        .send(HubCommand::PermissionResponse {
            id: json!(0),
            option_id: "allow".into(),
        })
        .await
        .expect("send an answer with an id of another type");

    // The raised one still goes through, and it was queued behind the
    // three dropped ones, so its arrival proves those were processed.
    attached
        .commands
        .send(HubCommand::PermissionResponse {
            id: raised_id.clone(),
            option_id: "allow".into(),
        })
        .await
        .expect("send the real answer");
    let real = Invocation::PermissionResponse {
        id: raised_id,
        option_id: "allow".into(),
    };
    assert!(wait_for_invocation(&backend, &real).await);
    let stray: Vec<Invocation> = backend
        .invocations()
        .into_iter()
        .filter(|i| matches!(i, Invocation::PermissionResponse { .. }) && *i != real)
        .collect();
    assert!(
        stray.is_empty(),
        "no answer for an unraised id reached the Backend: {stray:?}"
    );

    backend.release_turn(Release::Ok);
    let tail = collect_until(&mut attached.outbound, "prompt_done").await;
    assert!(tail.iter().any(|e| e["type"] == "prompt_done"));
}

#[tokio::test]
async fn an_answer_arriving_after_its_turn_ended_is_dropped() {
    // The outstanding set is cleared when the turn ends. A card left on
    // screen by a turn that resolved without its answer has no turn to
    // answer into, and the session goes on taking prompts.
    let registry = HubRegistry::new();
    let id = json!("late");
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![
        permission_request(id.clone()),
    ])));
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("first")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");
    let raised = collect_until(&mut attached.outbound, "permission_request").await;
    assert!(raised.iter().any(|e| e["type"] == "permission_request"));

    // The turn resolves with the request unanswered.
    backend.release_turn(Release::Ok);
    let tail = collect_until(&mut attached.outbound, "prompt_done").await;
    assert!(tail.iter().any(|e| e["type"] == "prompt_done"));

    attached
        .commands
        .send(HubCommand::PermissionResponse {
            id: id.clone(),
            option_id: "allow".into(),
        })
        .await
        .expect("send the late answer");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !backend.saw(&Invocation::PermissionResponse {
            id,
            option_id: "allow".into(),
        }),
        "an answer for a turn that has ended reaches no Backend"
    );

    // The session is still usable.
    backend.push_turn(ScriptedTurn::success(vec![agent_append("still here")]));
    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("second")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send a second Prompt");
    let second = collect_until(&mut attached.outbound, "prompt_done").await;
    assert!(second
        .iter()
        .any(|e| e["role"] == "agent" && e["text"] == "still here"));
}

#[tokio::test]
async fn a_stalled_model_change_stalls_nothing_else_on_the_session() {
    // The regression this pins: `set_model` was awaited inline on the
    // owner loop, and the subscriber counter held its lock across a send
    // on a bounded channel only that loop drained. A Backend whose model
    // change stalled on I/O, plus one browser reconnecting a few times,
    // wedged the session id until restart. The loop now awaits nothing a
    // Backend implements, and attaching takes no lock and waits on
    // nothing.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new().set_model_pending());
    let attached = registry
        .register_for_test_with_grace(
            Duration::from_millis(50),
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;

    attached
        .commands
        .send(HubCommand::SetModel {
            model_id: "slow".into(),
        })
        .await
        .expect("send SetModel");
    assert!(
        wait_for_invocation(&backend, &Invocation::SetModel("slow".into())).await,
        "the change is under way and parked"
    );

    // One browser flapping while the change is parked: 32 attach and
    // detach pairs, well past the eight slots the old channel had.
    let flapped = timeout(Duration::from_secs(5), async {
        for _ in 0..32 {
            let peer = registry
                .attach_existing_for_test(SESSION_ID)
                .await
                .expect("hub registered");
            drop(peer);
        }
    })
    .await;
    assert!(flapped.is_ok(), "attaching never waits on the owner loop");

    // The inbox is live: a cancel reaches the Backend with the change
    // still parked.
    attached
        .commands
        .send(HubCommand::Cancel)
        .await
        .expect("send Cancel");
    assert!(
        wait_for_invocation(&backend, &Invocation::Cancel).await,
        "the loop is not held behind the model change"
    );

    // The grace timer is live too: detach the last browser and the hub is
    // reclaimed on its 50ms window, the change still parked.
    drop(attached);
    assert!(
        wait_until_unregistered(&registry, SESSION_ID).await,
        "grace teardown runs with the change still parked"
    );
    assert!(backend.saw(&Invocation::Shutdown));

    // Releasing the change afterwards is harmless: the loop is gone and
    // the reply has nowhere to go.
    backend.release_set_model(Ok(json!({ "models": {} })));
}

#[tokio::test]
async fn model_changes_run_one_at_a_time_and_the_latest_request_runs_next() {
    // A burst of selections runs one change at a time. The request that
    // was made last is the one that runs next; the ones between are
    // superseded and never reach the Backend.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new().set_model_pending());
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    for model_id in ["a", "b", "c"] {
        attached
            .commands
            .send(HubCommand::SetModel {
                model_id: model_id.into(),
            })
            .await
            .expect("send SetModel");
    }
    assert!(wait_for_invocation(&backend, &Invocation::SetModel("a".into())).await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let changes_started = |backend: &ScriptedBackend| {
        backend
            .invocations()
            .iter()
            .filter(|i| matches!(i, Invocation::SetModel(_)))
            .count()
    };
    assert_eq!(changes_started(&backend), 1, "one change runs at a time");

    backend.release_set_model(Ok(json!({ "models": { "currentModelId": "a" } })));
    let frame = next_event(&mut attached.outbound)
        .await
        .expect("the reply for a");
    assert_eq!(frame["type"], "session_info");
    assert_eq!(frame["info"]["models"]["currentModelId"], "a");

    assert!(
        wait_for_invocation(&backend, &Invocation::SetModel("c".into())).await,
        "the request made last runs next"
    );
    assert!(
        !backend.saw(&Invocation::SetModel("b".into())),
        "the superseded request never runs"
    );
    backend.release_set_model(Err("refused".into()));
    let notice = next_event(&mut attached.outbound)
        .await
        .expect("the notice for c");
    assert_eq!(notice["type"], "append");
    assert_eq!(notice["role"], "sys");
    assert!(notice["text"]
        .as_str()
        .is_some_and(|t| t.contains("refused")));

    // Nothing is left in flight or pending: a fresh request runs at once.
    attached
        .commands
        .send(HubCommand::SetModel {
            model_id: "d".into(),
        })
        .await
        .expect("send SetModel");
    assert!(wait_for_invocation(&backend, &Invocation::SetModel("d".into())).await);
    assert_eq!(changes_started(&backend), 3, "a, c and d ran; b did not");
    backend.release_set_model(Ok(json!({ "models": { "currentModelId": "d" } })));
    let frame = next_event(&mut attached.outbound)
        .await
        .expect("the reply for d");
    assert_eq!(frame["info"]["models"]["currentModelId"], "d");
}

#[tokio::test]
async fn a_prompt_past_the_text_ceiling_is_refused_to_its_sender_alone() {
    // One `error` stamped for the sender and nothing else: no echo, no
    // turn slot, no Backend call. The session takes the next prompt, and
    // a prompt exactly at the ceiling is accepted.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new());
    let mut sender = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;
    let mut peer = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");

    let too_long = "x".repeat(MAX_PROMPT_TEXT_BYTES + 1);
    sender
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block(&too_long)],
            attach_id: sender.attach_id,
        })
        .await
        .expect("send Prompt");

    let frame = next_event(&mut sender.outbound).await.expect("the refusal");
    assert_eq!(frame["type"], "error");
    assert_eq!(
        frame["_target"].as_u64(),
        Some(sender.attach_id),
        "stamped for the sender"
    );
    assert!(
        frame["message"]
            .as_str()
            .is_some_and(|m| m.contains(&MAX_PROMPT_TEXT_BYTES.to_string())),
        "the message names the ceiling: {}",
        frame["message"]
    );
    // The peer receives the same stamped frame on the broadcast; the
    // attach loop is what drops it there.
    assert_eq!(next_event(&mut peer.outbound).await.as_ref(), Some(&frame));

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        sender.outbound.try_recv().is_err(),
        "no echo and no prompt_done follow"
    );
    assert_eq!(backend.prompt_count(), 0, "no Backend call");
    let after = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub registered");
    assert_eq!(
        after.snapshot_ready["busy"], false,
        "no turn slot was claimed"
    );

    // Two text blocks whose join is one byte over: the join's newline
    // counts.
    let half = "y".repeat(MAX_PROMPT_TEXT_BYTES / 2);
    sender
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block(&half), text_block(&half)],
            attach_id: sender.attach_id,
        })
        .await
        .expect("send Prompt");
    let frame = next_event(&mut sender.outbound)
        .await
        .expect("the refusal of the join");
    assert_eq!(frame["type"], "error");
    assert_eq!(backend.prompt_count(), 0);

    // Exactly at the ceiling is accepted and runs.
    backend.push_turn(ScriptedTurn::success(vec![agent_append("ok")]));
    let at_limit = "z".repeat(MAX_PROMPT_TEXT_BYTES);
    sender
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block(&at_limit)],
            attach_id: sender.attach_id,
        })
        .await
        .expect("send Prompt");
    let seen = collect_until(&mut sender.outbound, "prompt_done").await;
    assert!(seen.iter().any(|e| e["type"] == "prompt_done"));
    assert_eq!(backend.prompt_count(), 1, "the prompt at the ceiling ran");
}

#[tokio::test(start_paused = true)]
async fn an_attach_and_detach_inside_one_wake_leave_the_grace_window_whole() {
    // Requirement 7 criteria 4 and 6. The loop reads the subscriber count
    // on every wake rather than trusting the wake, and cancels an armed
    // window by dropping it, never by treating the cancel as a fire. An
    // earlier shape resolved the deadline future on either the timer or
    // the cancel handle and could tear an idle hub down at once when an
    // attach and a detach landed in one wake. Both phases below coalesce
    // three counter changes into one wake: on this current-thread runtime
    // the only awaits between them are uncontended locks, which do not
    // yield to the loop.
    let grace = Duration::from_millis(50);

    // Phase 1: nothing armed yet when the coalesced wake lands.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new());
    let first = registry
        .register_for_test_with_grace(
            grace,
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;
    drop(first);
    let second = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("the hub is registered");
    drop(second);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        registry.is_registered_for_test(SESSION_ID).await,
        "the window is whole: no teardown before one grace period"
    );
    assert!(!backend.saw(&Invocation::Shutdown));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !registry.is_registered_for_test(SESSION_ID).await,
        "the window fires once, one period after the last detach"
    );
    assert!(wait_for_invocation(&backend, &Invocation::Shutdown).await);

    // Phase 2: the window is already armed when the coalesced wake lands.
    // The deadline must be neither pulled in nor pushed out.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new());
    let first = registry
        .register_for_test_with_grace(
            grace,
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;
    drop(first);
    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("the hub is registered inside the window");
    drop(second);
    tokio::time::sleep(Duration::from_millis(35)).await;
    assert!(
        registry.is_registered_for_test(SESSION_ID).await,
        "at 45ms the window armed at 0ms has not fired"
    );
    assert!(!backend.saw(&Invocation::Shutdown));
    // The loop's deadline at 50ms runs before this wake at 55ms. A loop
    // that re-armed on the coalesced wake would still be registered.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        !registry.is_registered_for_test(SESSION_ID).await,
        "the window fires at 50ms, unmoved by the coalesced wake"
    );
    assert!(wait_for_invocation(&backend, &Invocation::Shutdown).await);
}

#[tokio::test]
async fn a_cancel_with_no_turn_open_leaves_the_next_turn_untouched() {
    // The trait's obligation (src/backend.rs): a cancel with no turn open
    // is a no-op. The scripted Backend used to store a release anyway,
    // which the next `Pending` turn consumed at once, so a test could
    // pass for the wrong reason. Requirement 5 criterion 5.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![
        agent_append("working"),
    ])));
    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    attached
        .commands
        .send(HubCommand::Cancel)
        .await
        .expect("send Cancel");
    assert!(wait_for_invocation(&backend, &Invocation::Cancel).await);

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("after the cancel")],
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");
    let echo = next_event(&mut attached.outbound).await.expect("the echo");
    assert_eq!(echo["role"], "user");
    let streamed = next_event(&mut attached.outbound)
        .await
        .expect("the turn's event");
    assert_eq!(streamed["text"], "working");

    // The turn stays open: no error and no prompt_done arrive on their
    // own.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        attached.outbound.try_recv().is_err(),
        "a stale release ended the turn that the earlier cancel never aimed at"
    );
    assert_eq!(backend.prompt_count(), 1);

    backend.release_turn(Release::Ok);
    let rest = collect_until(&mut attached.outbound, "prompt_done").await;
    let types: Vec<&str> = rest.iter().filter_map(|e| e["type"].as_str()).collect();
    assert_eq!(
        types,
        vec!["prompt_done"],
        "the turn ends as released, with no error"
    );
}

#[tokio::test]
async fn a_shutdown_with_no_turn_open_leaves_the_next_turn_untouched() {
    // The trait's obligation: shutdown is idempotent and a shutdown with
    // no turn open leaves nothing behind for a later turn.
    let backend = ScriptedBackend::new();
    backend.shutdown().await;
    backend.shutdown().await;

    backend.push_turn(ScriptedTurn::pending(vec![]));
    let (events, _rx) = mpsc::unbounded_channel();
    let turn = backend.prompt(vec![text_block("later")], events);
    assert!(
        timeout(Duration::from_millis(150), turn).await.is_err(),
        "the earlier shutdowns left no release for this turn"
    );
    assert_eq!(
        backend.invocations(),
        vec![
            Invocation::Shutdown,
            Invocation::Shutdown,
            Invocation::Prompt(vec![text_block("later")]),
        ]
    );
}

#[tokio::test]
async fn a_new_session_is_refused_once_the_registry_holds_its_capacity() {
    // Requirement 7 criterion 15 as amended: the registry holds at most
    // its capacity of hubs. A new id past that is refused with
    // `RegistryFull`; a live session stays joinable, since joining adds
    // no hub. The production path builds `EchoBackend` hubs here.
    let registry = HubRegistry::with_capacity_for_test(2);
    let ids = [
        "00000000000000000000000000000001",
        "00000000000000000000000000000002",
        "00000000000000000000000000000003",
    ];
    let first = registry
        .attach_or_create(ids[0])
        .await
        .expect("the first session");
    let second = registry
        .attach_or_create(ids[1])
        .await
        .expect("the second session");

    let refused = match registry.attach_or_create(ids[2]).await {
        Ok(_) => panic!("a third session should have been refused"),
        Err(e) => e,
    };
    let full = refused
        .downcast_ref::<RegistryFull>()
        .expect("the refusal is a RegistryFull");
    assert_eq!(full.capacity, 2);
    assert!(
        !registry.is_registered_for_test(ids[2]).await,
        "a refused session builds nothing"
    );

    let rejoin = registry
        .attach_or_create(ids[0])
        .await
        .expect("a live session is always joinable");
    assert_ne!(rejoin.attach_id, first.attach_id);
    drop((first, second, rejoin));
}

#[tokio::test]
async fn a_slot_freed_by_teardown_admits_the_next_session() {
    // The cap counts live hubs, not ids ever seen: once a hub's grace
    // window ends and its entry goes, the next new session gets its slot.
    let registry = HubRegistry::with_capacity_for_test(1);
    let backend = Arc::new(ScriptedBackend::new());
    let held = registry
        .register_for_test_with_grace(
            Duration::from_millis(10),
            backend,
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;
    let new_id = "00000000000000000000000000000009";
    assert!(
        registry.attach_or_create(new_id).await.is_err(),
        "the one slot is held"
    );

    drop(held);
    assert!(wait_until_unregistered(&registry, SESSION_ID).await);
    let admitted = registry
        .attach_or_create(new_id)
        .await
        .expect("the freed slot admits the next session");
    assert_eq!(admitted.session_id, new_id);
}

#[tokio::test]
async fn an_attach_during_a_slow_shutdown_builds_a_fresh_hub_instead_of_joining_the_dead_one() {
    // Requirement 3 criterion 12 as amended: the loop frees its registry
    // slot before it runs the Backend's shutdown. Under the old order an
    // attach arriving during a slow shutdown found the dead hub, sent
    // into its closed inbox and was dropped; the browser then reconnected
    // into a fresh session. Now it builds the fresh hub at once.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new().shutdown_pending());
    let attached = registry
        .register_for_test_with_grace(
            Duration::from_millis(20),
            backend.clone(),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;

    drop(attached);
    assert!(
        wait_for_invocation(&backend, &Invocation::Shutdown).await,
        "the loop exited and its shutdown is parked"
    );
    assert!(
        !registry.is_registered_for_test(SESSION_ID).await,
        "the slot is freed before the shutdown runs"
    );

    let mut fresh = registry
        .attach_or_create(SESSION_ID)
        .await
        .expect("a fresh hub is built while the old shutdown is parked");
    fresh
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("alive")],
            attach_id: fresh.attach_id,
        })
        .await
        .expect("send Prompt");
    let seen = collect_until(&mut fresh.outbound, "prompt_done").await;
    assert!(
        seen.iter()
            .any(|e| e["role"] == "agent" && e["text"] == "alive"),
        "the fresh hub answers"
    );

    backend.release_shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        registry.is_registered_for_test(SESSION_ID).await,
        "the old hub's teardown removed nothing but its own entry"
    );
    assert_eq!(backend.count_of(&Invocation::Shutdown), 1);
    drop(fresh);
}

#[tokio::test]
async fn taking_the_outbound_receiver_leaves_no_second_one_behind() {
    // A broadcast slot is freed only once every receiver has read it, so a
    // receiver nobody reads pins every event in the ring until it is
    // lapped, the whole prompt text in each echo included. The handle hands
    // its one receiver to the attach loop and keeps none; an earlier shape
    // left a fresh `resubscribe` behind for the whole attach.
    let registry = HubRegistry::new();
    let attached = registry
        .register_for_test(
            Arc::new(ScriptedBackend::new()),
            SESSION_ID.into(),
            ready_event(),
            None,
        )
        .await;
    assert_eq!(
        registry.receiver_count_for_test(SESSION_ID).await,
        Some(1),
        "one attach, one receiver"
    );
    let (outbound, guard) = attached.take_outbound();
    assert_eq!(
        registry.receiver_count_for_test(SESSION_ID).await,
        Some(1),
        "handing the receiver to the attach loop adds none"
    );
    drop(outbound);
    assert_eq!(
        registry.receiver_count_for_test(SESSION_ID).await,
        Some(0),
        "with the loop's receiver gone nothing pins the ring"
    );
    drop(guard);
}
