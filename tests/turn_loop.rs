//! The turn loop driven directly, with no hub, over a `ScriptedProvider`.
//! What the loop records, what it streams, how each way a turn can end
//! resolves, and that none of them leaves a session stuck.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mezame::backend::{Backend, EntryBody, HistoryEntry, TurnOutcome};
use mezame::conversation::{Block, Conversation, Role};
use mezame::provider::{LoopSettings, StopReason, ThinkingMode, TurnEvent, Usage};
use mezame::turn::{
    session_info_for, stop_name, LoopBackend, TurnLog, CLOSED_ERROR, CONTENT_FILTERED_ERROR,
    CONTEXT_WINDOW_ERROR, REFUSAL_ERROR, STREAM_ENDED_ERROR, TOOL_ERROR,
};
use serde_json::{json, Value};
use support::{ScriptedProvider, ScriptedStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const SONNET: &str = "anthropic.claude-sonnet-5";
const HAIKU: &str = "anthropic.claude-haiku-4-5-20251001-v1:0";

fn settings() -> LoopSettings {
    LoopSettings {
        model: SONNET.to_string(),
        models: vec![SONNET.to_string(), HAIKU.to_string()],
        thinking: None,
        thinking_budget: 4096,
        max_output_tokens: 16384,
    }
}

fn backend(provider: &Arc<ScriptedProvider>) -> LoopBackend {
    LoopBackend::new(
        Arc::clone(provider) as Arc<dyn mezame::provider::Provider>,
        settings(),
        "test-session",
        "alice",
        None,
    )
}

fn text(text: &str) -> Vec<Value> {
    vec![json!({ "type": "text", "text": text })]
}

fn events(list: Vec<TurnEvent>) -> ScriptedStream {
    ScriptedStream::Events(list)
}

/// Run one turn and collect what it streamed.
async fn turn(
    backend: &LoopBackend,
    blocks: Vec<Value>,
) -> (anyhow::Result<TurnOutcome>, Vec<Value>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = backend.prompt(blocks, tx).await;
    let mut frames = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        frames.push(frame);
    }
    (result, frames)
}

async fn history_texts(backend: &LoopBackend) -> Vec<String> {
    backend
        .history()
        .await
        .iter()
        .map(|entry| match &entry.body {
            EntryBody::User { text } => format!("user:{text}"),
            EntryBody::Agent { text } => format!("agent:{text}"),
            EntryBody::Thought { text } => format!("thought:{text}"),
            other => format!("{other:?}"),
        })
        .collect()
}

async fn wait_for_requests(provider: &ScriptedProvider, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while provider.request_count() < count && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(provider.request_count(), count, "the request arrived");
}

fn end_turn() -> TurnEvent {
    TurnEvent::Stop(StopReason::EndTurn)
}

fn usage() -> TurnEvent {
    TurnEvent::Usage(Usage {
        input: 17,
        output: 700,
        cache_read: 1370,
        cache_write: 0,
    })
}

#[tokio::test]
async fn the_user_entry_is_recorded_before_the_stream_starts() {
    let provider = Arc::new(ScriptedProvider::with_stream(ScriptedStream::Pending {
        first: Vec::new(),
    }));
    let backend = Arc::new(backend(&provider));
    let (tx, _rx) = mpsc::unbounded_channel();
    let running = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { backend.prompt(text("hello"), tx).await })
    };
    wait_for_requests(&provider, 1).await;
    assert_eq!(history_texts(&backend).await, vec!["user:hello"]);
    assert!(backend.has_open_exchange_for_test());
    let request = &provider.requests()[0];
    assert_eq!(request.model, SONNET);
    assert_eq!(request.thinking, ThinkingMode::Adaptive);
    assert_eq!(request.roles, vec![Role::User]);
    assert!(request.system_static.contains("You are Mezame"));
    assert!(request.date_line.starts_with("Today's date is "));
    provider.release_stream(vec![end_turn()]);
    assert!(running.await.unwrap().is_ok());
    assert!(!backend.has_open_exchange_for_test());
}

#[tokio::test]
async fn text_becomes_appends_thinking_becomes_thoughts_and_both_are_kept() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        events(vec![
            TurnEvent::MessageStart {
                model: SONNET.into(),
            },
            TurnEvent::ThinkingStart { id: "0".into() },
            TurnEvent::ThinkingDelta {
                id: "0".into(),
                text: "Let me".into(),
            },
            TurnEvent::ThinkingDelta {
                id: "0".into(),
                text: " see".into(),
            },
            TurnEvent::ThinkingEnd {
                id: "0".into(),
                signature: Some("sig".into()),
            },
            TurnEvent::TextDelta("Hi".into()),
            TurnEvent::TextDelta("!".into()),
            end_turn(),
            usage(),
        ]),
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    let (result, frames) = turn(&backend, text("hello")).await;
    let outcome = result.unwrap();
    assert_eq!(
        outcome.usage,
        Some(Usage {
            input: 17,
            output: 700,
            cache_read: 1370,
            cache_write: 0
        })
    );
    assert_eq!(
        frames,
        vec![
            json!({ "type": "thought", "text": "Let me" }),
            json!({ "type": "thought", "text": " see" }),
            json!({ "type": "append", "role": "agent", "text": "Hi" }),
            json!({ "type": "append", "role": "agent", "text": "!" }),
        ]
    );
    assert_eq!(
        history_texts(&backend).await,
        vec!["user:hello", "thought:Let me see", "agent:Hi!"]
    );
    // The next request replays the assistant message in stream order.
    turn(&backend, text("again")).await.0.unwrap();
    let second = &provider.requests()[1];
    assert_eq!(second.roles, vec![Role::User, Role::Assistant, Role::User]);
    assert_eq!(
        second.messages[1].blocks,
        vec![
            Block::Thinking {
                text: "Let me see".into(),
                signature: Some("sig".into()),
                provider: "bedrock".into(),
                model: SONNET.into(),
            },
            Block::Text { text: "Hi!".into() },
        ]
    );
}

#[tokio::test]
async fn a_signature_only_block_is_kept_and_records_no_thought() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        events(vec![
            TurnEvent::ThinkingEnd {
                id: "0".into(),
                signature: Some("sig".into()),
            },
            TurnEvent::TextDelta("x".into()),
            end_turn(),
        ]),
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    turn(&backend, text("q")).await.0.unwrap();
    assert_eq!(history_texts(&backend).await, vec!["user:q", "agent:x"]);
    turn(&backend, text("q2")).await.0.unwrap();
    assert!(matches!(
        &provider.requests()[1].messages[1].blocks[0],
        Block::Thinking { text, signature: Some(_), .. } if text.is_empty()
    ));
}

#[tokio::test]
async fn an_unsigned_or_unfinished_thinking_block_is_never_persisted() {
    // Ends after a delta with no end and no stop.
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        events(vec![
            TurnEvent::ThinkingStart { id: "0".into() },
            TurnEvent::ThinkingDelta {
                id: "0".into(),
                text: "half".into(),
            },
        ]),
        events(vec![end_turn()]),
        events(vec![
            TurnEvent::ThinkingStart { id: "0".into() },
            TurnEvent::ThinkingDelta {
                id: "0".into(),
                text: "unsigned".into(),
            },
            TurnEvent::ThinkingEnd {
                id: "0".into(),
                signature: None,
            },
            TurnEvent::TextDelta("t".into()),
            end_turn(),
        ]),
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    let (result, frames) = turn(&backend, text("q")).await;
    assert_eq!(result.unwrap_err().to_string(), STREAM_ENDED_ERROR);
    assert_eq!(frames, vec![json!({ "type": "thought", "text": "half" })]);
    assert_eq!(
        history_texts(&backend).await,
        vec!["user:q"],
        "no thought entry"
    );
    turn(&backend, text("q2")).await.0.unwrap();
    assert_eq!(
        provider.requests()[1].roles,
        vec![Role::User, Role::User],
        "no assistant message was persisted"
    );
    // Ends with an unsigned block and text: the text is kept, the block is not.
    turn(&backend, text("q3")).await.0.unwrap();
    assert_eq!(
        history_texts(&backend).await,
        vec!["user:q", "user:q2", "user:q3", "agent:t"]
    );
    turn(&backend, text("q4")).await.0.unwrap();
    let requests = provider.requests();
    let last_assistant = requests[3]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .unwrap();
    assert_eq!(
        last_assistant.blocks,
        vec![Block::Text { text: "t".into() }]
    );
}

#[tokio::test]
async fn every_stop_reason_has_its_outcome() {
    let cases: Vec<(StopReason, Result<(), String>)> = vec![
        (StopReason::EndTurn, Ok(())),
        (StopReason::StopSequence, Ok(())),
        (StopReason::MaxTokens, Ok(())),
        (
            StopReason::ContextWindowExceeded,
            Err(CONTEXT_WINDOW_ERROR.into()),
        ),
        (
            StopReason::ContentFiltered,
            Err(CONTENT_FILTERED_ERROR.into()),
        ),
        (StopReason::Refusal, Err(REFUSAL_ERROR.into())),
        (StopReason::ToolUse, Err(TOOL_ERROR.into())),
        (
            StopReason::Other("malformed_tool_use".into()),
            Err("The model stopped for an unexpected reason: malformed_tool_use.".into()),
        ),
    ];
    for (stop, expected) in cases {
        let provider = Arc::new(ScriptedProvider::with_stream(events(vec![
            TurnEvent::TextDelta("t".into()),
            TurnEvent::Stop(stop.clone()),
            usage(),
        ])));
        let backend = backend(&provider);
        let (result, frames) = turn(&backend, text("q")).await;
        match expected {
            Ok(()) => {
                let outcome = result.unwrap_or_else(|e| panic!("{stop:?}: {e}"));
                assert!(outcome.usage.is_some(), "{stop:?}");
            }
            Err(message) => assert_eq!(result.unwrap_err().to_string(), message, "{stop:?}"),
        }
        if stop == StopReason::MaxTokens {
            assert_eq!(
                frames.last().unwrap(),
                &json!({ "type": "append", "role": "sys", "text": "\n[The reply stopped at the output limit of 16384 tokens.]\n" })
            );
        } else {
            assert!(frames.iter().all(|f| f["role"] != "sys"), "{stop:?}");
        }
        // Whatever the reason, the text so far is kept.
        assert_eq!(
            history_texts(&backend).await,
            vec!["user:q", "agent:t"],
            "{stop:?}"
        );
    }
}

#[tokio::test]
async fn a_tool_use_start_ends_the_turn_and_persists_no_tool_block() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        events(vec![
            TurnEvent::TextDelta("a".into()),
            TurnEvent::ToolUseStart {
                id: "t".into(),
                name: "read".into(),
            },
            TurnEvent::ToolInputDelta {
                id: "t".into(),
                json_fragment: "{}".into(),
            },
            TurnEvent::ToolUseEnd { id: "t".into() },
            TurnEvent::Stop(StopReason::ToolUse),
        ]),
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    let (result, _) = turn(&backend, text("q")).await;
    assert_eq!(result.unwrap_err().to_string(), TOOL_ERROR);
    assert_eq!(history_texts(&backend).await, vec!["user:q", "agent:a"]);
    turn(&backend, text("q2")).await.0.unwrap();
    assert_eq!(
        provider.requests()[1].messages[1].blocks,
        vec![Block::Text { text: "a".into() }]
    );
}

#[tokio::test]
async fn a_pre_stream_failure_keeps_the_question_and_the_next_request_carries_both() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        ScriptedStream::BeforeStream {
            retryable: true,
            rejected: false,
            message: "Bedrock is throttling requests".into(),
        },
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    let (result, frames) = turn(&backend, text("first")).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "Bedrock is throttling requests"
    );
    assert!(frames.is_empty());
    assert_eq!(history_texts(&backend).await, vec!["user:first"]);
    assert!(
        !backend.has_open_exchange_for_test(),
        "a reply that never came closes the exchange"
    );
    turn(&backend, text("second")).await.0.unwrap();
    let request = &provider.requests()[1];
    assert_eq!(
        request.roles,
        vec![Role::User, Role::User],
        "both ride into the request; the builder merges them"
    );
    assert_eq!(request.messages[0].text(), "first");
    assert_eq!(request.messages[1].text(), "second");
}

#[tokio::test]
async fn a_rejected_request_keeps_its_entry_and_leaves_later_requests() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        ScriptedStream::BeforeStream {
            retryable: false,
            rejected: true,
            message: "Bedrock rejected the request: blank.".into(),
        },
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    let (result, _) = turn(&backend, text("refused")).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "Bedrock rejected the request: blank."
    );
    assert_eq!(history_texts(&backend).await, vec!["user:refused"]);
    turn(&backend, text("ok")).await.0.unwrap();
    let request = &provider.requests()[1];
    assert_eq!(request.roles, vec![Role::User]);
    assert_eq!(request.messages[0].text(), "ok");
    assert_eq!(
        history_texts(&backend).await,
        vec!["user:refused", "user:ok"]
    );
}

#[tokio::test]
async fn a_mid_stream_error_persists_the_partial_text() {
    let provider = Arc::new(ScriptedProvider::with_stream(events(vec![
        TurnEvent::TextDelta("par".into()),
        TurnEvent::Error {
            retryable: true,
            message: "Could not reach Bedrock: reset.".into(),
        },
    ])));
    let backend = backend(&provider);
    let (result, frames) = turn(&backend, text("q")).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "Could not reach Bedrock: reset."
    );
    assert_eq!(frames.len(), 1);
    assert_eq!(history_texts(&backend).await, vec!["user:q", "agent:par"]);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_stream_is_cut_after_the_idle_timeout() {
    let provider = Arc::new(ScriptedProvider::with_stream(ScriptedStream::Pending {
        first: vec![TurnEvent::TextDelta("a".into())],
    }));
    let backend = backend(&provider).with_idle_timeout_for_test(Duration::from_millis(50));
    // Bounded from the outside too: a loop that never cut the stall would
    // otherwise hang this test rather than fail it.
    let (result, frames) = timeout(Duration::from_secs(5), turn(&backend, text("q")))
        .await
        .expect("the turn resolved within the bound");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("cut off"), "{err}");
    assert_eq!(frames.len(), 1);
    assert_eq!(history_texts(&backend).await, vec!["user:q", "agent:a"]);
    assert!(!backend.has_open_exchange_for_test());
}

#[tokio::test]
async fn cancel_during_a_pending_send_resolves_within_a_second_and_streams_nothing() {
    let provider = Arc::new(ScriptedProvider::with_stream(ScriptedStream::PendingSend));
    let backend = Arc::new(backend(&provider));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let running = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { backend.prompt(text("q"), tx).await })
    };
    wait_for_requests(&provider, 1).await;
    backend.cancel().await;
    let outcome = timeout(Duration::from_secs(1), running)
        .await
        .expect("resolved within a second")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.usage, None);
    assert!(rx.try_recv().is_err(), "no frame");
    assert_eq!(history_texts(&backend).await, vec!["user:q"]);
    assert!(!backend.has_open_exchange_for_test());
}

#[tokio::test]
async fn cancel_mid_stream_keeps_the_signed_blocks_and_drops_the_unsigned_one() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        ScriptedStream::Pending {
            first: vec![
                TurnEvent::ThinkingEnd {
                    id: "0".into(),
                    signature: Some("s".into()),
                },
                TurnEvent::TextDelta("partial".into()),
                TurnEvent::ThinkingStart { id: "1".into() },
                TurnEvent::ThinkingDelta {
                    id: "1".into(),
                    text: "open".into(),
                },
                usage(),
            ],
        },
        events(vec![end_turn()]),
    ]));
    let backend = Arc::new(backend(&provider));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let running = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { backend.prompt(text("q"), tx).await })
    };
    // Wait for the open block's thought to arrive, then cancel.
    let mut seen = 0;
    while seen < 2 {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        seen += 1;
    }
    backend.cancel().await;
    let outcome = timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(outcome.usage, None, "a cancelled turn reports no usage");
    assert_eq!(
        history_texts(&backend).await,
        vec!["user:q", "agent:partial"]
    );
    turn(&backend, text("q2")).await.0.unwrap();
    assert_eq!(
        provider.requests()[1].messages[1].blocks,
        vec![
            Block::Thinking {
                text: String::new(),
                signature: Some("s".into()),
                provider: "bedrock".into(),
                model: SONNET.into(),
            },
            Block::Text {
                text: "partial".into()
            },
        ]
    );
}

#[tokio::test]
async fn cancel_with_no_turn_open_is_a_no_op() {
    let provider = Arc::new(ScriptedProvider::with_stream(events(vec![end_turn()])));
    let backend = backend(&provider);
    backend.cancel().await;
    turn(&backend, text("q")).await.0.unwrap();
    assert_eq!(provider.request_count(), 1);
}

#[tokio::test]
async fn shutdown_during_a_pending_turn_clears_everything_and_refuses_later_prompts() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        ScriptedStream::Pending {
            first: vec![TurnEvent::TextDelta("a".into())],
        },
        events(vec![end_turn()]),
    ]));
    let backend = Arc::new(backend(&provider));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let running = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { backend.prompt(text("q"), tx).await })
    };
    timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    backend.shutdown().await;
    let outcome = timeout(Duration::from_secs(1), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(outcome.usage, None);
    assert!(
        history_texts(&backend).await.is_empty(),
        "cleared, and the turn persisted nothing"
    );
    backend.shutdown().await;
    let (result, _) = turn(&backend, text("later")).await;
    assert_eq!(result.unwrap_err().to_string(), CLOSED_ERROR);
    assert_eq!(provider.request_count(), 1, "no request after shutdown");
    assert!(history_texts(&backend).await.is_empty());
}

#[tokio::test]
async fn set_model_switches_the_next_request_and_its_thinking_mode() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        events(vec![end_turn()]),
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    let info = backend.set_model(HAIKU.to_string()).await.unwrap();
    assert_eq!(info["models"]["currentModelId"], HAIKU);
    assert_eq!(
        info["models"]["availableModels"],
        json!([
            { "modelId": SONNET, "name": SONNET, "description": "adaptive thinking" },
            { "modelId": HAIKU, "name": HAIKU, "description": "thinking with a budget" }
        ])
    );
    assert_eq!(backend.current_model(), HAIKU);
    turn(&backend, text("q")).await.0.unwrap();
    let request = &provider.requests()[0];
    assert_eq!(request.model, HAIKU);
    assert_eq!(request.thinking, ThinkingMode::Enabled);
    assert_eq!(request.thinking_budget, 4096);

    let err = backend
        .set_model("amazon.nova-pro-v1:0".to_string())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not a configured model") && err.contains(SONNET) && err.contains(HAIKU),
        "{err}"
    );
    assert_eq!(
        backend.current_model(),
        HAIKU,
        "a refused change changes nothing"
    );
    assert_eq!(
        session_info_for(&settings(), SONNET)["models"]["currentModelId"],
        SONNET
    );
}

#[tokio::test]
async fn history_returns_while_a_turn_is_pending() {
    let provider = Arc::new(ScriptedProvider::with_stream(ScriptedStream::Pending {
        first: Vec::new(),
    }));
    let backend = Arc::new(backend(&provider));
    let (tx, _rx) = mpsc::unbounded_channel();
    let running = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { backend.prompt(text("pending"), tx).await })
    };
    wait_for_requests(&provider, 1).await;
    let history = timeout(Duration::from_secs(1), backend.history())
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    provider.release_stream(vec![end_turn()]);
    running.await.unwrap().unwrap();
}

#[tokio::test]
async fn eviction_drops_the_same_exchanges_from_both_stores() {
    let provider = Arc::new(ScriptedProvider::new());
    for _ in 0..6 {
        provider.push_stream(events(vec![
            TurnEvent::TextDelta("answer".into()),
            end_turn(),
        ]));
    }
    // 60 bytes: an exchange of "question-N" + "answer" is 16, so three fit.
    let backend = backend(&provider)
        .with_conversation_for_test(Conversation::with_budget_for_test(60, 10_000));
    for i in 0..5 {
        turn(&backend, text(&format!("question-{i}")))
            .await
            .0
            .unwrap();
    }
    let history = history_texts(&backend).await;
    assert_eq!(
        history,
        vec![
            "user:question-2",
            "agent:answer",
            "user:question-3",
            "agent:answer",
            "user:question-4",
            "agent:answer"
        ]
    );
    turn(&backend, text("question-5")).await.0.unwrap();
    let request = &provider.requests()[5];
    let user_texts: Vec<String> = request
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.text())
        .collect();
    // At request time the open exchange still fits (48 + 10 bytes), so four
    // questions ride along; the answer then tips the budget and question-2
    // leaves both stores together.
    assert_eq!(
        user_texts,
        vec!["question-2", "question-3", "question-4", "question-5"],
        "the request holds the exchanges the transcript held when it was built"
    );
    assert_eq!(
        history_texts(&backend).await,
        vec![
            "user:question-3",
            "agent:answer",
            "user:question-4",
            "agent:answer",
            "user:question-5",
            "agent:answer"
        ]
    );
}

#[tokio::test]
async fn an_unsupported_block_or_an_over_limit_message_resolves_before_any_request() {
    let provider = Arc::new(ScriptedProvider::with_stream(events(vec![end_turn()])));
    let backend = backend(&provider);
    let (result, frames) = turn(
        &backend,
        vec![json!({ "type": "resource", "resource": { "uri": "file:///a.zip", "mimeType": "application/zip", "blob": "AQID" } })],
    )
    .await;
    let err = result.unwrap_err().to_string();
    assert!(
        err.starts_with("Block 0 (`resource`, `application/zip`)"),
        "{err}"
    );
    assert!(frames.is_empty());
    let too_many: Vec<Value> = (0..21)
        .map(|_| json!({ "type": "image", "mimeType": "image/png", "data": "AQID" }))
        .collect();
    let (result, _) = turn(&backend, too_many).await;
    assert!(result.unwrap_err().to_string().contains("over the limit"));
    let (result, _) = turn(&backend, vec![json!({ "type": "text", "text": "   " })]).await;
    assert!(result.unwrap_err().to_string().contains("nothing to send"));
    assert_eq!(provider.request_count(), 0);
    assert!(history_texts(&backend).await.is_empty());
}

#[tokio::test]
async fn an_unscripted_request_fails_before_the_stream_and_is_not_rejected() {
    let provider = Arc::new(ScriptedProvider::new());
    let backend = backend(&provider);
    let (result, _) = turn(&backend, text("q")).await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("no stream was scripted"));
    assert_eq!(history_texts(&backend).await, vec!["user:q"]);
}

#[test]
fn the_log_line_renders_each_outcome() {
    let ok = TurnLog {
        session: "s1",
        user: "alice",
        model: SONNET,
        outcome: "ok",
        stop: Some(&StopReason::EndTurn),
        usage: Some(Usage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
        }),
        elapsed: Duration::from_millis(1234),
        error: None,
    };
    assert_eq!(
        ok.render(),
        format!("turn session=s1 user=alice model={SONNET} outcome=ok stop=end_turn in=1 out=2 cache_read=3 cache_write=4 ms=1234")
    );
    let cancelled = TurnLog {
        session: "s1",
        user: "alice",
        model: SONNET,
        outcome: "cancelled",
        stop: None,
        usage: None,
        elapsed: Duration::from_millis(5),
        error: None,
    };
    assert_eq!(
        cancelled.render(),
        format!("turn session=s1 user=alice model={SONNET} outcome=cancelled stop=- in=- out=- cache_read=- cache_write=- ms=5")
    );
    let error = TurnLog {
        session: "s1",
        user: "alice",
        model: SONNET,
        outcome: "error",
        stop: Some(&StopReason::Other("weird".into())),
        usage: None,
        elapsed: Duration::from_millis(9),
        error: Some("line one\nline two".into()),
    };
    assert_eq!(
        error.render(),
        format!("turn session=s1 user=alice model={SONNET} outcome=error stop=weird in=- out=- cache_read=- cache_write=- ms=9 error=line one line two")
    );
    assert_eq!(
        stop_name(&StopReason::ContextWindowExceeded),
        "context_window_exceeded"
    );
}

#[test]
fn a_history_entry_carries_the_turn_s_timestamp() {
    let entry = HistoryEntry {
        body: EntryBody::Thought { text: "t".into() },
        timestamp: 5,
        usage: None,
    };
    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({ "role": "thought", "text": "t", "timestamp": 5 })
    );
}

#[tokio::test]
async fn a_transport_error_after_the_stop_is_still_a_complete_reply() {
    // Bedrock sends the stop, then the counts, then the end of body. A
    // transport error between the stop and the counts must not turn the
    // whole answer into a failed one; the counts are simply unknown.
    let provider = Arc::new(ScriptedProvider::with_stream(events(vec![
        TurnEvent::TextDelta("done".into()),
        end_turn(),
        TurnEvent::Error {
            retryable: true,
            message: "reset reading the tail".into(),
        },
    ])));
    let backend = backend(&provider);
    let (result, _) = turn(&backend, text("q")).await;
    let outcome = result.expect("a whole answer is not an error");
    assert_eq!(outcome.usage, None, "the counts never arrived");
    assert_eq!(history_texts(&backend).await, vec!["user:q", "agent:done"]);
}

#[tokio::test]
async fn once_the_stop_and_the_counts_are_in_the_turn_completes_without_waiting_for_the_tail() {
    // The stream is never closed by the provider; the loop must not wait
    // for its end, nor let a late cancel turn the reply into a cancelled one.
    let provider = Arc::new(ScriptedProvider::with_stream(ScriptedStream::Pending {
        first: vec![TurnEvent::TextDelta("whole".into()), end_turn(), usage()],
    }));
    let backend = backend(&provider);
    let (result, _) = timeout(Duration::from_secs(1), turn(&backend, text("q")))
        .await
        .expect("resolved without the stream's end");
    let outcome = result.unwrap();
    assert_eq!(outcome.usage.map(|u| u.output), Some(700));
}

#[tokio::test]
async fn a_reply_with_reasoning_but_no_text_is_not_persisted() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        events(vec![
            TurnEvent::ThinkingStart { id: "0".into() },
            TurnEvent::ThinkingDelta {
                id: "0".into(),
                text: "thinking only".into(),
            },
            TurnEvent::ThinkingEnd {
                id: "0".into(),
                signature: Some("s".into()),
            },
            end_turn(),
        ]),
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    let (result, frames) = turn(&backend, text("q")).await;
    result.unwrap();
    assert_eq!(frames.len(), 1, "the thought streamed live");
    assert_eq!(
        history_texts(&backend).await,
        vec!["user:q"],
        "nothing kept without text"
    );
    turn(&backend, text("q2")).await.0.unwrap();
    assert_eq!(provider.requests()[1].roles, vec![Role::User, Role::User]);
}

#[tokio::test]
async fn a_refused_or_filtered_reply_leaves_later_requests_and_keeps_its_transcript() {
    for (stop, expected) in [
        (StopReason::Refusal, REFUSAL_ERROR),
        (StopReason::ContentFiltered, CONTENT_FILTERED_ERROR),
    ] {
        let provider = Arc::new(ScriptedProvider::with_streams(vec![
            events(vec![
                TurnEvent::TextDelta("par".into()),
                TurnEvent::Stop(stop.clone()),
            ]),
            events(vec![end_turn()]),
        ]));
        let backend = backend(&provider);
        let (result, _) = turn(&backend, text("bad")).await;
        assert_eq!(result.unwrap_err().to_string(), expected, "{stop:?}");
        assert_eq!(
            history_texts(&backend).await,
            vec!["user:bad", "agent:par"],
            "{stop:?}"
        );
        assert!(!backend.has_open_exchange_for_test());
        turn(&backend, text("next")).await.0.unwrap();
        let request = &provider.requests()[1];
        assert_eq!(
            request.roles,
            vec![Role::User],
            "{stop:?}: the refused exchange is gone"
        );
        assert_eq!(request.messages[0].text(), "next");
    }
}

#[tokio::test]
async fn a_cancel_that_lands_before_the_turn_is_polled_cancels_that_turn() {
    let provider = Arc::new(ScriptedProvider::with_stream(events(vec![
        TurnEvent::TextDelta("never".into()),
        end_turn(),
    ])));
    let backend = backend(&provider);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let future = backend.prompt(text("q"), tx);
    // The handle exists before the future is polled, so this reaches it.
    backend.cancel().await;
    let outcome = future.await.unwrap();
    assert_eq!(outcome.usage, None);
    assert_eq!(provider.request_count(), 0, "cancelled before any request");
    assert!(rx.try_recv().is_err());
    assert_eq!(history_texts(&backend).await, vec!["user:q"]);
    // And a cancel after the turn is over reaches nothing: the next turn
    // runs to completion.
    provider.push_stream(events(vec![TurnEvent::TextDelta("ok".into()), end_turn()]));
    backend.cancel().await;
    let (result, _) = turn(&backend, text("q2")).await;
    result.unwrap();
    assert_eq!(provider.request_count(), 1);
}

// ---------- review fixes, 2026-09-08 ----------

fn image(data: &str) -> Value {
    json!({ "type": "image", "mimeType": "image/png", "data": data })
}

fn thinking_turn() -> ScriptedStream {
    events(vec![
        TurnEvent::MessageStart {
            model: SONNET.into(),
        },
        TurnEvent::ThinkingStart { id: "0".into() },
        TurnEvent::ThinkingDelta {
            id: "0".into(),
            text: "hmm".into(),
        },
        TurnEvent::ThinkingEnd {
            id: "0".into(),
            signature: Some("sig".into()),
        },
        TurnEvent::TextDelta("Hi".into()),
        end_turn(),
        usage(),
    ])
}

#[tokio::test]
async fn a_rejection_excludes_the_unanswered_run_merged_into_the_refused_request() {
    // Turn 1 fails before any event for a transient reason: its message
    // stays for the next request. Turn 2 is refused for its content: what
    // the service saw was turns 1 and 2 merged, so both leave. Turn 3
    // carries its own text alone.
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        ScriptedStream::BeforeStream {
            retryable: true,
            rejected: false,
            message: "throttled".into(),
        },
        ScriptedStream::BeforeStream {
            retryable: false,
            rejected: true,
            message: "Bedrock rejected the request: image too large.".into(),
        },
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    assert!(turn(&backend, text("one")).await.0.is_err());
    assert!(turn(&backend, text("two")).await.0.is_err());
    let (result, _) = turn(&backend, text("three")).await;
    assert!(result.is_ok());
    let requests = provider.requests();
    assert_eq!(
        requests[1].roles,
        vec![Role::User, Role::User],
        "the unanswered first message rode along and the builder merges the two"
    );
    assert_eq!(
        requests[2].roles,
        vec![Role::User],
        "both refused messages are gone"
    );
    assert_eq!(
        requests[2].messages[0].blocks,
        vec![Block::Text {
            text: "three".into()
        }]
    );
    assert_eq!(
        history_texts(&backend).await,
        vec!["user:one", "user:two", "user:three"]
    );
}

#[tokio::test]
async fn a_too_long_rejection_leaves_the_message_out_of_later_requests() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        ScriptedStream::BeforeStream {
            retryable: false,
            rejected: true,
            message: mezame::provider::CONTEXT_WINDOW_ERROR.into(),
        },
        events(vec![TurnEvent::TextDelta("ok".into()), end_turn()]),
    ]));
    let backend = backend(&provider);
    let (result, _) = turn(&backend, text("a huge paste")).await;
    assert!(result.unwrap_err().to_string().contains("context"));
    let (result, _) = turn(&backend, text("hello")).await;
    assert!(result.is_ok());
    let requests = provider.requests();
    assert_eq!(requests[1].messages.len(), 1);
    assert_eq!(
        requests[1].messages[0].blocks,
        vec![Block::Text {
            text: "hello".into()
        }]
    );
}

#[tokio::test]
async fn stale_reasoning_is_dropped_and_the_request_retried_once() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        thinking_turn(),
        ScriptedStream::StaleReasoning,
        events(vec![TurnEvent::TextDelta("ok".into()), end_turn()]),
    ]));
    let backend = backend(&provider);
    assert!(turn(&backend, text("one")).await.0.is_ok());
    let (result, frames) = turn(&backend, text("two")).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        frames,
        vec![json!({ "type": "append", "role": "agent", "text": "ok" })]
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "the refused request was sent once more");
    assert!(
        requests[1].messages[1]
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. })),
        "the first attempt replayed the reasoning"
    );
    assert!(
        requests[2].messages[1]
            .blocks
            .iter()
            .all(|b| !matches!(b, Block::Thinking { .. } | Block::Opaque { .. })),
        "the retry carried none"
    );
    assert_eq!(
        requests[2].messages[1].blocks,
        vec![Block::Text { text: "Hi".into() }]
    );
    // The transcript keeps the thought the browser was shown.
    assert_eq!(
        history_texts(&backend).await,
        vec![
            "user:one",
            "thought:hmm",
            "agent:Hi",
            "user:two",
            "agent:ok"
        ]
    );
}

#[tokio::test]
async fn a_second_stale_refusal_is_reported_and_not_retried_again() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        thinking_turn(),
        ScriptedStream::StaleReasoning,
        ScriptedStream::StaleReasoning,
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    assert!(turn(&backend, text("one")).await.0.is_ok());
    let (result, _) = turn(&backend, text("two")).await;
    let message = result.unwrap_err().to_string();
    assert!(message.contains("Invalid `signature`"), "{message}");
    assert_eq!(provider.request_count(), 3, "one retry, not two");
    assert!(!backend.has_open_exchange_for_test());
    // Not a rejection: the message rides into the next request beside
    // the new one.
    assert!(turn(&backend, text("three")).await.0.is_ok());
    assert_eq!(
        provider.requests()[3].roles,
        vec![Role::User, Role::Assistant, Role::User, Role::User]
    );
}

#[tokio::test]
async fn a_refusal_stop_followed_by_a_cancel_still_rejects_the_exchange() {
    // The stop arrives, the counts do not yet, and the user clicks Stop.
    // The stop reason decides: the exchange is rejected however the
    // stream ended after it, so the refused content is never resent.
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        ScriptedStream::Pending {
            first: vec![
                TurnEvent::TextDelta("I cannot".into()),
                TurnEvent::Stop(StopReason::Refusal),
            ],
        },
        events(vec![end_turn()]),
    ]));
    let backend = Arc::new(backend(&provider));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let running = Arc::clone(&backend);
    let handle = tokio::spawn(async move { running.prompt(text("bad"), tx).await });
    // Both scripted events have been applied once the append is out.
    let first = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["type"], "append");
    tokio::time::sleep(Duration::from_millis(20)).await;
    backend.cancel().await;
    let result = handle.await.unwrap();
    assert!(
        result.is_ok(),
        "a cancel resolves without an error: {result:?}"
    );
    assert!(!backend.has_open_exchange_for_test());
    assert!(turn(&backend, text("next")).await.0.is_ok());
    let requests = provider.requests();
    assert_eq!(requests[1].roles, vec![Role::User]);
    assert_eq!(
        requests[1].messages[0].blocks,
        vec![Block::Text {
            text: "next".into()
        }]
    );
}

#[tokio::test]
async fn the_system_prompt_is_byte_identical_across_the_turns_of_a_conversation() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        events(vec![TurnEvent::TextDelta("a".into()), end_turn()]),
        events(vec![TurnEvent::TextDelta("b".into()), end_turn()]),
    ]));
    let backend = backend(&provider);
    assert!(turn(&backend, text("one")).await.0.is_ok());
    assert!(turn(&backend, text("two")).await.0.is_ok());
    let requests = provider.requests();
    assert_eq!(requests[0].system_static, requests[1].system_static);
    assert_eq!(requests[0].date_line, requests[1].date_line);
    assert!(requests[0].date_line.starts_with("Today's date is "));
}

#[tokio::test]
async fn an_opaque_block_streams_no_frame_and_is_replayed_in_order() {
    let raw = json!({ "redactedContent": "AQID" });
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        events(vec![
            TurnEvent::MessageStart {
                model: SONNET.into(),
            },
            TurnEvent::ThinkingStart { id: "0".into() },
            TurnEvent::ThinkingDelta {
                id: "0".into(),
                text: "t".into(),
            },
            TurnEvent::ThinkingEnd {
                id: "0".into(),
                signature: Some("sig".into()),
            },
            TurnEvent::OpaqueBlock { raw: raw.clone() },
            TurnEvent::TextDelta("Hi".into()),
            end_turn(),
            usage(),
        ]),
        events(vec![end_turn()]),
    ]));
    let backend = backend(&provider);
    let (result, frames) = turn(&backend, text("q")).await;
    assert!(result.is_ok());
    assert_eq!(
        frames,
        vec![
            json!({ "type": "thought", "text": "t" }),
            json!({ "type": "append", "role": "agent", "text": "Hi" }),
        ],
        "the opaque block reaches no browser"
    );
    assert!(turn(&backend, text("again")).await.0.is_ok());
    let blocks = &provider.requests()[1].messages[1].blocks;
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[0], Block::Thinking { .. }));
    assert_eq!(
        blocks[1],
        Block::Opaque {
            provider: "bedrock".into(),
            model: SONNET.into(),
            raw
        }
    );
    assert!(matches!(&blocks[2], Block::Text { text } if text == "Hi"));
}

#[tokio::test]
async fn an_attachment_reaches_the_request_but_not_the_user_entry() {
    let provider = Arc::new(ScriptedProvider::with_stream(events(vec![end_turn()])));
    let backend = backend(&provider);
    let blocks = vec![
        json!({ "type": "text", "text": "summarise" }),
        json!({ "type": "resource", "resource": { "uri": "file:///notes.txt", "mimeType": "text/plain", "text": "body" } }),
    ];
    assert!(turn(&backend, blocks).await.0.is_ok());
    assert_eq!(history_texts(&backend).await, vec!["user:summarise"]);
    let sent = &provider.requests()[0].messages[0].blocks;
    assert_eq!(sent.len(), 2);
    assert!(matches!(&sent[1], Block::Text { text } if text.contains("body")));
}

#[tokio::test]
async fn the_limit_check_counts_an_earlier_unanswered_message() {
    // Twelve images fail for a transient reason and stay for the next
    // request; twelve more would make twenty-four in one merged message,
    // over the limit of twenty. Refused locally: no request, nothing
    // recorded.
    let provider = Arc::new(ScriptedProvider::with_stream(
        ScriptedStream::BeforeStream {
            retryable: true,
            rejected: false,
            message: "throttled".into(),
        },
    ));
    let backend = backend(&provider);
    let twelve = |label: &str| {
        let mut blocks = vec![json!({ "type": "text", "text": label })];
        blocks.extend((0..12).map(|_| image("AQ==")));
        blocks
    };
    assert!(turn(&backend, twelve("first")).await.0.is_err());
    let (result, _) = turn(&backend, twelve("second")).await;
    let message = result.unwrap_err().to_string();
    assert!(message.to_lowercase().contains("limit"), "{message}");
    assert_eq!(
        provider.request_count(),
        1,
        "the second was refused before any request"
    );
    assert_eq!(history_texts(&backend).await, vec!["user:first"]);
}

#[test]
fn the_idle_timeout_is_five_minutes() {
    assert_eq!(mezame::turn::IDLE_TIMEOUT, Duration::from_secs(300));
}

#[tokio::test]
async fn history_timestamps_never_decrease() {
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        thinking_turn(),
        events(vec![TurnEvent::TextDelta("b".into()), end_turn()]),
    ]));
    let backend = backend(&provider);
    assert!(turn(&backend, text("one")).await.0.is_ok());
    assert!(turn(&backend, text("two")).await.0.is_ok());
    let history = backend.history().await;
    assert_eq!(history.len(), 5);
    for pair in history.windows(2) {
        assert!(pair[0].timestamp <= pair[1].timestamp, "{history:?}");
    }
    // A turn's entries share one stamp, at or after the user's.
    assert_eq!(history[1].timestamp, history[2].timestamp);
    assert!(history[0].timestamp <= history[1].timestamp);
}
