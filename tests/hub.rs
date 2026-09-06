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

use mezame::hub::{HubCommand, HubRegistry};
use serde_json::{json, Value};
use support::{Invocation, Release, Resolution, ScriptedBackend, ScriptedTurn};
use tokio::sync::broadcast;
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
    // only the first reaches the Backend.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new());
    let mut attached_a = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
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

#[tokio::test]
async fn detach_to_zero_then_reattach_exercises_grace_counter() {
    // Requirement 7 criterion 4: the last detach arms the grace timer and
    // a fresh attach inside the window cancels the pending teardown.
    let registry = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(vec![
        agent_append("still alive"),
    ])));

    let first = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;

    // The `Drop` impl spawns the decrement, so give the runtime a moment
    // to run it and let the loop install its cancel handle.
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut second = registry
        .attach_existing_for_test(SESSION_ID)
        .await
        .expect("hub still registered inside the grace window");

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
        "a cancelled teardown runs no shutdown"
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

    // Sync point: the turn is open and has streamed its event.
    let streamed = collect_until(&mut attached.outbound, "append").await;
    assert!(!streamed.is_empty());
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
    assert!(
        backend.saw(&Invocation::Shutdown),
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

    let mut attached = registry
        .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
        .await;
    assert_eq!(
        attached.snapshot_ready["busy"], false,
        "no turn in flight yet"
    );
    let mut outbound = attached.take_outbound();

    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: vec![text_block("hi")],
            attach_id: attached.attach_id,
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
    // Neither attach registers a Backend of its own here: this is the
    // production path, so the hub builds an `EchoBackend`.
    let registry = HubRegistry::new();
    let id = "raced-session";

    let (first, second) =
        tokio::join!(registry.attach_or_create(id), registry.attach_or_create(id));
    let mut first = first.expect("the first attach");
    let mut second = second.expect("the second attach");

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
