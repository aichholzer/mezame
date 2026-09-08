//! The coupled stores: one exchange at a time, one budget for both.

use mezame::backend::{EntryBody, HistoryEntry};
use mezame::conversation::{Block, Conversation, DocumentFormat, ExchangeStatus, Message, Role};

fn entry(role: &str, text: &str, timestamp: i64) -> HistoryEntry {
    let body = match role {
        "user" => EntryBody::User {
            text: text.to_string(),
        },
        "agent" => EntryBody::Agent {
            text: text.to_string(),
        },
        "thought" => EntryBody::Thought {
            text: text.to_string(),
        },
        _ => unreachable!(),
    };
    HistoryEntry { body, timestamp }
}

fn text(text: &str) -> Block {
    Block::Text {
        text: text.to_string(),
    }
}

fn user(blocks: Vec<Block>) -> Message {
    Message {
        role: Role::User,
        blocks,
    }
}

fn assistant(blocks: Vec<Block>) -> Message {
    Message {
        role: Role::Assistant,
        blocks,
    }
}

fn texts(conversation: &Conversation) -> Vec<String> {
    conversation
        .messages()
        .iter()
        .map(|m| format!("{:?}:{}", m.role, m.text()))
        .collect()
}

fn history_texts(conversation: &Conversation) -> Vec<String> {
    conversation
        .history()
        .iter()
        .map(|e| match &e.body {
            EntryBody::User { text } => format!("user:{text}"),
            EntryBody::Agent { text } => format!("agent:{text}"),
            EntryBody::Thought { text } => format!("thought:{text}"),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn begin_opens_an_exchange_and_complete_closes_it_with_the_reply() {
    let mut conversation = Conversation::new();
    assert!(!conversation.has_open_exchange());
    assert!(conversation.last_timestamp().is_none());

    conversation.begin(user(vec![text("hello")]), entry("user", "hello", 10));
    assert!(conversation.has_open_exchange());
    assert_eq!(conversation.exchange_count(), 1);
    assert_eq!(texts(&conversation), vec!["User:hello"]);
    assert_eq!(conversation.last_timestamp(), Some(10));

    let closed = conversation.complete(
        Some(assistant(vec![text("hi")])),
        vec![entry("thought", "let me see", 11), entry("agent", "hi", 11)],
    );
    assert!(closed);
    assert!(!conversation.has_open_exchange());
    assert_eq!(texts(&conversation), vec!["User:hello", "Assistant:hi"]);
    assert_eq!(
        history_texts(&conversation),
        vec!["user:hello", "thought:let me see", "agent:hi"]
    );
    assert_eq!(conversation.last_timestamp(), Some(11));
    assert_eq!(
        conversation
            .exchanges()
            .map(|e| e.status)
            .collect::<Vec<_>>(),
        vec![ExchangeStatus::Closed]
    );
}

#[test]
fn complete_with_no_reply_closes_the_exchange_too() {
    let mut conversation = Conversation::new();
    conversation.begin(user(vec![text("q")]), entry("user", "q", 1));
    assert!(conversation.complete(None, Vec::new()));
    assert!(
        !conversation.has_open_exchange(),
        "a reply that never came is still an answer"
    );
    // Nothing is open, so nothing can be completed or rejected again.
    assert!(!conversation.complete(
        Some(assistant(vec![text("late")])),
        vec![entry("agent", "late", 2)]
    ));
    assert!(!conversation.reject_open(Vec::new()));
    assert_eq!(
        texts(&conversation),
        vec!["User:q"],
        "the user message rides into the next request"
    );
    assert_eq!(history_texts(&conversation), vec!["user:q"]);

    // The next exchange merges with it at request time; here it follows.
    conversation.begin(user(vec![text("q2")]), entry("user", "q2", 3));
    assert_eq!(texts(&conversation), vec!["User:q", "User:q2"]);
}

#[test]
fn a_rejected_exchange_stays_in_the_transcript_and_leaves_the_requests() {
    let mut conversation = Conversation::new();
    conversation.begin(user(vec![text("ok")]), entry("user", "ok", 1));
    conversation.complete(
        Some(assistant(vec![text("fine")])),
        vec![entry("agent", "fine", 2)],
    );
    conversation.begin(user(vec![text("refused")]), entry("user", "refused", 3));
    assert!(conversation.reject_open(vec![entry("agent", "partial", 4)]));
    assert!(!conversation.has_open_exchange());
    assert!(
        !conversation.reject_open(Vec::new()),
        "rejecting twice changes nothing"
    );
    assert!(
        !conversation.complete(None, Vec::new()),
        "a rejected exchange is not open"
    );
    assert_eq!(texts(&conversation), vec!["User:ok", "Assistant:fine"]);
    assert_eq!(
        history_texts(&conversation),
        vec!["user:ok", "agent:fine", "user:refused", "agent:partial"],
        "the browser saw the partial reply, so the transcript keeps it"
    );
    assert_eq!(
        conversation
            .exchanges()
            .map(|e| e.status)
            .collect::<Vec<_>>(),
        vec![ExchangeStatus::Closed, ExchangeStatus::Rejected]
    );
}

#[test]
fn complete_and_reject_on_an_empty_or_cleared_conversation_do_nothing() {
    let mut conversation = Conversation::new();
    assert!(!conversation.complete(
        Some(assistant(vec![text("x")])),
        vec![entry("agent", "x", 1)]
    ));
    assert!(!conversation.reject_open(Vec::new()));
    assert_eq!(conversation.exchange_count(), 0);
    assert!(conversation.history().is_empty());

    conversation.begin(user(vec![text("q")]), entry("user", "q", 1));
    conversation.clear();
    assert!(!conversation.complete(
        Some(assistant(vec![text("x")])),
        vec![entry("agent", "x", 2)]
    ));
    assert_eq!(conversation.exchange_count(), 0);
    assert!(conversation.history().is_empty());
    assert_eq!(conversation.bytes(), 0);

    // Begin after clear starts afresh.
    conversation.begin(user(vec![text("again")]), entry("user", "again", 3));
    assert_eq!(texts(&conversation), vec!["User:again"]);
}

#[test]
fn the_budget_counts_transcript_text_plus_the_payload_no_entry_carries() {
    let mut conversation = Conversation::new();
    // Typed text is in the entry; the attached file's text and the image
    // bytes are not, so they count as payload.
    let attached = "Attached file notes.txt:\nsix66";
    conversation.begin(
        user(vec![
            text("typed"),
            text(attached),
            Block::Image {
                media_type: "image/png".into(),
                data: vec![0; 100],
            },
            Block::Document {
                format: DocumentFormat::Pdf,
                name: "d".into(),
                data: vec![0; 50],
            },
        ]),
        entry("user", "typed", 1),
    );
    assert_eq!(
        conversation.bytes(),
        "typed".len() + attached.len() + 100 + 50
    );

    let raw = serde_json::json!({ "redactedContent": "AQID" });
    conversation.complete(
        Some(assistant(vec![
            Block::Thinking {
                text: "thinking text".into(),
                signature: Some("0123456789".into()),
                provider: "bedrock".into(),
                model: "m".into(),
            },
            Block::Opaque {
                provider: "bedrock".into(),
                model: "m".into(),
                raw: raw.clone(),
            },
            text("reply"),
        ])),
        vec![
            entry("thought", "thinking text", 2),
            entry("agent", "reply", 2),
        ],
    );
    let opaque = serde_json::to_vec(&raw).unwrap().len();
    assert_eq!(
        conversation.bytes(),
        "typed".len()
            + attached.len()
            + 100
            + 50
            + "thinking text".len()
            + 10
            + opaque
            + "reply".len()
    );
}

#[test]
fn eviction_drops_whole_exchanges_from_both_stores_and_never_the_newest() {
    // Budget: 60 bytes of text plus payload, plenty of entries.
    let mut conversation = Conversation::with_budget_for_test(60, 10_000);
    for i in 0..5 {
        let q = format!("question-{i}");
        let a = format!("answer-{i}");
        conversation.begin(user(vec![text(&q)]), entry("user", &q, i));
        conversation.complete(Some(assistant(vec![text(&a)])), vec![entry("agent", &a, i)]);
    }
    // Each exchange is 10 + 8 = 18 bytes; three fit in 60, four do not.
    assert_eq!(conversation.exchange_count(), 3);
    assert!(conversation.bytes() <= 60);
    assert_eq!(
        texts(&conversation),
        vec![
            "User:question-2",
            "Assistant:answer-2",
            "User:question-3",
            "Assistant:answer-3",
            "User:question-4",
            "Assistant:answer-4"
        ]
    );
    assert_eq!(
        history_texts(&conversation),
        vec![
            "user:question-2",
            "agent:answer-2",
            "user:question-3",
            "agent:answer-3",
            "user:question-4",
            "agent:answer-4"
        ]
    );

    // A single exchange over the whole budget is kept on its own.
    let long = "x".repeat(500);
    conversation.begin(user(vec![text(&long)]), entry("user", &long, 9));
    assert_eq!(conversation.exchange_count(), 1);
    assert!(conversation.has_open_exchange());
    assert_eq!(conversation.history().len(), 1);
    // Payload alone evicts too: an image on the newest exchange is over the
    // budget, and that exchange is the one kept.
    conversation.complete(None, Vec::new());
    conversation.begin(
        user(vec![Block::Image {
            media_type: "image/png".into(),
            data: vec![0; 1000],
        }]),
        entry("user", "", 10),
    );
    assert_eq!(conversation.exchange_count(), 1);
    assert_eq!(conversation.bytes(), 1000);
}

#[test]
fn the_entry_cap_evicts_whole_exchanges_and_never_the_newest() {
    let mut conversation = Conversation::with_budget_for_test(1 << 20, 4);
    for i in 0..3 {
        conversation.begin(user(vec![text("q")]), entry("user", "q", i));
        conversation.complete(
            Some(assistant(vec![text("a")])),
            vec![entry("thought", "t", i), entry("agent", "a", i)],
        );
    }
    // Three entries per exchange against a cap of four: one exchange stays.
    assert_eq!(conversation.exchange_count(), 1);
    assert_eq!(conversation.history().len(), 3);
    // A single exchange over the cap is kept whole.
    conversation.begin(user(vec![text("q")]), entry("user", "q", 5));
    conversation.complete(
        Some(assistant(vec![text("a")])),
        (0..6).map(|i| entry("thought", "t", 5 + i)).collect(),
    );
    assert_eq!(conversation.exchange_count(), 1);
    assert_eq!(conversation.history().len(), 7);
}

// ---------- review fixes, 2026-09-08 ----------

fn thinking(text: &str) -> Block {
    Block::Thinking {
        text: text.to_string(),
        signature: Some("sig".to_string()),
        provider: "bedrock".to_string(),
        model: "m".to_string(),
    }
}

fn statuses(conversation: &Conversation) -> Vec<ExchangeStatus> {
    conversation.exchanges().map(|e| e.status).collect()
}

#[test]
fn a_rejection_covers_the_unanswered_run_before_it() {
    // The request the service refused was the merge of every unanswered
    // user message before the new one, so those exchanges are rejected
    // with it; an exchange that got a reply, and everything before it,
    // stays.
    let mut c = Conversation::new();
    c.begin(user(vec![text("a")]), entry("user", "a", 1));
    c.complete(None, vec![]); // failed before any reply: closed, no assistant
    c.begin(user(vec![text("b")]), entry("user", "b", 2));
    c.complete(
        Some(assistant(vec![text("B")])),
        vec![entry("agent", "B", 3)],
    );
    c.begin(user(vec![text("c")]), entry("user", "c", 4));
    c.complete(None, vec![]);
    c.begin(user(vec![text("d")]), entry("user", "d", 5));
    c.complete(None, vec![]);
    c.begin(user(vec![text("e")]), entry("user", "e", 6));
    assert!(c.reject_open(vec![]));
    assert_eq!(
        statuses(&c),
        vec![
            ExchangeStatus::Closed,
            ExchangeStatus::Closed,
            ExchangeStatus::Rejected,
            ExchangeStatus::Rejected,
            ExchangeStatus::Rejected,
        ]
    );
    // The next request carries a, b and B: nothing the service refused.
    assert_eq!(texts(&c), vec!["User:a", "User:b", "Assistant:B"]);
    // The transcript keeps every entry.
    assert_eq!(c.history().len(), 6);
}

#[test]
fn strip_reasoning_drops_every_reasoning_block_and_keeps_the_text() {
    let mut c = Conversation::new();
    c.begin(user(vec![text("a")]), entry("user", "a", 1));
    c.complete(
        Some(assistant(vec![
            thinking("think"),
            Block::Opaque {
                provider: "bedrock".into(),
                model: "m".into(),
                raw: serde_json::json!({ "redactedContent": "AQID" }),
            },
            text("A"),
        ])),
        vec![entry("thought", "think", 2), entry("agent", "A", 3)],
    );
    c.begin(user(vec![text("b")]), entry("user", "b", 4));
    c.complete(
        Some(assistant(vec![thinking("more"), text("B")])),
        vec![entry("agent", "B", 5)],
    );
    let before = c.bytes();
    assert_eq!(c.strip_reasoning(), 3);
    assert!(
        c.bytes() < before,
        "the signatures and the opaque payload left the budget"
    );
    assert_eq!(
        texts(&c),
        vec!["User:a", "Assistant:A", "User:b", "Assistant:B"]
    );
    assert!(c
        .messages()
        .iter()
        .all(|m| m.blocks.iter().all(|b| matches!(b, Block::Text { .. }))));
    // The transcript still shows the thought.
    assert_eq!(c.history().len(), 5);
    assert_eq!(c.strip_reasoning(), 0, "nothing left to drop");
}

#[test]
fn an_eviction_drops_the_reasoning_of_what_remains() {
    // Removing an earlier turn changes the prefix every later reasoning
    // block is bound to, so the eviction takes the reasoning with it and
    // the next request replays none.
    // Budget 80: the two exchanges hold 60 bytes (54 of text, 6 of
    // signatures); the third question tips them over and the front one
    // goes.
    let mut c = Conversation::with_budget_for_test(80, 10_000);
    c.begin(
        user(vec![text("first question")]),
        entry("user", "first question", 1),
    );
    c.complete(
        Some(assistant(vec![thinking("t1"), text("first answer")])),
        vec![entry("agent", "first answer", 2)],
    );
    c.begin(
        user(vec![text("second question")]),
        entry("user", "second question", 3),
    );
    c.complete(
        Some(assistant(vec![thinking("t2"), text("second answer")])),
        vec![entry("agent", "second answer", 4)],
    );
    assert_eq!(c.exchange_count(), 2, "both fit so far");
    c.begin(
        user(vec![text("a third question that tips the budget over")]),
        entry("user", "a third question that tips the budget over", 5),
    );
    assert!(c.exchange_count() < 3, "the front exchange was evicted");
    for message in c.messages() {
        assert!(
            message
                .blocks
                .iter()
                .all(|b| !matches!(b, Block::Thinking { .. } | Block::Opaque { .. })),
            "reasoning survived the eviction: {message:?}"
        );
    }
    assert!(
        texts(&c).iter().any(|t| t == "Assistant:second answer"),
        "the text stays"
    );
}

#[test]
fn a_rejected_exchange_with_reply_entries_evicts_whole() {
    // A rejected exchange that streamed a partial reply holds two entries;
    // when the budget evicts it, both leave the transcript, or the two
    // stores drift for the life of the session. Budget 64: the exchanges
    // hold 16 bytes before the long question and 65 with it, so the front
    // exchange (two entries) goes and 55 remain.
    let mut c = Conversation::with_budget_for_test(64, 10_000);
    c.begin(user(vec![text("bad")]), entry("user", "bad", 1));
    assert!(c.reject_open(vec![entry("agent", "partial", 2)]));
    c.begin(user(vec![text("ok")]), entry("user", "ok", 3));
    c.complete(
        Some(assistant(vec![text("fine")])),
        vec![entry("agent", "fine", 4)],
    );
    let long = "a long enough question to evict the first exchange";
    c.begin(user(vec![text(long)]), entry("user", long, 5));
    assert_eq!(
        history_texts(&c),
        vec![
            "user:ok".to_string(),
            "agent:fine".to_string(),
            format!("user:{long}")
        ]
    );
    assert_eq!(c.exchange_count(), 2);
    assert_eq!(
        texts(&c),
        vec![
            "User:ok".to_string(),
            "Assistant:fine".to_string(),
            format!("User:{long}")
        ]
    );
}

#[test]
fn an_image_with_no_data_is_refused_before_any_request() {
    use mezame::conversation::{user_message_from_blocks, BlockError};
    use serde_json::json;
    for block in [
        json!({ "type": "image", "mimeType": "image/png" }),
        json!({ "type": "image", "mimeType": "image/png", "data": "" }),
        json!({ "type": "image", "mimeType": "image/png", "data": null }),
        json!({ "type": "resource", "resource": { "uri": "file:///x.pdf", "mimeType": "application/pdf", "blob": "" } }),
    ] {
        let err = user_message_from_blocks(std::slice::from_ref(&block)).unwrap_err();
        match err {
            BlockError::Unsupported { reason, .. } => {
                assert!(reason.contains("holds no data"), "{block}: {reason}")
            }
            other => panic!("{block}: {other:?}"),
        }
    }
}
