//! Persistence: what the loop writes to the store, what a conversation
//! rebuilt from the rows holds, and `GET /history` served from the store.
//!
//! Every case runs over an in-memory store and a `ScriptedProvider`; the
//! route cases build the server state the way the suite's other HTTP
//! cases do and go through the real router.

mod support;

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mezame::backend::{Backend, EntryBody, HistoryEntry, TurnOutcome};
use mezame::config::{Config, TransportConfig};
use mezame::conversation::{Block, Conversation, ExchangeStatus, Role};
use mezame::history::entries_from_rows;
use mezame::http::{build_router, AppState};
use mezame::hub::{HubCommand, HubRegistry, OwnerContext};
use mezame::persistent_factory;
use mezame::provider::{LoopSettings, Provider, StopReason, TurnEvent, Usage};
use mezame::store::crypto::MasterKey;
use mezame::store::sqlite::SqliteStore;
use mezame::store::{MessageRole, MessageRow, Role as UserRole, Store};
use mezame::turn::{LoopBackend, REFUSAL_ERROR};
use serde_json::{json, Value};
use support::{CountingStore, FailingStore, ScriptedProvider, ScriptedStream};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use tower::ServiceExt;

const SONNET: &str = "anthropic.claude-sonnet-5";

fn memory_store() -> Arc<SqliteStore> {
    Arc::new(
        SqliteStore::open_in_memory(MasterKey::from_bytes_for_test([7u8; 32]).keys())
            .expect("an in-memory store opens"),
    )
}

/// A user `name` and a session `session_id` of theirs; the user's id.
async fn seed(store: &dyn Store, name: &str, session_id: &str) -> String {
    let user = match store.user_by_name(name).await.unwrap() {
        Some(user) => user,
        None => store
            .create_user(name, "$argon2id$stub", UserRole::User, 1_000)
            .await
            .unwrap(),
    };
    store
        .create_session(&user.id, session_id, None, 1_000)
        .await
        .unwrap();
    user.id
}

fn settings() -> LoopSettings {
    LoopSettings {
        model: SONNET.to_string(),
        models: vec![SONNET.to_string()],
        thinking: None,
        thinking_budget: 4096,
        max_output_tokens: 16384,
    }
}

fn backend(
    provider: &Arc<ScriptedProvider>,
    store: Option<Arc<dyn Store>>,
    session_id: &str,
) -> LoopBackend {
    LoopBackend::new(
        Arc::clone(provider) as Arc<dyn Provider>,
        settings(),
        session_id,
        "alice",
        store,
    )
}

fn text(text: &str) -> Vec<Value> {
    vec![json!({ "type": "text", "text": text })]
}

fn events(list: Vec<TurnEvent>) -> ScriptedStream {
    ScriptedStream::Events(list)
}

fn end_turn() -> TurnEvent {
    TurnEvent::Stop(StopReason::EndTurn)
}

fn usage() -> Usage {
    Usage {
        input: 17,
        output: 700,
        cache_read: 1370,
        cache_write: 0,
    }
}

/// A reply of one signed thinking block then `reply`, ended normally with
/// the counts.
fn thoughtful_reply(thought: &str, reply: &str) -> ScriptedStream {
    events(vec![
        TurnEvent::ThinkingStart { id: "0".into() },
        TurnEvent::ThinkingDelta {
            id: "0".into(),
            text: thought.into(),
        },
        TurnEvent::ThinkingEnd {
            id: "0".into(),
            signature: Some("sig".into()),
        },
        TurnEvent::TextDelta(reply.into()),
        end_turn(),
        TurnEvent::Usage(usage()),
    ])
}

fn plain_reply(reply: &str) -> ScriptedStream {
    events(vec![
        TurnEvent::TextDelta(reply.into()),
        end_turn(),
        TurnEvent::Usage(usage()),
    ])
}

fn thinking(text: &str) -> Block {
    Block::Thinking {
        text: text.to_string(),
        signature: Some("sig".to_string()),
        provider: "bedrock".to_string(),
        model: SONNET.to_string(),
    }
}

fn text_block(text: &str) -> Block {
    Block::Text {
        text: text.to_string(),
    }
}

async fn turn(backend: &LoopBackend, blocks: Vec<Value>) -> anyhow::Result<TurnOutcome> {
    let (tx, _rx) = mpsc::unbounded_channel();
    backend.prompt(blocks, tx).await
}

fn entry_texts(entries: &[HistoryEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| match &entry.body {
            EntryBody::User { text } => format!("user:{text}"),
            EntryBody::Agent { text } => format!("agent:{text}"),
            EntryBody::Thought { text } => format!("thought:{text}"),
            other => format!("{other:?}"),
        })
        .collect()
}

async fn rows(store: &dyn Store, session_id: &str) -> Vec<MessageRow> {
    store
        .load_window(session_id, 1_000, 1 << 24)
        .await
        .unwrap()
        .rows
}

fn config() -> Config {
    Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".to_string(),
            hosts: vec![],
        }],
        version: 2,
        datastore: Default::default(),
        public_url: None,
        models: vec![],
        bedrock: None,
    }
}

/// A registry whose hubs run the loop over `provider` and write to `store`:
/// the production factory over the scripted provider.
fn persistent_registry(provider: &Arc<ScriptedProvider>, store: Arc<dyn Store>) -> HubRegistry {
    HubRegistry::with_factory(persistent_factory(
        Arc::clone(provider) as Arc<dyn Provider>,
        settings(),
        store,
    ))
}

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

/// Attach to `session_id` as `owner` on `hubs`, send `blocks` as one
/// prompt and wait for it to end. Returns the attach, held so the hub
/// stays up, and every frame seen.
async fn prompt_through(
    hubs: &HubRegistry,
    session_id: &str,
    owner: &OwnerContext,
    blocks: Vec<Value>,
) -> (mezame::hub::AttachedHub, Vec<Value>) {
    let attached = hubs
        .attach_or_create(session_id, owner, None)
        .await
        .expect("the attach succeeds");
    let mut rx = attached.outbound.resubscribe();
    attached
        .commands
        .send(HubCommand::Prompt {
            blocks,
            attach_id: attached.attach_id,
        })
        .await
        .expect("send Prompt");
    let frames = collect_until(&mut rx, "prompt_done").await;
    assert_eq!(
        frames.last().map(|f| f["type"].clone()),
        Some(json!("prompt_done")),
        "the turn ended: {frames:?}"
    );
    (attached, frames)
}

async fn get_history(state: &Arc<AppState>, cookie: &str, session_id: &str) -> (StatusCode, Value) {
    let res = build_router(state.clone())
        .oneshot(
            Request::get(format!("/history?session={session_id}"))
                .header(axum::http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router did not respond");
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).expect("a JSON body"))
}

// ---------- the writes ----------

#[tokio::test]
async fn a_completed_turn_writes_the_user_row_then_the_assistant_row() {
    // Requirement 8 criterion 1: the user row carries the blocks, the
    // entry text and the timestamp; the assistant row the kept blocks
    // (the signed thinking block and the text) and the four counts.
    let store = memory_store();
    seed(store.as_ref(), "alice", "s1").await;
    let provider = Arc::new(ScriptedProvider::with_stream(thoughtful_reply(
        "Let me see",
        "hello there",
    )));
    let backend = backend(&provider, Some(store.clone()), "s1");

    let outcome = turn(&backend, text("hi")).await.expect("the turn resolves");
    assert_eq!(outcome.usage, Some(usage()));

    let rows = rows(store.as_ref(), "s1").await;
    assert_eq!(rows.len(), 2);
    let user = &rows[0];
    assert_eq!(user.role, MessageRole::User);
    assert_eq!(user.blocks, vec![text_block("hi")]);
    assert_eq!(user.text.as_deref(), Some("hi"));
    assert_eq!(user.usage, None);
    assert!(!user.rejected);
    let assistant = &rows[1];
    assert_eq!(assistant.role, MessageRole::Assistant);
    assert_eq!(
        assistant.blocks,
        vec![thinking("Let me see"), text_block("hello there")]
    );
    assert_eq!(assistant.text, None);
    assert_eq!(assistant.usage, Some(usage()));
    assert!(!assistant.rejected);
    assert!(user.id < assistant.id);
    assert!(user.created <= assistant.created);
}

#[tokio::test]
async fn a_refused_reply_writes_a_flagged_row_and_flags_the_unanswered_run() {
    // Requirement 8 criterion 1, the rejection: a request that failed
    // before any output leaves a user row with no reply; the refusal on
    // the next turn was of both user messages merged, so both rows are
    // flagged, and the refused reply's text is a flagged assistant row.
    let store = memory_store();
    seed(store.as_ref(), "alice", "s1").await;
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        ScriptedStream::BeforeStream {
            retryable: false,
            rejected: false,
            message: "no credentials".into(),
        },
        events(vec![
            TurnEvent::TextDelta("I cannot help with that".into()),
            TurnEvent::Stop(StopReason::Refusal),
            TurnEvent::Usage(usage()),
        ]),
        plain_reply("sure"),
    ]));
    let backend = backend(&provider, Some(store.clone()), "s1");

    assert!(turn(&backend, text("first")).await.is_err());
    let refused = turn(&backend, text("second")).await;
    assert_eq!(refused.unwrap_err().to_string(), REFUSAL_ERROR);

    let rows_after = rows(store.as_ref(), "s1").await;
    assert_eq!(rows_after.len(), 3);
    assert_eq!(rows_after[0].text.as_deref(), Some("first"));
    assert!(rows_after[0].rejected, "the unanswered row is flagged");
    assert_eq!(rows_after[1].text.as_deref(), Some("second"));
    assert!(rows_after[1].rejected);
    assert_eq!(rows_after[2].role, MessageRole::Assistant);
    assert_eq!(
        rows_after[2].blocks,
        vec![text_block("I cannot help with that")]
    );
    assert!(rows_after[2].rejected, "the refused reply is flagged");
    assert_eq!(rows_after[2].usage, Some(usage()));

    // The next turn carries none of it, and its rows are not flagged.
    assert!(turn(&backend, text("third")).await.is_ok());
    let request = &provider.requests()[2];
    assert_eq!(request.roles, vec![Role::User]);
    assert_eq!(request.messages[0].text(), "third");
    let rows_after = rows(store.as_ref(), "s1").await;
    assert_eq!(rows_after.len(), 5);
    assert!(!rows_after[3].rejected);
    assert!(!rows_after[4].rejected);
    assert_eq!(rows_after[4].blocks, vec![text_block("sure")]);
}

#[tokio::test]
async fn a_rejection_before_any_output_flags_the_user_row_and_writes_no_reply() {
    let store = memory_store();
    seed(store.as_ref(), "alice", "s1").await;
    let provider = Arc::new(ScriptedProvider::with_stream(
        ScriptedStream::BeforeStream {
            retryable: false,
            rejected: true,
            message: "refused".into(),
        },
    ));
    let backend = backend(&provider, Some(store.clone()), "s1");

    assert!(turn(&backend, text("hi")).await.is_err());

    let rows = rows(store.as_ref(), "s1").await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].rejected);
    assert_eq!(rows[0].role, MessageRole::User);
}

#[tokio::test]
async fn a_reply_with_no_text_writes_no_assistant_row() {
    // The kept-blocks rule: a reply of reasoning alone is not a message
    // the next request can carry, so nothing is written for it, and a
    // rebuild sees a user row with no reply.
    let store = memory_store();
    seed(store.as_ref(), "alice", "s1").await;
    let provider = Arc::new(ScriptedProvider::with_stream(events(vec![
        TurnEvent::ThinkingStart { id: "0".into() },
        TurnEvent::ThinkingDelta {
            id: "0".into(),
            text: "only thoughts".into(),
        },
        TurnEvent::ThinkingEnd {
            id: "0".into(),
            signature: Some("sig".into()),
        },
        end_turn(),
        TurnEvent::Usage(usage()),
    ])));
    let backend = backend(&provider, Some(store.clone()), "s1");

    assert!(turn(&backend, text("hi")).await.is_ok());

    let rows = rows(store.as_ref(), "s1").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, MessageRole::User);
    assert!(!rows[0].rejected);
}

#[tokio::test]
async fn a_failing_store_is_reported_once_and_every_turn_still_resolves() {
    // Requirement 8 criterion 2: the write fails, the turn resolves as if
    // it had not, one line is written for the session, and the next turn
    // runs on the in-memory conversation.
    let inner = memory_store();
    seed(inner.as_ref(), "alice", "s1").await;
    let failing = Arc::new(FailingStore::new(inner.clone()));
    failing.fail();
    let provider = Arc::new(ScriptedProvider::with_streams(vec![
        plain_reply("one"),
        plain_reply("two"),
        plain_reply("three"),
    ]));
    let backend = backend(&provider, Some(failing.clone() as Arc<dyn Store>), "s1");

    assert!(turn(&backend, text("a")).await.is_ok());
    assert_eq!(
        backend.store_failures_for_test(),
        2,
        "the user and the assistant write"
    );
    assert_eq!(backend.store_failure_lines_for_test(), 1);

    assert!(turn(&backend, text("b")).await.is_ok());
    assert_eq!(backend.store_failures_for_test(), 4);
    assert_eq!(backend.store_failure_lines_for_test(), 1, "no second line");
    assert_eq!(
        entry_texts(&backend.history().await),
        vec!["user:a", "agent:one", "user:b", "agent:two"]
    );
    assert_eq!(
        provider.requests()[1].roles,
        vec![Role::User, Role::Assistant, Role::User],
        "the second request carried the first exchange"
    );
    assert!(rows(inner.as_ref(), "s1").await.is_empty());

    // Once the store answers again, the rows of later turns land.
    failing.recover();
    assert!(turn(&backend, text("c")).await.is_ok());
    assert_eq!(backend.store_failures_for_test(), 4);
    let rows = rows(inner.as_ref(), "s1").await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].text.as_deref(), Some("c"));
    assert_eq!(rows[1].blocks, vec![text_block("three")]);
}

// ---------- the rebuild ----------

/// Rows for one session: a replied exchange with an attachment and a
/// thought, a refused exchange with the refused text, a refusal before
/// any output, a request that got no reply, and a plain replied exchange.
/// Returns the ids of the rows in the order written.
async fn write_mixed_rows(store: &dyn Store, session_id: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    let u1 = vec![
        text_block("q1"),
        Block::Image {
            media_type: "image/png".into(),
            data: vec![1, 2, 3, 4, 5],
        },
    ];
    ids.push(store.append_user(session_id, &u1, "q1", 1).await.unwrap());
    let a1 = vec![thinking("t1"), text_block("a1")];
    ids.push(
        store
            .append_assistant(session_id, &a1, Some(usage()), false, 2)
            .await
            .unwrap(),
    );
    let u2 = store
        .append_user(session_id, &[text_block("q2")], "q2", 3)
        .await
        .unwrap();
    ids.push(u2);
    let a2 = store
        .append_assistant(session_id, &[text_block("no")], None, true, 4)
        .await
        .unwrap();
    ids.push(a2);
    let u3 = store
        .append_user(session_id, &[text_block("q3")], "q3", 5)
        .await
        .unwrap();
    ids.push(u3);
    store.mark_rejected(&[u2, u3]).await.unwrap();
    ids.push(
        store
            .append_user(session_id, &[text_block("q4")], "q4", 6)
            .await
            .unwrap(),
    );
    ids.push(
        store
            .append_user(session_id, &[text_block("q5")], "q5", 7)
            .await
            .unwrap(),
    );
    ids.push(
        store
            .append_assistant(session_id, &[text_block("a5")], Some(usage()), false, 8)
            .await
            .unwrap(),
    );
    ids
}

#[tokio::test]
async fn restore_rebuilds_the_statuses_the_entries_and_the_payload() {
    // Requirement 8 criterion 3: every row shape the loop writes, rebuilt.
    let store = memory_store();
    seed(store.as_ref(), "alice", "s1").await;
    let ids = write_mixed_rows(store.as_ref(), "s1").await;
    let window = store.load_window("s1", 1_000, 1 << 24).await.unwrap();
    assert!(window.complete);

    let mut conversation = Conversation::new();
    conversation.restore(window);

    let exchanges: Vec<_> = conversation.exchanges().collect();
    assert_eq!(
        exchanges.iter().map(|e| e.status).collect::<Vec<_>>(),
        vec![
            ExchangeStatus::Closed,
            ExchangeStatus::Rejected,
            ExchangeStatus::Rejected,
            ExchangeStatus::Closed,
            ExchangeStatus::Closed,
        ]
    );
    assert_eq!(
        exchanges.iter().map(|e| e.user_row()).collect::<Vec<_>>(),
        vec![
            Some(ids[0]),
            Some(ids[2]),
            Some(ids[4]),
            Some(ids[5]),
            Some(ids[6])
        ],
        "each exchange names its user row"
    );
    // The replied exchange keeps its text and not its reasoning; the
    // rejected pair holds no message; the unanswered one has no reply.
    assert_eq!(
        exchanges[0].assistant.as_ref().map(|m| m.blocks.clone()),
        Some(vec![text_block("a1")])
    );
    assert!(exchanges[1].assistant.is_none());
    assert!(exchanges[2].assistant.is_none());
    assert!(exchanges[3].assistant.is_none());
    assert_eq!(
        exchanges[4].assistant.as_ref().map(|m| m.blocks.clone()),
        Some(vec![text_block("a5")])
    );

    let messages: Vec<String> = conversation
        .messages()
        .iter()
        .map(|m| format!("{:?}:{}", m.role, m.text()))
        .collect();
    assert_eq!(
        messages,
        vec![
            "User:q1",
            "Assistant:a1",
            "User:q4",
            "User:q5",
            "Assistant:a5"
        ],
        "the rejected pair and the lone rejection are absent"
    );

    let history = conversation.history();
    assert_eq!(
        entry_texts(&history),
        vec![
            "user:q1",
            "thought:t1",
            "agent:a1",
            "user:q2",
            "agent:no",
            "user:q3",
            "user:q4",
            "user:q5",
            "agent:a5"
        ],
        "the transcript keeps the refused reply's text"
    );
    assert_eq!(history[2].usage, Some(usage()));
    assert_eq!(history[4].usage, None);
    assert_eq!(history[0].timestamp, 1);
    assert_eq!(history[8].timestamp, 8);

    // The budget figure: every entry's text plus the image's bytes; the
    // dropped signature counts for nothing.
    let text_bytes: usize = history
        .iter()
        .map(|entry| match &entry.body {
            EntryBody::User { text } | EntryBody::Agent { text } | EntryBody::Thought { text } => {
                text.len()
            }
            _ => 0,
        })
        .sum();
    assert_eq!(conversation.bytes(), text_bytes + 5);
}

#[tokio::test]
async fn restore_strips_reasoning_with_and_without_a_cut_and_the_cut_keeps_the_newest() {
    let store = memory_store();
    seed(store.as_ref(), "alice", "s1").await;
    for i in 1..=3 {
        store
            .append_user(
                "s1",
                &[text_block(&format!("q{i}"))],
                &format!("q{i}"),
                i * 2,
            )
            .await
            .unwrap();
        let reply = vec![
            thinking(&format!("t{i}")),
            Block::Opaque {
                provider: "bedrock".into(),
                model: SONNET.into(),
                raw: json!({ "kind": "redacted", "n": i }),
            },
            text_block(&format!("a{i}")),
        ];
        store
            .append_assistant("s1", &reply, Some(usage()), false, i * 2 + 1)
            .await
            .unwrap();
    }
    let window = store.load_window("s1", 1_000, 1 << 24).await.unwrap();

    let no_reasoning = |conversation: &Conversation| {
        conversation.messages().iter().all(|m| {
            m.blocks
                .iter()
                .all(|b| !matches!(b, Block::Thinking { .. } | Block::Opaque { .. }))
        })
    };

    // Everything fits: three exchanges, no reasoning, the thoughts kept.
    let mut whole = Conversation::new();
    whole.restore(window.clone());
    assert_eq!(whole.exchange_count(), 3);
    assert!(no_reasoning(&whole), "a whole restore replays no reasoning");
    assert_eq!(
        entry_texts(&whole.history()),
        vec![
            "user:q1",
            "thought:t1",
            "agent:a1",
            "user:q2",
            "thought:t2",
            "agent:a2",
            "user:q3",
            "thought:t3",
            "agent:a3"
        ]
    );

    // Five entries at most: each exchange is three, so the second evicts
    // the first and the third evicts the second, and the newest stays.
    let mut cut = Conversation::with_budget_for_test(1 << 20, 5);
    cut.restore(window);
    assert_eq!(cut.exchange_count(), 1);
    assert!(no_reasoning(&cut), "a cut restore replays no reasoning");
    assert_eq!(
        entry_texts(&cut.history()),
        vec!["user:q3", "thought:t3", "agent:a3"],
        "the newest exchange is the one kept"
    );
    assert_eq!(cut.messages()[0].text(), "q3");
}

#[tokio::test]
async fn a_window_cut_between_a_pair_skips_the_orphaned_reply() {
    let store = memory_store();
    seed(store.as_ref(), "alice", "s1").await;
    for i in 1..=3 {
        store
            .append_user("s1", &[text_block(&format!("q{i}"))], &format!("q{i}"), i)
            .await
            .unwrap();
        store
            .append_assistant("s1", &[text_block(&format!("a{i}"))], None, false, i)
            .await
            .unwrap();
    }
    // Three rows: the reply of exchange two, then exchange three whole.
    let window = store.load_window("s1", 3, 1 << 24).await.unwrap();
    assert!(!window.complete);
    assert_eq!(window.rows.len(), 3);
    assert_eq!(window.rows[0].role, MessageRole::Assistant);

    let mut conversation = Conversation::new();
    conversation.restore(window);
    assert_eq!(conversation.exchange_count(), 1);
    assert_eq!(
        entry_texts(&conversation.history()),
        vec!["user:q3", "agent:a3"],
        "a reply whose question is outside the window is not served alone"
    );
}

#[tokio::test]
async fn restore_serves_the_newest_rows_within_the_bound_and_none_older() {
    // Requirement 8 criterion 7, the load bound: the rows the query kept
    // are the newest ones inside it, named by their ids.
    let store = memory_store();
    seed(store.as_ref(), "alice", "s1").await;
    let mut user_ids = Vec::new();
    for i in 1..=10 {
        user_ids.push(
            store
                .append_user("s1", &[text_block(&format!("q{i}"))], &format!("q{i}"), i)
                .await
                .unwrap(),
        );
        store
            .append_assistant("s1", &[text_block(&format!("a{i}"))], None, false, i)
            .await
            .unwrap();
    }
    let window = store.load_window("s1", 6, 1 << 24).await.unwrap();
    assert!(!window.complete);

    let mut conversation = Conversation::new();
    conversation.restore(window);
    let kept: Vec<Option<i64>> = conversation.exchanges().map(|e| e.user_row()).collect();
    assert_eq!(
        kept,
        vec![Some(user_ids[7]), Some(user_ids[8]), Some(user_ids[9])],
        "the three newest exchanges, by their user rows"
    );
    assert_eq!(
        entry_texts(&conversation.history()),
        vec![
            "user:q8",
            "agent:a8",
            "user:q9",
            "agent:a9",
            "user:q10",
            "agent:a10"
        ]
    );
}

// ---------- the mapping ----------

fn row(id: i64, role: MessageRole, blocks: Vec<Block>, text: Option<&str>) -> MessageRow {
    MessageRow {
        id,
        session_id: "s1".to_string(),
        role,
        blocks,
        text: text.map(str::to_string),
        usage: None,
        rejected: false,
        created: id * 10,
    }
}

#[test]
fn entries_from_rows_maps_each_row_shape_and_carries_usage_on_agent_entries_alone() {
    // Requirement 8 criterion 4: the mapping, with and without usage.
    // A user row never holds counts; one that did would still map to an
    // entry without them, so the browser never shows a footer under a
    // prompt.
    let rows = vec![
        MessageRow {
            usage: Some(usage()),
            ..row(
                1,
                MessageRole::User,
                vec![
                    text_block("summarise"),
                    text_block("Attached file notes.txt (text/plain):\nthe body"),
                ],
                Some("summarise"),
            )
        },
        MessageRow {
            usage: Some(usage()),
            ..row(
                2,
                MessageRole::Assistant,
                vec![thinking("first"), thinking(""), text_block("done")],
                None,
            )
        },
        MessageRow {
            rejected: true,
            ..row(3, MessageRole::Assistant, vec![text_block("no")], None)
        },
        row(4, MessageRole::Assistant, vec![thinking("quiet")], None),
    ];

    let entries = entries_from_rows(&rows);
    assert_eq!(
        entry_texts(&entries),
        vec![
            "user:summarise",
            "thought:first",
            "agent:done",
            "agent:no",
            "thought:quiet"
        ],
        "the entry text is the row's text, an empty thought is dropped, a \
         flagged row maps the same, a reply with no text has no agent entry"
    );
    assert_eq!(
        serde_json::to_value(&entries[0]).unwrap(),
        json!({ "role": "user", "text": "summarise", "timestamp": 10 }),
        "no usage key on a user entry"
    );
    assert_eq!(
        serde_json::to_value(&entries[2]).unwrap(),
        json!({
            "role": "agent",
            "text": "done",
            "timestamp": 20,
            "usage": { "input": 17, "output": 700, "cacheRead": 1370, "cacheWrite": 0 }
        })
    );
    assert_eq!(
        serde_json::to_value(&entries[3]).unwrap(),
        json!({ "role": "agent", "text": "no", "timestamp": 30 }),
        "no usage key when the row holds no counts"
    );
    assert!(entries
        .iter()
        .all(|e| !matches!(e.body, EntryBody::Sys { .. })));
}

// ---------- through the hub and the route ----------

#[tokio::test]
async fn a_resource_prompt_is_restored_and_served_with_the_typed_text_alone() {
    // Requirement 8 criterion 7, the attachment: the request carried the
    // file's text, the row records the typed text, and `/history` served
    // from the store shows the typed text alone, as phase 1 did live.
    let store = memory_store();
    let provider = Arc::new(ScriptedProvider::with_stream(plain_reply("a summary")));
    let hubs = persistent_registry(&provider, store.clone());
    let state = AppState::for_test_with(config(), hubs, 8, Some(store.clone()), None);
    let cookie = state.login_for_test("alice", "correct horse battery").await;
    let alice = state.store.user_by_name("alice").await.unwrap().unwrap();
    state
        .store
        .create_session(&alice.id, "res-session", None, 1_000)
        .await
        .unwrap();
    let owner = OwnerContext {
        user_id: alice.id.clone(),
        user_name: "alice".to_string(),
    };

    let blocks = vec![
        json!({ "type": "text", "text": "summarise" }),
        json!({
            "type": "resource",
            "resource": {
                "uri": "file:///notes.txt",
                "mimeType": "text/plain",
                "text": "the body of the file"
            }
        }),
    ];
    let (_attached, frames) = prompt_through(&state.hubs, "res-session", &owner, blocks).await;
    assert!(frames
        .iter()
        .any(|f| f["type"] == "append" && f["text"] == "a summary"));

    // The request carried the file's text; the row records the typed text.
    let sent = &provider.requests()[0].messages[0];
    assert!(sent.text().contains("the body of the file"));
    let rows = rows(store.as_ref(), "res-session").await;
    assert_eq!(rows[0].text.as_deref(), Some("summarise"));
    assert_eq!(rows[0].blocks.len(), 2, "both text blocks are stored");

    let (status, body) = get_history(&state, &cookie, "res-session").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["entries"],
        json!([
            { "role": "user", "text": "summarise", "timestamp": rows[0].created },
            {
                "role": "agent",
                "text": "a summary",
                "timestamp": rows[1].created,
                "usage": { "input": 17, "output": 700, "cacheRead": 1370, "cacheWrite": 0 }
            }
        ])
    );

    // Served from the store: a second state over the same store, with no
    // hub for the id, answers the same.
    let elsewhere = AppState::for_test_with(
        config(),
        persistent_registry(&Arc::new(ScriptedProvider::new()), store.clone()),
        8,
        Some(store.clone()),
        None,
    );
    let (status, again) = get_history(&elsewhere, &cookie, "res-session").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, body);
}

#[tokio::test]
async fn a_second_registry_over_the_same_store_serves_the_history_and_replays_the_conversation() {
    // Requirement 8 criterion 6, the restart: the first registry's hub
    // wrote the rows; a second registry, built after the first is gone,
    // rebuilds the conversation from them, serves it and sends it.
    let inner = memory_store();
    let counting = Arc::new(CountingStore::new(inner.clone()));
    let store: Arc<dyn Store> = counting.clone();
    let alice = seed(store.as_ref(), "alice", "again").await;
    let owner = OwnerContext {
        user_id: alice,
        user_name: "alice".to_string(),
    };

    let first = Arc::new(ScriptedProvider::with_stream(thoughtful_reply(
        "pondering",
        "first reply",
    )));
    {
        let hubs = persistent_registry(&first, store.clone());
        let (attached, _) = prompt_through(&hubs, "again", &owner, text("one")).await;
        assert_eq!(
            counting.loads(),
            1,
            "the first build loaded the empty session"
        );
        drop(attached);
        drop(hubs);
    }

    let second = Arc::new(ScriptedProvider::with_stream(plain_reply("second reply")));
    let hubs = persistent_registry(&second, store.clone());
    let attached = hubs
        .attach_or_create("again", &owner, None)
        .await
        .expect("the attach succeeds");
    assert_eq!(counting.loads(), 2, "the second build loaded the rows once");
    assert_eq!(
        entry_texts(&hubs.history("again").await.expect("a hub")),
        vec!["user:one", "thought:pondering", "agent:first reply"],
        "the rebuilt transcript, thoughts included"
    );

    let mut rx = attached.outbound.resubscribe();
    attached
        .commands
        .send(HubCommand::Prompt {
            blocks: text("two"),
            attach_id: attached.attach_id,
        })
        .await
        .unwrap();
    let frames = collect_until(&mut rx, "prompt_done").await;
    assert!(frames
        .iter()
        .any(|f| f["type"] == "append" && f["text"] == "second reply"));

    let request = &second.requests()[0];
    assert_eq!(
        request.roles,
        vec![Role::User, Role::Assistant, Role::User],
        "the rebuilt exchange rides ahead of the new prompt"
    );
    assert_eq!(request.messages[0].text(), "one");
    assert_eq!(
        request.messages[1].blocks,
        vec![text_block("first reply")],
        "the reply's text is replayed and its reasoning is not"
    );
    assert_eq!(request.messages[2].text(), "two");

    // The store now holds both exchanges, in order.
    let rows = rows(store.as_ref(), "again").await;
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[2].text.as_deref(), Some("two"));
    assert_eq!(rows[3].blocks, vec![text_block("second reply")]);
}

#[tokio::test]
async fn history_from_the_store_is_the_owner_s_alone() {
    let store = memory_store();
    let hubs = persistent_registry(&Arc::new(ScriptedProvider::new()), store.clone());
    let state = AppState::for_test_with(config(), hubs, 8, Some(store.clone()), None);
    let bob = state.login_for_test("bob", "correct horse battery").await;
    let alice = state.login_for_test("alice", "correct horse battery").await;
    let bob_row = state.store.user_by_name("bob").await.unwrap().unwrap();
    state
        .store
        .create_session(&bob_row.id, "bobs", None, 1_000)
        .await
        .unwrap();
    store
        .append_user("bobs", &[text_block("private")], "private", 5)
        .await
        .unwrap();

    let (status, body) = get_history(&state, &bob, "bobs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["entries"],
        json!([{ "role": "user", "text": "private", "timestamp": 5 }])
    );
    let (status, body) = get_history(&state, &alice, "bobs").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "entries": [] }));
}

#[tokio::test]
async fn an_echo_deployment_serves_history_from_the_hub_and_not_from_the_store() {
    // Requirement 8 criterion 5: with no persisting factory the phase 1
    // path stands. Rows in the store for the id are not what is served;
    // with no hub registered the answer is empty.
    let store = memory_store();
    let state = AppState::for_test_with(config(), HubRegistry::new(), 8, Some(store.clone()), None);
    assert!(!state.hubs.persists());
    let cookie = state.login_for_test("alice", "correct horse battery").await;
    let alice = state.store.user_by_name("alice").await.unwrap().unwrap();
    state
        .store
        .create_session(&alice.id, "echo-session", None, 1_000)
        .await
        .unwrap();
    store
        .append_user("echo-session", &[text_block("stored")], "stored", 5)
        .await
        .unwrap();

    let (status, body) = get_history(&state, &cookie, "echo-session").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "entries": [] }));

    // With a hub, the echo's transcript is what is served.
    let owner = OwnerContext {
        user_id: alice.id.clone(),
        user_name: "alice".to_string(),
    };
    let (_attached, _) = prompt_through(&state.hubs, "echo-session", &owner, text("ping")).await;
    let (status, body) = get_history(&state, &cookie, "echo-session").await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["text"], "ping");
    assert_eq!(entries[1]["role"], "agent");
    assert_eq!(
        rows(store.as_ref(), "echo-session").await.len(),
        1,
        "the echo wrote nothing"
    );
}

#[tokio::test]
async fn a_persisting_registry_reports_so_and_the_default_does_not() {
    let store = memory_store();
    assert!(persistent_registry(&Arc::new(ScriptedProvider::new()), store).persists());
    assert!(!HubRegistry::new().persists());
}
