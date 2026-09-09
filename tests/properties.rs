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
use mezame::conversation::{Block, Conversation, Message as CanonicalMessage, Role};
use mezame::hub::{AttachedHub, HubCommand, HubRegistry};
use mezame::prompt::{assemble, Date, Part};
use mezame::provider::bedrock::{thinking_rule, to_bedrock_messages, Normaliser};
use mezame::provider::{ThinkingMode, TurnEvent, Usage};
use mezame::store::Store;
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
fn spawn_attach_loop(attached: AttachedHub) -> (u64, mpsc::Receiver<Message>) {
    let attach_id = attached.attach_id;
    let commands = attached.commands.clone();
    let (outbound, guard) = attached.take_outbound();
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
        drop(guard);
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
    (body, 0i64..4_000_000_000_000).prop_map(|(body, timestamp)| HistoryEntry {
        body,
        timestamp,
        usage: None,
    })
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
            let opener = registry
                .register_for_test(backend.clone(), SESSION_ID.into(), ready_event(), None)
                .await;
            let opener_commands = opener.commands.clone();
            let opener_attach_id = opener.attach_id;
            let (mut opener_rx, _opener_guard) = opener.take_outbound();

            opener_commands
                .send(HubCommand::Prompt {
                    blocks: vec![text_block("go")],
                    attach_id: opener_attach_id,
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

            let joiner = registry
                .attach_existing_for_test(SESSION_ID)
                .await
                .expect("hub registered");
            let busy = joiner.snapshot_ready["busy"] == Value::Bool(true);
            let (mut joiner_rx, _joiner_guard) = joiner.take_outbound();

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

// ---------- phase 1: the provider, the request and the prompt ----------

mod bedrock_events {
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStopEvent, ConversationRole,
        ConverseStreamOutput as StreamEvent, MessageStartEvent, MessageStopEvent,
        StopReason as BedrockStop,
    };

    pub fn start() -> StreamEvent {
        StreamEvent::MessageStart(
            MessageStartEvent::builder()
                .role(ConversationRole::Assistant)
                .build()
                .unwrap(),
        )
    }

    pub fn text(index: i32, text: &str) -> StreamEvent {
        StreamEvent::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(index)
                .delta(ContentBlockDelta::Text(text.to_string()))
                .build()
                .unwrap(),
        )
    }

    pub fn stop(index: i32) -> StreamEvent {
        StreamEvent::ContentBlockStop(
            ContentBlockStopEvent::builder()
                .content_block_index(index)
                .build()
                .unwrap(),
        )
    }

    pub fn end_turn() -> StreamEvent {
        StreamEvent::MessageStop(
            MessageStopEvent::builder()
                .stop_reason(BedrockStop::EndTurn)
                .build()
                .unwrap(),
        )
    }
}

fn non_empty_block() -> impl Strategy<Value = Block> {
    prop_oneof![
        "[a-zA-Z0-9 ,.!?]{1,40}".prop_map(|text| Block::Text { text }),
        prop::collection::vec(any::<u8>(), 1..8).prop_map(|data| Block::Image {
            media_type: "image/png".to_string(),
            data,
        }),
        "[a-z]{1,20}".prop_map(|text| Block::Thinking {
            text,
            signature: Some("sig".to_string()),
            provider: "bedrock".to_string(),
            model: "anthropic.claude-sonnet-5".to_string(),
        }),
    ]
}

fn exchanges() -> impl Strategy<Value = Vec<(Vec<Block>, Option<Vec<Block>>)>> {
    prop::collection::vec(
        (
            prop::collection::vec(non_empty_block(), 1..4),
            prop::option::of(prop::collection::vec(non_empty_block(), 1..4)),
        ),
        1..30,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // Feature: bedrock-end-to-end, Property 1: Normalised text is the text
    // that was fed
    #[test]
    fn property_p1_normalised_text_is_the_text_that_was_fed(
        chunks in prop::collection::vec("[\\p{L}\\p{N}\\p{M}\\p{P} ]{1,200}", 1..50)
    ) {
        let mut normaliser = Normaliser::new("anthropic.claude-sonnet-5");
        let mut events = normaliser.feed(bedrock_events::start());
        for chunk in &chunks {
            events.extend(normaliser.feed(bedrock_events::text(0, chunk)));
        }
        events.extend(normaliser.feed(bedrock_events::stop(0)));
        events.extend(normaliser.feed(bedrock_events::end_turn()));

        let starts_with_message_start = matches!(events.first(), Some(TurnEvent::MessageStart { .. }));
        prop_assert!(starts_with_message_start);
        let ends_with_end_turn = matches!(
            events.last(),
            Some(TurnEvent::Stop(mezame::provider::StopReason::EndTurn))
        );
        prop_assert!(ends_with_end_turn);
        let middle = &events[1..events.len() - 1];
        prop_assert_eq!(middle.len(), chunks.len());
        let mut joined = String::new();
        for event in middle {
            match event {
                TurnEvent::TextDelta(text) => joined.push_str(text),
                other => prop_assert!(false, "unexpected event {:?}", other),
            }
        }
        prop_assert_eq!(joined, chunks.concat());
    }

    // Feature: bedrock-end-to-end, Property 2: Built requests alternate
    // roles from `user`
    #[test]
    fn property_p2_built_requests_alternate_roles_from_user(exchanges in exchanges()) {
        use aws_sdk_bedrockruntime::types::{ContentBlock, ConversationRole};

        let last = exchanges.len() - 1;
        let mut messages = Vec::new();
        let mut input_blocks = 0usize;
        for (i, (user_blocks, assistant_blocks)) in exchanges.into_iter().enumerate() {
            input_blocks += user_blocks.len();
            messages.push(CanonicalMessage { role: Role::User, blocks: user_blocks });
            if i != last {
                if let Some(blocks) = assistant_blocks {
                    input_blocks += blocks.len();
                    messages.push(CanonicalMessage { role: Role::Assistant, blocks });
                }
            }
        }

        let built = to_bedrock_messages(&messages, "anthropic.claude-sonnet-5");
        prop_assert!(!built.is_empty());
        for (i, message) in built.iter().enumerate() {
            let expected = if i % 2 == 0 { ConversationRole::User } else { ConversationRole::Assistant };
            prop_assert_eq!(message.role(), &expected, "message {} has the wrong role", i);
        }
        prop_assert_eq!(built.last().unwrap().role(), &ConversationRole::User);

        let mut cache_points = Vec::new();
        let mut plain = 0usize;
        for (m, message) in built.iter().enumerate() {
            for (b, block) in message.content().iter().enumerate() {
                if matches!(block, ContentBlock::CachePoint(_)) {
                    cache_points.push((m, b));
                } else {
                    plain += 1;
                }
            }
        }
        prop_assert_eq!(plain, input_blocks);
        let last_message = built.len() - 1;
        let last_block = built[last_message].content().len() - 1;
        prop_assert_eq!(cache_points, vec![(last_message, last_block)]);
    }

    // Feature: bedrock-end-to-end, Property 3: The thinking rule is total
    // and agrees with the table
    #[test]
    fn property_p3_the_thinking_rule_is_total_and_agrees_with_the_table(id in ".{0,300}") {
        let _ = thinking_rule(&id);

        use ThinkingMode::{Adaptive, Enabled, Off};
        let table = [
            ("anthropic.claude-sonnet-5", Adaptive),
            ("anthropic.claude-opus-5", Adaptive),
            ("anthropic.claude-fable-5-1", Adaptive),
            ("anthropic.claude-opus-4-8", Adaptive),
            ("anthropic.claude-sonnet-4-6", Adaptive),
            ("anthropic.claude-opus-4-6-v1", Adaptive),
            ("anthropic.claude-sonnet-4-5-20250929-v1:0", Enabled),
            ("anthropic.claude-haiku-4-5-20251001-v1:0", Enabled),
            ("anthropic.claude-opus-4-5-20251101-v1:0", Enabled),
            ("anthropic.claude-opus-4-1-20250805-v1:0", Enabled),
            ("anthropic.claude-3-7-sonnet-20250219-v1:0", Enabled),
            ("anthropic.claude-3-5-haiku-20241022-v1:0", Off),
        ];
        for prefix in ["", "us.", "global.", "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us."] {
            for (base, expected) in table {
                let id = format!("{prefix}{base}");
                prop_assert_eq!(thinking_rule(&id), expected, "{}", id);
            }
        }
        prop_assert_eq!(thinking_rule("amazon.nova-pro-v1:0"), Off);
        prop_assert_eq!(thinking_rule(""), Off);
    }

    // Feature: bedrock-end-to-end, Property 5: Assembly is a function
    #[test]
    fn property_p5_assembly_is_a_function(
        texts in prop::collection::vec(".{0,80}", 0..6),
        year in 1i64..=9999,
        month in 1u32..=12,
        day in 1u32..=28,
    ) {
        let parts: Vec<Part> = texts.iter().map(|text| Part { name: "part", text: text.clone() }).collect();
        let date = Date { year, month, day };
        let first = assemble(&parts, date);
        let second = assemble(&parts, date);
        prop_assert_eq!(&first, &second);
        prop_assert_eq!(&first.date_line, &format!("Today's date is {date}."));
        prop_assert!(!first.static_text.contains("Today's date is"));
    }
}

fn payload_block() -> impl Strategy<Value = Block> {
    prop_oneof![
        "[a-z ]{1,256}".prop_map(|text| Block::Text { text }),
        prop::collection::vec(any::<u8>(), 0..1024).prop_map(|data| Block::Image {
            media_type: "image/png".to_string(),
            data,
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // Feature: bedrock-end-to-end, Property 4: The two stores evict together
    #[test]
    fn property_p4_the_two_stores_evict_together(
        exchanges in prop::collection::vec(
            (
                prop::collection::vec(payload_block(), 1..3),
                "[a-z ]{0,256}",
                prop::option::of("[a-z ]{0,256}"),
            ),
            1..60,
        )
    ) {
        // A lowered budget and cap so eviction happens within the run. A
        // reply that never came (`None`) closes the exchange with one entry
        // and one message; a reply adds one of each.
        let mut conversation = Conversation::with_budget_for_test(4 * 1024, 24);
        let mut expected: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        for (i, (blocks, user_text, agent_text)) in exchanges.into_iter().enumerate() {
            let stamp = i as i64;
            conversation.begin(
                CanonicalMessage { role: Role::User, blocks },
                HistoryEntry { body: EntryBody::User { text: user_text }, timestamp: stamp, usage: None },
            );
            expected.push_back(1);
            let held = conversation.exchange_count();
            prop_assert!(conversation.bytes() <= 4 * 1024 || held == 1);
            prop_assert!(conversation.history().len() <= 24 || held == 1);
            let assistant = agent_text.map(|text| (
                CanonicalMessage { role: Role::Assistant, blocks: vec![Block::Text { text: text.clone() }] },
                HistoryEntry { body: EntryBody::Agent { text }, timestamp: stamp, usage: None },
            ));
            match assistant {
                Some((message, entry)) => {
                    conversation.complete(Some(message), vec![entry]);
                    *expected.back_mut().unwrap() = 2;
                }
                None => {
                    conversation.complete(None, Vec::new());
                }
            }
            let held = conversation.exchange_count();
            prop_assert!(conversation.bytes() <= 4 * 1024 || held == 1);
            prop_assert!(conversation.history().len() <= 24 || held == 1);
            while expected.len() > held {
                expected.pop_front();
            }
            // Both stores hold the same exchanges, entry for message.
            let sum: usize = expected.iter().sum();
            prop_assert_eq!(conversation.history().len(), sum);
            prop_assert_eq!(conversation.messages().len(), sum);
            let users = conversation.messages().iter().filter(|m| m.role == Role::User).count();
            prop_assert_eq!(users, held);
        }
    }
}

// ---------- phase 2: identity ----------

fn cookie_key() -> [u8; 32] {
    mezame::store::crypto::MasterKey::from_bytes_for_test([3u8; 32])
        .keys()
        .cookie
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // Feature: store-auth-persistence, Property 2: A cookie changed anywhere
    // fails to verify. For any valid cookie and any single-byte change to
    // its rendered value that keeps it ASCII, `verify` returns `None`.
    #[test]
    fn property_p2_a_cookie_changed_anywhere_fails_to_verify(
        epoch in 0u64..1_000,
        now in 1_000_000_000i64..2_000_000_000,
        position in 0usize..200,
        replacement in 0x21u8..0x7f,
    ) {
        let key = cookie_key();
        let cookie = mezame::auth::Cookie::issue(
            "0123456789abcdef0123456789abcdef",
            epoch,
            now,
        );
        let value = mezame::auth::sign(&cookie, &key);
        prop_assert_eq!(mezame::auth::verify(&value, &key, now), Some(cookie));
        let mut bytes = value.clone().into_bytes();
        let at = position % bytes.len();
        if bytes[at] == replacement {
            return Ok(());
        }
        bytes[at] = replacement;
        let altered = String::from_utf8(bytes).expect("ASCII stays UTF-8");
        prop_assert_eq!(mezame::auth::verify(&altered, &key, now), None);
    }

    // Feature: store-auth-persistence, Property 3: The limiter admits ten.
    // For any sequence of 1 to 40 attempts at increasing instants inside one
    // window, the first ten pass and every later one is refused with a wait
    // no longer than the time left in the window.
    #[test]
    fn property_p3_the_limiter_admits_ten(
        offsets in proptest::collection::vec(0u64..59_000, 1..40),
    ) {
        let limiter = mezame::auth::RateLimiter::default();
        let start = std::time::Instant::now();
                let mut instants: Vec<u64> = offsets;
        instants.sort_unstable();
        // The window opens at the first attempt, not at the origin.
        let opened = instants[0];
        for (i, ms) in instants.iter().enumerate() {
            let at = start + std::time::Duration::from_millis(*ms);
            let outcome = limiter.check("u:alice", at);
            if i < mezame::auth::LOGIN_LIMIT as usize {
                prop_assert!(outcome.is_ok(), "attempt {i} at {ms} ms");
            } else {
                let left = outcome.expect_err("refused past ten");
                let remaining = mezame::auth::LOGIN_WINDOW
                    .saturating_sub(std::time::Duration::from_millis(*ms - opened));
                prop_assert!(
                    left <= remaining.max(std::time::Duration::from_secs(1)),
                    "wait {left:?} past the window's {remaining:?}"
                );
            }
        }
    }
}

// ---------- phase 2: persistence ----------

/// Where a stored exchange stands. `Open` is the shape a request that got
/// no reply leaves: the user row alone; in memory the exchange is closed
/// with no reply once the next one begins, and stays open when it is the
/// last, which is the one place the two differ and `messages()` does not.
#[derive(Debug, Clone, Copy)]
enum StoredStatus {
    Open,
    Closed,
    Rejected,
}

fn arb_usage() -> impl Strategy<Value = Usage> {
    (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()).prop_map(
        |(input, output, cache_read, cache_write)| Usage {
            input,
            output,
            cache_read,
            cache_write,
        },
    )
}

type StoredExchange = (
    Vec<Block>,
    String,
    Option<(Vec<Block>, Option<Usage>)>,
    StoredStatus,
);

fn stored_exchanges() -> impl Strategy<Value = Vec<StoredExchange>> {
    prop::collection::vec(
        (
            prop::collection::vec(non_empty_block(), 1..4),
            "[a-zA-Z0-9 ]{0,40}",
            prop::option::of((
                prop::collection::vec(non_empty_block(), 1..4),
                prop::option::of(arb_usage()),
            )),
            prop_oneof![
                Just(StoredStatus::Open),
                Just(StoredStatus::Closed),
                Just(StoredStatus::Rejected),
            ],
        ),
        1..30,
    )
}

/// The entries the loop records for a reply: a thought per thinking block
/// with text, then the agent text, the text blocks concatenated with
/// nothing between them, as the live accumulator concatenates deltas.
fn reply_entries(blocks: &[Block], usage: Option<Usage>, timestamp: i64) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    for block in blocks {
        if let Block::Thinking { text, .. } = block {
            if !text.is_empty() {
                entries.push(HistoryEntry {
                    body: EntryBody::Thought { text: text.clone() },
                    timestamp,
                    usage: None,
                });
            }
        }
    }
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if !text.is_empty() {
        entries.push(HistoryEntry {
            body: EntryBody::Agent { text },
            timestamp,
            usage,
        });
    }
    entries
}

fn transcript_shape(conversation: &Conversation) -> Vec<(String, Option<Usage>, i64)> {
    conversation
        .history()
        .iter()
        .map(|entry| (format!("{:?}", entry.body), entry.usage, entry.timestamp))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // Feature: store-auth-persistence, Property 1: The store round-trips a
    // conversation. Written through `append_user`, `append_assistant` and
    // `mark_rejected`, loaded under a bound large enough for all of it and
    // rebuilt, a conversation has the same `messages()` and the same
    // transcript as the one built in memory from the same sequence, both
    // after `strip_reasoning`.
    #[test]
    fn property_p1_the_store_round_trips_a_conversation(exchanges in stored_exchanges()) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime");
        rt.block_on(async move {
            let keys = mezame::store::crypto::MasterKey::from_bytes_for_test([11u8; 32]).keys();
            let store = mezame::store::sqlite::SqliteStore::open_in_memory(keys).unwrap();
            let user = store
                .create_user("alice", "$argon2id$stub", mezame::store::Role::User, 1)
                .await
                .unwrap();
            store.create_session(&user.id, "p1", None, 1).await.unwrap();

            let mut live = Conversation::new();
            let count = exchanges.len();
            for (i, (user_blocks, text, assistant, status)) in exchanges.into_iter().enumerate() {
                let stamp = i as i64 * 10;
                let last = i + 1 == count;
                let user_row = store.append_user("p1", &user_blocks, &text, stamp).await.unwrap();
                live.begin(
                    CanonicalMessage { role: Role::User, blocks: user_blocks },
                    HistoryEntry { body: EntryBody::User { text }, timestamp: stamp, usage: None },
                );
                live.set_user_row(user_row);
                match status {
                    StoredStatus::Open => {
                        if !last {
                            live.complete(None, Vec::new());
                        }
                    }
                    StoredStatus::Closed => match assistant {
                        Some((blocks, usage)) => {
                            store.append_assistant("p1", &blocks, usage, false, stamp + 1).await.unwrap();
                            let entries = reply_entries(&blocks, usage, stamp + 1);
                            live.complete(
                                Some(CanonicalMessage { role: Role::Assistant, blocks }),
                                entries,
                            );
                        }
                        None => {
                            live.complete(None, Vec::new());
                        }
                    },
                    StoredStatus::Rejected => {
                        let entries = match &assistant {
                            Some((blocks, usage)) => {
                                store.append_assistant("p1", blocks, *usage, true, stamp + 1).await.unwrap();
                                reply_entries(blocks, *usage, stamp + 1)
                            }
                            None => Vec::new(),
                        };
                        let rows = live.reject_open(entries).expect("an open exchange");
                        store.mark_rejected(&rows).await.unwrap();
                    }
                }
            }
            live.strip_reasoning();

            let window = store.load_window("p1", 10_000, 1 << 30).await.unwrap();
            prop_assert!(window.complete, "the bound holds every row");
            let mut restored = Conversation::new();
            restored.restore(window);

            prop_assert_eq!(restored.messages(), live.messages());
            prop_assert_eq!(transcript_shape(&restored), transcript_shape(&live));
            prop_assert_eq!(restored.exchange_count(), live.exchange_count());
            Ok(())
        })?;
    }
}
