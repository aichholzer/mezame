//! Property tests for the surviving transport and the shared derivations.
//!
//! The example-based cases elsewhere pin the frames the requirements name
//! by test function. These cover the invariants: the interleavings and the
//! input shapes a hand-written list misses. Every property runs a minimum
//! of 100 cases and is tagged with the design property it validates.
//!
//! The async properties build a current-thread runtime with a paused clock
//! inside the property body. A paused clock makes the grace schedule
//! deterministic and removes wall-clock sleeps from the turn interleavings,
//! which is what keeps 100 cases of each cheap.

mod support;

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message;
use mezame::backend::{
    extract_user_text, user_echo_event, user_text_len, Backend, EchoBackend, EntryBody,
    HistoryEntry, ToolCall, ToolCallStatus, ToolContent, ToolLocation,
};
use mezame::hub::{AttachedHub, HubCommand, HubRegistry};
use mezame::ws::{decide_session, is_session_id, new_session_id, run_attach_loop, SessionDecision};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use serde_json::{json, Map, Value};
use support::{Invocation, Release, ScriptedBackend, ScriptedTurn};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

const SESSION_ID: &str = "prop-session";

/// How long an await inside a property may take in simulated time. The
/// clock is paused, so this costs nothing unless the hub genuinely wedges.
const PATIENCE: Duration = Duration::from_secs(60);

// ---------- runtime and hub helpers ----------

fn paused_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a current-thread runtime")
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

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// Every frame of one turn, up to and including its `prompt_done`.
async fn frames_until_prompt_done(rx: &mut broadcast::Receiver<Arc<Value>>) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        match timeout(PATIENCE, rx.recv()).await {
            Ok(Ok(event)) => {
                let done = event["type"] == "prompt_done";
                seen.push((*event).clone());
                if done {
                    return seen;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return seen,
        }
    }
}

/// Run this attach's loop against a socket that never speaks, and return
/// the sink it writes to. The `AttachedHub` moves into the task, so the
/// subscriber count holds for as long as the loop runs.
fn spawn_attach_loop(mut attached: AttachedHub) -> (u64, mpsc::Receiver<Message>) {
    let attach_id = attached.attach_id;
    let outbound = attached.take_outbound();
    let commands = attached.commands.clone();
    // The same capacity as the hub's broadcast ring, so no property's
    // sink can be the smaller buffer: a full queue ends the attach, and
    // the properties are about what the loop forwards, not about that.
    let (to_ws_tx, to_ws_rx) = mpsc::channel::<Message>(1024);
    tokio::spawn(async move {
        let mut stream = Box::pin(futures_util::stream::pending::<Result<Message, Infallible>>());
        run_attach_loop(
            &mut stream,
            &to_ws_tx,
            outbound,
            commands,
            attach_id,
            // Far beyond anything a property awaits, so no heartbeat
            // frame ever lands in the sink.
            Duration::from_secs(3_600),
            Duration::from_secs(36_000),
        )
        .await;
        drop(attached);
    });
    (attach_id, to_ws_rx)
}

/// Drain a sink until its `prompt_done`, returning the JSON of every text
/// frame.
async fn sink_until_prompt_done(rx: &mut mpsc::Receiver<Message>) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let Ok(Some(frame)) = timeout(PATIENCE, rx.recv()).await else {
            return seen;
        };
        let Message::Text(text) = frame else { continue };
        let value: Value = serde_json::from_str(&text).expect("a sink frame is JSON");
        let done = value["type"] == "prompt_done";
        seen.push(value);
        if done {
            return seen;
        }
    }
}

// ---------- generators ----------

/// Arbitrary Unicode, deliberately including newlines, tabs, spaces and
/// the empty string.
fn arb_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            6 => any::<char>(),
            2 => Just('\n'),
            1 => Just(' '),
            1 => Just('\t'),
        ],
        0..16,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn arb_status() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("pending"),
        Just("in_progress"),
        Just("completed"),
        Just("failed"),
    ]
}

/// One optional field of a streamed `tool_call`, over the three states it
/// can be in on the wire: omitted, present holding JSON null, or present
/// holding a value.
fn arb_optional(inner: BoxedStrategy<Value>) -> impl Strategy<Value = Option<Value>> {
    prop_oneof![
        1 => Just(None),
        1 => Just(Some(Value::Null)),
        2 => inner.prop_map(Some),
    ]
}

/// A `tool_call` event over every combination of present and absent
/// optional fields.
fn arb_tool_call_event() -> impl Strategy<Value = Value> {
    (
        "[a-z0-9_-]{1,10}",
        arb_text(),
        arb_status(),
        arb_optional(("[a-z_]{1,8}").prop_map(Value::String).boxed()),
        prop_oneof![
            Just(json!({})),
            Just(json!({ "path": "/tmp/x" })),
            Just(json!([1, 2])),
            Just(Value::Null),
        ],
        arb_optional(
            prop::collection::vec(arb_text(), 0..3)
                .prop_map(|texts| {
                    Value::Array(
                        texts
                            .into_iter()
                            .map(|text| json!({ "type": "text", "text": text }))
                            .collect(),
                    )
                })
                .boxed(),
        ),
        arb_optional(
            prop::collection::vec(("[a-z/.]{1,10}", prop::option::of(0u32..500)), 0..3)
                .prop_map(|ls| {
                    Value::Array(
                        ls.into_iter()
                            .map(|(path, line)| match line {
                                Some(line) => json!({ "path": path, "line": line }),
                                None => json!({ "path": path }),
                            })
                            .collect(),
                    )
                })
                .boxed(),
        ),
    )
        .prop_map(|(id, title, status, kind, raw_input, content, locations)| {
            let mut map = Map::new();
            map.insert("type".into(), json!("tool_call"));
            map.insert("toolCallId".into(), json!(id));
            map.insert("title".into(), json!(title));
            map.insert("status".into(), json!(status));
            map.insert("rawInput".into(), raw_input);
            if let Some(kind) = kind {
                map.insert("kind".into(), kind);
            }
            if let Some(content) = content {
                map.insert("content".into(), content);
            }
            if let Some(locations) = locations {
                map.insert("locations".into(), locations);
            }
            Value::Object(map)
        })
}

/// Any event a Backend streams that the Hub does not stamp.
fn arb_untargeted_event() -> impl Strategy<Value = Value> {
    prop_oneof![
        3 => (prop_oneof![Just("agent"), Just("sys")], arb_text())
            .prop_map(|(role, text)| json!({ "type": "append", "role": role, "text": text })),
        2 => arb_text().prop_map(|text| json!({ "type": "thought", "text": text })),
        2 => arb_tool_call_event(),
    ]
}

fn arb_permission_request() -> impl Strategy<Value = Value> {
    ("[a-z0-9-]{1,8}", arb_text()).prop_map(|(id, title)| {
        json!({
            "type": "permission_request",
            "id": id,
            "title": title,
            "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" }]
        })
    })
}

/// A prompt block list drawn from `text`, `image` and `resource`.
fn arb_blocks() -> impl Strategy<Value = Vec<Value>> {
    prop::collection::vec(
        prop_oneof![
            4 => arb_text().prop_map(|text| json!({ "type": "text", "text": text })),
            1 => Just(json!({ "type": "image", "mimeType": "image/png", "data": "AAAA" })),
            1 => Just(json!({
                "type": "resource",
                "resource": { "uri": "file:///x", "mimeType": "text/plain", "text": "body" }
            })),
        ],
        0..32,
    )
}

fn arb_tool_call() -> impl Strategy<Value = ToolCall> {
    (
        "[a-z0-9_-]{1,10}",
        arb_text(),
        prop_oneof![
            Just(ToolCallStatus::Pending),
            Just(ToolCallStatus::InProgress),
            Just(ToolCallStatus::Completed),
            Just(ToolCallStatus::Failed),
        ],
        prop::option::of("[a-z_]{1,8}"),
        prop_oneof![
            Just(json!({})),
            Just(json!({ "path": "/tmp/x" })),
            Just(Value::Null),
        ],
        prop::option::of(prop::collection::vec(arb_text(), 0..3)),
        prop::option::of(prop::collection::vec(
            ("[a-z/.]{1,10}", prop::option::of(0u32..500)),
            0..3,
        )),
    )
        .prop_map(
            |(tool_call_id, title, status, kind, raw_input, content, locations)| ToolCall {
                tool_call_id,
                title,
                status,
                kind,
                raw_input,
                content: content.map(|texts| {
                    texts
                        .into_iter()
                        .map(|text| ToolContent::Text { text })
                        .collect()
                }),
                locations: locations.map(|ls| {
                    ls.into_iter()
                        .map(|(path, line)| ToolLocation { path, line })
                        .collect()
                }),
            },
        )
}

fn arb_history_entry() -> impl Strategy<Value = HistoryEntry> {
    let body = prop_oneof![
        arb_text().prop_map(|text| EntryBody::User { text }),
        arb_text().prop_map(|text| EntryBody::Agent { text }),
        arb_text().prop_map(|text| EntryBody::Sys { text }),
        arb_text().prop_map(|text| EntryBody::Thought { text }),
        arb_tool_call().prop_map(EntryBody::ToolCall),
    ];
    (body, 0i64..4_000_000_000_000).prop_map(|(body, timestamp)| HistoryEntry { body, timestamp })
}

/// Strings that stress the session id form: whitespace, path separators,
/// dot segments, non-ASCII, and the lengths on either side of the bound.
/// One lowercase hexadecimal character.
fn arb_hex_char() -> impl Strategy<Value = char> {
    prop_oneof![prop::char::range('0', '9'), prop::char::range('a', 'f')]
}

fn arb_loose_string() -> impl Strategy<Value = String> {
    prop_oneof![
        // Hex strings of the minted length and one either side of it,
        // some with one character lifted to upper case.
        3 => prop::collection::vec(arb_hex_char(), 30..=34)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
        1 => (prop::collection::vec(arb_hex_char(), 32), 0usize..32).prop_map(|(mut chars, at)| {
            chars[at] = chars[at].to_ascii_uppercase();
            chars.into_iter().collect::<String>()
        }),
        1 => Just(" 0123456789abcdef0123456789abcdef ".to_string()),
        6 => prop::collection::vec(
            prop_oneof![
                6 => prop::char::range('a', 'z'),
                3 => prop::char::range('0', '9'),
                1 => Just('-'),
                1 => Just('_'),
                1 => Just('/'),
                1 => Just('\\'),
                1 => Just('.'),
                1 => Just(' '),
                1 => Just('\t'),
                2 => any::<char>(),
            ],
            0..14,
        ).prop_map(|chars| chars.into_iter().collect::<String>()),
        1 => Just("a".repeat(128)),
        1 => Just("a".repeat(129)),
        1 => Just("..".to_string()),
        1 => Just("../x".to_string()),
        1 => Just("a/../b".to_string()),
        1 => Just(String::new()),
        1 => Just("   ".to_string()),
        1 => Just(" abc-1 ".to_string()),
    ]
}

// ---------- the properties ----------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // Feature: harness-strip-and-seam, Property 1: For any sequence of
    // untargeted Server_Events a Backend streams during one turn, and for
    // any number of attaches on that hub, every attach receives that
    // sequence in the streamed order, with every field value unchanged and
    // no field added.
    //
    // Validates: Requirements 3.3, 7.1, 7.11, 8.6
    #[test]
    fn property_1_untargeted_broadcast_fidelity(
        events in prop::collection::vec(arb_untargeted_event(), 0..200),
        attaches in 1usize..=8,
    ) {
        let rt = paused_runtime();
        rt.block_on(async move {
            let registry = HubRegistry::new();
            let backend = Arc::new(ScriptedBackend::with_turn(
                ScriptedTurn::success(events.clone()),
            ));
            let mut first = registry
                .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
                .await;
            let mut peers = Vec::new();
            for _ in 1..attaches {
                peers.push(
                    registry
                        .attach_existing_for_test(SESSION_ID)
                        .await
                        .expect("hub registered"),
                );
            }

            first
                .commands
                .send(HubCommand::Prompt {
                    blocks: vec![text_block("go")],
                    attach_id: first.attach_id,
                })
                .await
                .expect("send Prompt");

            let expected: Vec<Value> = events.clone();
            let mut received = vec![frames_until_prompt_done(&mut first.outbound).await];
            for peer in peers.iter_mut() {
                received.push(frames_until_prompt_done(&mut peer.outbound).await);
            }

            for frames in &received {
                // The echo opens the turn and `prompt_done` closes it;
                // everything between is the Backend's own sequence.
                prop_assert!(frames.len() >= 2, "a turn has at least two frames");
                prop_assert_eq!(frames[0]["type"].as_str(), Some("append"));
                prop_assert_eq!(frames[0]["role"].as_str(), Some("user"));
                prop_assert_eq!(
                    frames[frames.len() - 1]["type"].as_str(),
                    Some("prompt_done")
                );
                let streamed = &frames[1..frames.len() - 1];
                prop_assert_eq!(
                    streamed.len(),
                    expected.len(),
                    "every streamed event is broadcast, none added"
                );
                for (got, want) in streamed.iter().zip(expected.iter()) {
                    prop_assert_eq!(got, want, "fields unchanged and no field added");
                }
            }
            // Every attach saw the same thing.
            for frames in &received {
                prop_assert_eq!(frames, &received[0]);
            }
            Ok::<(), TestCaseError>(())
        })?;
    }

    // Feature: harness-strip-and-seam, Property 2: For any number of
    // attaches on a hub and for any attach chosen as the prompter, a
    // permission_request streamed during that prompter's turn reaches the
    // prompter's sink stamped with the prompter's attach_id, and reaches no
    // other attach.
    //
    // Validates: Requirements 7.9, 7.10, 8.5, 8.16
    #[test]
    fn property_2_targeted_delivery_is_exact(
        attaches in 2usize..=8,
        prompter_index in 0usize..8,
        untargeted in prop::collection::vec(arb_untargeted_event(), 0..12),
        card in arb_permission_request(),
        card_position in 0usize..13,
    ) {
        let prompter_index = prompter_index % attaches;
        let card_position = card_position.min(untargeted.len());
        let rt = paused_runtime();
        rt.block_on(async move {
            let mut events = untargeted.clone();
            events.insert(card_position, card.clone());

            let registry = HubRegistry::new();
            let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::success(events)));
            let first = registry
                .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
                .await;

            // Every attach runs its own loop, so the `_target` filter is
            // exercised where production applies it.
            let mut sinks = Vec::new();
            let mut senders = Vec::new();
            senders.push(first.commands.clone());
            sinks.push(spawn_attach_loop(first));
            for _ in 1..attaches {
                let peer = registry
                    .attach_existing_for_test(SESSION_ID)
                    .await
                    .expect("hub registered");
                senders.push(peer.commands.clone());
                sinks.push(spawn_attach_loop(peer));
            }

            let prompter_id = sinks[prompter_index].0;
            senders[prompter_index]
                .send(HubCommand::Prompt {
                    blocks: vec![text_block("go")],
                    attach_id: prompter_id,
                })
                .await
                .expect("send Prompt");

            for (index, (attach_id, rx)) in sinks.iter_mut().enumerate() {
                let frames = sink_until_prompt_done(rx).await;
                let cards: Vec<&Value> = frames
                    .iter()
                    .filter(|f| f["type"] == "permission_request")
                    .collect();
                if index == prompter_index {
                    prop_assert_eq!(cards.len(), 1, "the prompter sees the card once");
                    prop_assert_eq!(
                        cards[0]["_target"].as_u64(),
                        Some(*attach_id),
                        "stamped with the prompter's own attach id"
                    );
                    prop_assert_eq!(&cards[0]["id"], &card["id"]);
                    prop_assert_eq!(&cards[0]["title"], &card["title"]);
                } else {
                    prop_assert!(
                        cards.is_empty(),
                        "a peer never receives a card it was not asked to answer"
                    );
                }
                // Untargeted events reach everyone, stamped with nothing.
                for frame in &frames {
                    if frame["type"] != "permission_request" {
                        prop_assert!(
                            frame.get("_target").is_none(),
                            "only a permission request is stamped"
                        );
                    }
                }
                prop_assert_eq!(
                    frames.iter().filter(|f| f["type"] == "append" && f["role"] == "user").count(),
                    1,
                    "the echo reaches every attach, the sender included"
                );
            }
            Ok::<(), TestCaseError>(())
        })?;
    }

    // Feature: harness-strip-and-seam, Property 3: For any event vector
    // and for any terminal outcome, the sequence an attach receives for one
    // turn is the user echo, then the streamed events in order, then one
    // error frame when the outcome is an error or a panic, then exactly one
    // prompt_done, with no frame for that turn after it.
    //
    // Validates: Requirements 3.8, 3.9, 7.14, 7.17
    #[test]
    fn property_3_turn_ordering_and_terminal_frames(
        events in prop::collection::vec(arb_untargeted_event(), 0..200),
        outcome in 0usize..3,
        message in "[a-z ]{1,20}",
    ) {
        let rt = paused_runtime();
        rt.block_on(async move {
            let turn = match outcome {
                0 => ScriptedTurn::success(events.clone()),
                1 => ScriptedTurn::error(events.clone(), message.clone()),
                _ => ScriptedTurn::panicking(events.clone(), message.clone()),
            };
            let expects_error = outcome != 0;

            let registry = HubRegistry::new();
            let backend = Arc::new(ScriptedBackend::with_turn(turn));
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

            let frames = frames_until_prompt_done(&mut attached.outbound).await;
            let types: Vec<&str> = frames
                .iter()
                .map(|f| f["type"].as_str().unwrap_or(""))
                .collect();

            prop_assert_eq!(types.first(), Some(&"append"));
            prop_assert_eq!(frames[0]["role"].as_str(), Some("user"));
            prop_assert_eq!(types.last(), Some(&"prompt_done"));
            prop_assert_eq!(
                types.iter().filter(|t| **t == "prompt_done").count(),
                1,
                "exactly one prompt_done"
            );

            let tail = if expects_error { 2 } else { 1 };
            let streamed = &frames[1..frames.len() - tail];
            prop_assert_eq!(streamed.len(), events.len(), "every event, in order");
            for (got, want) in streamed.iter().zip(events.iter()) {
                prop_assert_eq!(got, want);
            }

            let errors: Vec<&Value> = frames.iter().filter(|f| f["type"] == "error").collect();
            if expects_error {
                prop_assert_eq!(errors.len(), 1, "one error frame");
                prop_assert_eq!(
                    frames[frames.len() - 2]["type"].as_str(),
                    Some("error"),
                    "the error sits immediately before prompt_done"
                );
                let text = errors[0]["message"].as_str().unwrap_or("");
                prop_assert!(
                    text.contains(&message),
                    "the error carries the outcome's text: {:?}",
                    text
                );
            } else {
                prop_assert!(errors.is_empty(), "a successful turn raises no error");
            }

            // Nothing further arrives for this turn.
            prop_assert!(
                attached.outbound.try_recv().is_err(),
                "no frame follows the prompt_done"
            );
            Ok::<(), TestCaseError>(())
        })?;
    }

    // Feature: harness-strip-and-seam, Property 4: For any terminal outcome
    // and for any point during a turn at which a second prompt arrives, the
    // in-flight count is above zero from before the user echo until the turn
    // is released, falls to zero before the prompt_done broadcast, and the
    // second prompt produces no broadcast, invokes no Backend method, and
    // leaves the count unchanged.
    //
    // Validates: Requirements 3.5, 3.7, 3.25, 7.20, 10.16
    #[test]
    fn property_4_the_inflight_count_returns_to_zero_once(
        events in prop::collection::vec(arb_untargeted_event(), 0..24),
        consume in 0usize..25,
        ending in 0usize..4,
        message in "[a-z ]{1,20}",
    ) {
        let rt = paused_runtime();
        rt.block_on(async move {
            let consume = consume.min(events.len());
            let registry = HubRegistry::new();
            // `Pending` is the only turn shape this property can use: a
            // turn that resolves in one poll cannot host a mid-turn
            // arrival.
            let backend = Arc::new(ScriptedBackend::with_turn(
                ScriptedTurn::pending(events.clone()),
            ));
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

            // The echo, then the generated number of streamed events.
            let echo = timeout(PATIENCE, attached.outbound.recv())
                .await
                .expect("the echo arrives")
                .expect("the channel is open");
            prop_assert_eq!(echo["role"].as_str(), Some("user"));
            for expected in events.iter().take(consume) {
                let event = timeout(PATIENCE, attached.outbound.recv())
                    .await
                    .expect("a streamed event arrives")
                    .expect("the channel is open");
                prop_assert_eq!(&*event, expected);
            }

            // The count is above zero right now: an attach landing here
            // reads busy.
            let mid_turn = registry
                .attach_existing_for_test(SESSION_ID)
                .await
                .expect("hub registered");
            prop_assert_eq!(
                &mid_turn.snapshot_ready["busy"],
                &Value::Bool(true),
                "the count is above zero for the whole turn"
            );

            attached
                .commands
                .send(HubCommand::Prompt {
                    blocks: vec![text_block("two")],
                    attach_id: attached.attach_id,
                })
                .await
                .expect("send the second Prompt");
            // Let the loop process it, then prove it did nothing.
            tokio::time::sleep(Duration::from_millis(50)).await;
            prop_assert_eq!(
                backend.prompt_count(),
                1,
                "the second prompt invokes no Backend method"
            );
            let after_second = registry
                .attach_existing_for_test(SESSION_ID)
                .await
                .expect("hub registered");
            prop_assert_eq!(
                &after_second.snapshot_ready["busy"],
                &Value::Bool(true),
                "the count is unchanged by the discarded prompt"
            );

            let expects_error = match ending {
                0 => {
                    backend.release_turn(Release::Ok);
                    false
                }
                1 => {
                    backend.release_turn(Release::Err(message.clone()));
                    true
                }
                2 => {
                    backend.release_turn(Release::Panic(message.clone()));
                    true
                }
                // The cancel arm sends no release afterwards, so no stale
                // permit is left for a later turn. The scripted Backend
                // resolves a cancelled turn with an error.
                _ => {
                    attached
                        .commands
                        .send(HubCommand::Cancel)
                        .await
                        .expect("send Cancel");
                    true
                }
            };

            let rest = frames_until_prompt_done(&mut attached.outbound).await;
            let remaining_events = &events[consume..];
            let tail = if expects_error { 2 } else { 1 };
            prop_assert!(rest.len() >= tail);
            let streamed = &rest[..rest.len() - tail];
            prop_assert_eq!(
                streamed.len(),
                remaining_events.len(),
                "the rest of the turn's events still arrive"
            );
            prop_assert_eq!(
                rest.iter().filter(|f| f["type"] == "prompt_done").count(),
                1,
                "exactly one prompt_done, for the turn that ran"
            );
            prop_assert_eq!(
                rest.iter().filter(|f| f["type"] == "error").count(),
                usize::from(expects_error),
                "one error on a failed, panicking or cancelled turn"
            );
            prop_assert!(
                !rest.iter().any(|f| f["type"] == "append" && f["role"] == "user"),
                "the discarded prompt never broadcast an echo"
            );

            // And the count is back to zero, once.
            let after = registry
                .attach_existing_for_test(SESSION_ID)
                .await
                .expect("hub registered");
            prop_assert_eq!(
                &after.snapshot_ready["busy"],
                &Value::Bool(false),
                "the count falls to zero before the prompt_done"
            );
            if ending == 3 {
                prop_assert!(
                    backend.saw(&Invocation::Cancel),
                    "the cancel reached the Backend"
                );
            }
            Ok::<(), TestCaseError>(())
        })?;
    }

    // Feature: harness-strip-and-seam, Property 5: For any schedule of
    // attach and detach events against a hub holding an unresolved turn,
    // the Backend's recorded invocation log holds no shutdown until either
    // the turn resolves or the detached hold passes 60 grace periods
    // measured from the first grace fire of that hold. A reattach ends
    // the hold, and the next detach starts a new one from zero.
    //
    // Validates: Requirements 3.11, 7.5, 7.6, 7.7, 7.8
    #[test]
    fn property_5_shutdown_never_runs_while_a_turn_is_in_flight(
        schedule in prop::collection::vec((any::<bool>(), 1u64..=1_250), 1..20),
    ) {
        let rt = paused_runtime();
        rt.block_on(async move {
            // A 50ms grace puts the cap at 3s: teardown runs on the 61st
            // grace fire of a hold, 3,050ms after the detach that began
            // it. Gaps run to 1,250ms, so a schedule can sit detached
            // past the cap, or reattach 2,400ms into a hold and detach
            // again for another 2,400ms, which only a hold measured from
            // its own start survives. Under the paused clock every gap
            // and every fire is exact.
            let grace = Duration::from_millis(50);
            let cap_crossed_at = grace * 61;
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
            let commands = first.commands.clone();
            let mut held: Vec<AttachedHub> = vec![first];
            // When the current detached hold began; `None` while attached.
            let mut detached_since: Option<tokio::time::Instant> = None;

            for (attach, gap_ms) in schedule {
                if attach {
                    if let Some(more) = registry.attach_existing_for_test(SESSION_ID).await {
                        held.push(more);
                        detached_since = None;
                    }
                } else {
                    held.pop();
                }
                if held.is_empty() && detached_since.is_none() {
                    detached_since = Some(tokio::time::Instant::now());
                }
                tokio::time::sleep(Duration::from_millis(gap_ms)).await;

                let detached_for = detached_since.map(|since| since.elapsed());
                if detached_for.is_some_and(|d| d >= cap_crossed_at) {
                    // This hold outlived the cap on its own: teardown is
                    // due, and the rest of the schedule has no hub to run
                    // against.
                    let mut polls = 0;
                    while !backend.saw(&Invocation::Shutdown) && polls < 100 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        polls += 1;
                    }
                    prop_assert!(
                        backend.saw(&Invocation::Shutdown),
                        "a hold past 60 grace periods releases the session (detached {:?})",
                        detached_for
                    );
                    prop_assert!(
                        !registry.is_registered_for_test(SESSION_ID).await,
                        "the capped teardown frees the registry entry"
                    );
                    return Ok::<(), TestCaseError>(());
                }
                prop_assert!(
                    !backend.saw(&Invocation::Shutdown),
                    "no shutdown while the turn is in flight and the hold is inside the \
                     cap (detached for {:?})",
                    detached_for
                );
            }
            prop_assert!(
                registry.is_registered_for_test(SESSION_ID).await,
                "the hub keeps its registry entry"
            );

            // The other half of the criterion: once the turn resolves and
            // nothing is attached, the grace timer does tear the hub down.
            backend.release_turn(Release::Ok);
            drop(held);
            drop(commands);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline
                && registry.is_registered_for_test(SESSION_ID).await
            {
                tokio::time::sleep(grace).await;
            }
            prop_assert!(
                !registry.is_registered_for_test(SESSION_ID).await,
                "an idle, detached hub is reclaimed"
            );
            prop_assert!(
                backend.saw(&Invocation::Shutdown),
                "teardown invokes the Backend's shutdown"
            );
            Ok::<(), TestCaseError>(())
        })?;
    }

    // Feature: harness-strip-and-seam, Property 6: For any count n up to
    // 10,000, minting n ids yields n distinct strings, each exactly 32
    // lowercase hexadecimal characters, and that form is the only one
    // accepted. For any string s, decide_session(Some(s)) is Mint
    // when s trims to empty, Accept of the trimmed value when that value
    // passes is_session_id, and Refuse in every other case;
    // decide_session(None) is Mint.
    //
    // Validates: Requirements 6.1, 6.2, 6.6, 6.7, 6.8
    #[test]
    fn property_6_session_ids_match_the_form_and_never_collide(
        n in 1usize..=10_000,
        s in arb_loose_string(),
    ) {
        let mut seen = HashSet::with_capacity(n);
        for _ in 0..n {
            let id = new_session_id();
            prop_assert!(is_session_id(&id), "a minted id is accepted: {:?}", id);
            prop_assert_eq!(id.len(), 32, "32 characters");
            prop_assert!(
                id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "lowercase hex only: {:?}",
                id
            );
            prop_assert!(seen.insert(id.clone()), "no id repeats: {:?}", id);
        }
        prop_assert_eq!(seen.len(), n);

        prop_assert_eq!(decide_session(None), SessionDecision::Mint);
        let trimmed = s.trim();
        match decide_session(Some(&s)) {
            SessionDecision::Mint => {
                prop_assert!(trimmed.is_empty(), "only an empty trim mints: {:?}", s);
            }
            SessionDecision::Accept(accepted) => {
                prop_assert_eq!(accepted.as_str(), trimmed, "the trimmed value is accepted");
                prop_assert!(is_session_id(&accepted), "an accepted id passes the form");
                prop_assert_eq!(accepted.len(), 32, "the minted length: {:?}", accepted);
                prop_assert!(
                    accepted
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "lowercase hex only, so no separator, dot segment, whitespace or \
                     upper case survives: {:?}",
                    accepted
                );
            }
            SessionDecision::Refuse => {
                prop_assert!(!trimmed.is_empty(), "an empty trim is never refused");
                prop_assert!(
                    !is_session_id(trimmed),
                    "only a value outside the form is refused: {:?}",
                    trimmed
                );
            }
        }
    }

    // Feature: harness-strip-and-seam, Property 7: For any prompt block
    // list, the echo text equals the text blocks joined by exactly one
    // newline, prefixed once with `> `, followed by exactly one newline;
    // the transcript's user entry text equals that same join with no prefix
    // and no trailing newline; and loadHistory's render of a user entry,
    // the formula `> ${text}\n`, applied to the stored entry reproduces the
    // echo text byte for byte.
    //
    // Validates: Requirements 4.2, 4.4, 12.3, 12.11, 12.12, 13.6
    #[test]
    fn property_7_the_echo_the_transcript_and_the_render_agree(blocks in arb_blocks()) {
        let rt = paused_runtime();
        rt.block_on(async move {
            // The join, computed here from the blocks alone.
            let join: String = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<&str>>()
                .join("\n");
            let has_text = blocks
                .iter()
                .any(|b| b["type"] == "text" && b["text"].is_string());

            prop_assert_eq!(
                extract_user_text(&blocks),
                if has_text { Some(join.clone()) } else { None },
                "the derivation takes text blocks only"
            );
            prop_assert_eq!(
                user_text_len(&blocks),
                extract_user_text(&blocks).map_or(0, |t| t.len()),
                "the hub's ceiling check measures the derived text without building it"
            );

            let echo = user_echo_event(&blocks);
            prop_assert_eq!(echo["type"].as_str(), Some("append"));
            prop_assert_eq!(echo["role"].as_str(), Some("user"));
            let echo_text = echo["text"].as_str().expect("echo text").to_string();
            prop_assert_eq!(&echo_text, &format!("> {join}\n"));

            let backend = EchoBackend::new();
            let (events_tx, _events_rx) = mpsc::unbounded_channel::<Value>();
            backend
                .prompt(blocks.clone(), events_tx)
                .await
                .expect("an echo turn resolves with success");
            let transcript = backend.history().await;
            prop_assert_eq!(transcript.len(), 2, "one user entry and one agent entry");
            let stored = match &transcript[0].body {
                EntryBody::User { text } => text.clone(),
                other => return Err(TestCaseError::fail(format!("expected a user entry, got {other:?}"))),
            };
            prop_assert_eq!(&stored, &join, "the entry holds the join, bare");
            prop_assert!(!stored.starts_with("> ") || join.starts_with("> "));
            prop_assert!(
                !stored.ends_with('\n') || join.ends_with('\n'),
                "no trailing newline is added"
            );

            // The browser's render of that entry, byte for byte.
            prop_assert_eq!(format!("> {stored}\n"), echo_text);
            Ok::<(), TestCaseError>(())
        })?;
    }

    // Feature: harness-strip-and-seam, Property 8: For any transcript,
    // every serialised entry declares exactly the keys its role admits,
    // kind, content and locations are present holding JSON null when
    // absent, a location omits line when absent, and the wire form of a
    // tool call, built through ToolCall::wire, differs from its history
    // form only by the discriminant key and the timestamp.
    //
    // Validates: Requirements 9.12, 13.6, 13.7
    #[test]
    fn property_8_every_entry_serialises_to_the_closed_shape(
        transcript in prop::collection::vec(arb_history_entry(), 0..64),
    ) {
        let serialised = serde_json::to_value(&transcript).expect("a transcript serialises");
        let entries = serialised.as_array().expect("an array");
        prop_assert_eq!(entries.len(), transcript.len());

        for (entry, source) in entries.iter().zip(transcript.iter()) {
            let map = entry.as_object().expect("an object");
            let role = map["role"].as_str().expect("a role");
            let keys: HashSet<&str> = map.keys().map(String::as_str).collect();

            match role {
                "user" | "agent" | "sys" | "thought" => {
                    prop_assert_eq!(
                        keys,
                        HashSet::from(["role", "text", "timestamp"]),
                        "a text entry declares exactly three keys"
                    );
                    prop_assert!(map["text"].is_string());
                }
                "tool_call" => {
                    prop_assert_eq!(
                        keys,
                        HashSet::from([
                            "role", "toolCallId", "title", "status", "kind", "rawInput",
                            "content", "locations", "timestamp",
                        ]),
                        "a tool-call entry declares exactly nine keys"
                    );
                    prop_assert!(map["toolCallId"].is_string());
                    prop_assert!(map["title"].is_string());
                    prop_assert!(
                        ["pending", "in_progress", "completed", "failed"]
                            .contains(&map["status"].as_str().unwrap_or("")),
                        "status holds one of the four values"
                    );

                    let EntryBody::ToolCall(call) = &source.body else {
                        return Err(TestCaseError::fail("role and body disagree"));
                    };
                    // Present holding JSON null when absent, never omitted.
                    prop_assert_eq!(map["kind"].is_null(), call.kind.is_none());
                    prop_assert_eq!(map["content"].is_null(), call.content.is_none());
                    prop_assert_eq!(map["locations"].is_null(), call.locations.is_none());

                    if let Some(locations) = map["locations"].as_array() {
                        let sources = call.locations.as_ref().expect("locations");
                        for (got, want) in locations.iter().zip(sources.iter()) {
                            let got = got.as_object().expect("a location object");
                            let got_keys: HashSet<&str> =
                                got.keys().map(String::as_str).collect();
                            let expected: HashSet<&str> = if want.line.is_some() {
                                HashSet::from(["path", "line"])
                            } else {
                                // The one optional field in the contract,
                                // and so the one that is omitted rather
                                // than nulled.
                                HashSet::from(["path"])
                            };
                            prop_assert_eq!(got_keys, expected);
                        }
                    }

                    // The wire form: same payload, `type` in place of
                    // `role`, and no timestamp.
                    let wire = serde_json::to_value(call.wire()).expect("the wire form");
                    let wire_map = wire.as_object().expect("an object");
                    prop_assert_eq!(wire_map["type"].as_str(), Some("tool_call"));
                    let mut expected: HashMap<&str, &Value> = HashMap::new();
                    for (key, value) in map.iter() {
                        if key != "role" && key != "timestamp" {
                            expected.insert(key.as_str(), value);
                        }
                    }
                    let mut got: HashMap<&str, &Value> = HashMap::new();
                    for (key, value) in wire_map.iter() {
                        if key != "type" {
                            got.insert(key.as_str(), value);
                        }
                    }
                    prop_assert_eq!(
                        got,
                        expected,
                        "the two forms differ only by the discriminant and the timestamp"
                    );
                }
                other => {
                    return Err(TestCaseError::fail(format!("unexpected role {other:?}")));
                }
            }
            prop_assert_eq!(map["timestamp"].as_i64(), Some(source.timestamp));
        }
    }

    // Feature: harness-strip-and-seam, Property 9: For any interleaving of
    // a new attach with the release of an open turn, that attach's ready
    // reports busy as true only when it also receives that turn's
    // prompt_done on the receiver take_outbound hands over, and reports
    // busy as false only when the turn had already been released.
    //
    // Validates: Requirements 7.21, 10.7, 10.8, 10.17, 10.18
    #[test]
    fn property_9_ready_busy_and_prompt_done_agree(
        arm in 0usize..3,
        release_ok in any::<bool>(),
        message in "[a-z ]{1,12}",
    ) {
        let rt = paused_runtime();
        rt.block_on(async move {
            let registry = HubRegistry::new();
            let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![])));
            let mut opener = registry
                .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
                .await;
            let mut opener_rx = opener.take_outbound();

            opener
                .commands
                .send(HubCommand::Prompt {
                    blocks: vec![text_block("go")],
                    attach_id: opener.attach_id,
                })
                .await
                .expect("send Prompt");
            let echo = timeout(PATIENCE, opener_rx.recv())
                .await
                .expect("the echo arrives")
                .expect("the channel is open");
            prop_assert_eq!(echo["role"].as_str(), Some("user"));

            let release = if release_ok {
                Release::Ok
            } else {
                Release::Err(message.clone())
            };

            // arm 0: attach while the turn is open.
            // arm 1: release, then attach with no yield in between.
            // arm 2: release, wait for the prompt_done, then attach.
            let mut released = false;
            if arm >= 1 {
                backend.release_turn(release.clone());
                released = true;
            }
            if arm == 2 {
                let frames = frames_until_prompt_done(&mut opener_rx).await;
                prop_assert!(
                    frames.iter().any(|f| f["type"] == "prompt_done"),
                    "the opener sees the turn end"
                );
            }

            let mut joiner = registry
                .attach_existing_for_test(SESSION_ID)
                .await
                .expect("hub registered");
            let busy = joiner.snapshot_ready["busy"] == Value::Bool(true);
            let mut joiner_rx = joiner.take_outbound();

            if arm == 0 {
                prop_assert!(busy, "an attach during an open turn reads busy");
                backend.release_turn(release);
                released = true;
            }
            prop_assert!(released, "the turn is released by now");

            if busy {
                // The pairing: the receiver was taken before the count was
                // read, and the loop decrements before it broadcasts, so
                // this attach must see the terminal frame.
                let frames = frames_until_prompt_done(&mut joiner_rx).await;
                prop_assert!(
                    frames.iter().any(|f| f["type"] == "prompt_done"),
                    "an attach that read busy receives that turn's prompt_done"
                );
            } else {
                prop_assert_eq!(
                    arm, 2,
                    "busy is false only once the turn had already been released"
                );
            }
            Ok::<(), TestCaseError>(())
        })?;
    }
}
