//! The seam's own behaviour: the shipped `EchoBackend`, the echo text
//! derivation both the Hub and a Backend are defined against, and the
//! session id form the upgrade handler binds a hub to.
//!
//! No hub and no socket is involved, and this file declares no support
//! module: every subject here is reachable from the library's public API
//! on its own.

use std::collections::HashSet;

use mezame::backend::{extract_user_text, user_echo_event, user_text_len, Backend, EchoBackend};
use mezame::ws::{decide_session, is_session_id, new_session_id, SessionDecision};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Run one turn against `backend` and return the events it streamed.
async fn run_turn(backend: &EchoBackend, blocks: Vec<Value>) -> Vec<Value> {
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Value>();
    backend
        .prompt(blocks, events_tx)
        .await
        .expect("an echo turn resolves with success");
    let mut streamed = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        streamed.push(event);
    }
    streamed
}

/// The transcript as JSON, which is the shape `GET /history` serves.
async fn transcript_json(backend: &EchoBackend) -> Vec<Value> {
    let entries = backend.history().await;
    serde_json::to_value(&entries)
        .expect("a transcript serialises")
        .as_array()
        .expect("an array")
        .clone()
}

#[tokio::test]
async fn echo_streams_one_agent_append_holding_the_joined_text_blocks() {
    // Requirement 4 criteria 2 and 6: exactly one event, an `append` with
    // `role` `agent`, whose text is the text blocks joined by a newline.
    // The image block between them contributes nothing.
    let backend = EchoBackend::new();
    let streamed = run_turn(
        &backend,
        vec![
            json!({ "type": "text", "text": "first" }),
            json!({ "type": "image", "mimeType": "image/png", "data": "AAAA" }),
            json!({ "type": "text", "text": "second" }),
        ],
    )
    .await;

    assert_eq!(streamed.len(), 1, "one event per turn, got {streamed:?}");
    assert_eq!(streamed[0]["type"], "append");
    assert_eq!(streamed[0]["role"], "agent");
    assert_eq!(streamed[0]["text"], "first\nsecond");
}

#[tokio::test]
async fn echo_records_the_user_turn_with_no_prefix_and_no_trailing_newline() {
    // Requirement 4 criterion 4. The browser adds the `> ` and the newline
    // when it renders the entry; a transcript holding the echo verbatim
    // would render `> > text`.
    let backend = EchoBackend::new();
    let streamed = run_turn(
        &backend,
        vec![
            json!({ "type": "text", "text": "hello" }),
            json!({ "type": "text", "text": "world" }),
        ],
    )
    .await;
    let entries = transcript_json(&backend).await;

    assert_eq!(entries.len(), 2, "one user entry and one agent entry");
    assert_eq!(entries[0]["role"], "user");
    assert_eq!(entries[0]["text"], "hello\nworld");
    assert_eq!(entries[1]["role"], "agent");
    assert_eq!(entries[1]["text"], streamed[0]["text"]);
    for entry in &entries {
        assert!(
            entry["timestamp"].as_i64().is_some_and(|ts| ts > 0),
            "every entry carries a millisecond timestamp, got {entry}"
        );
    }
}

#[tokio::test]
async fn echo_timestamps_never_decrease_across_turns() {
    // Requirement 4 criterion 5: every retained entry, in order, with
    // non-decreasing timestamps; the ceilings have their own cases below.
    let backend = EchoBackend::new();
    for turn in 0..4 {
        run_turn(
            &backend,
            vec![json!({ "type": "text", "text": format!("turn {turn}") })],
        )
        .await;
    }
    let entries = transcript_json(&backend).await;

    assert_eq!(entries.len(), 8, "two entries per turn, none dropped");
    assert_eq!(entries[0]["text"], "turn 0");
    assert_eq!(entries[6]["text"], "turn 3");
    let stamps: Vec<i64> = entries
        .iter()
        .map(|e| e["timestamp"].as_i64().expect("a timestamp"))
        .collect();
    assert!(
        stamps.windows(2).all(|w| w[0] <= w[1]),
        "timestamps must not decrease, got {stamps:?}"
    );
}

#[tokio::test]
async fn echo_refuses_a_model_change_and_leaves_the_transcript_alone() {
    // Requirement 4 criterion 7. The refusal is what exercises the hub's
    // failed-model-change arm, and it keeps the model picker honestly
    // empty.
    let backend = EchoBackend::new();
    run_turn(&backend, vec![json!({ "type": "text", "text": "hi" })]).await;
    let before = transcript_json(&backend).await;

    let refused = backend.set_model("anything".to_string()).await;
    let message = format!("{}", refused.expect_err("no model is selectable"));
    assert!(
        message.to_lowercase().contains("no model is selectable"),
        "the error states that no model is selectable, got {message:?}"
    );
    assert_eq!(
        transcript_json(&backend).await,
        before,
        "a refused model change records nothing"
    );
}

#[tokio::test]
async fn echo_answers_a_prompt_that_holds_no_text_block() {
    // Requirement 4 criterion 9. The user entry holds the empty string and
    // the agent entry holds the same notice the browser sees.
    let backend = EchoBackend::new();
    let streamed = run_turn(
        &backend,
        vec![json!({ "type": "image", "mimeType": "image/png", "data": "AAAA" })],
    )
    .await;

    assert_eq!(streamed.len(), 1);
    assert_eq!(streamed[0]["role"], "agent");
    let notice = streamed[0]["text"].as_str().expect("text");
    assert!(
        notice.contains("no text block"),
        "the reply states the prompt held no text block, got {notice:?}"
    );

    let entries = transcript_json(&backend).await;
    assert_eq!(entries[0]["role"], "user");
    assert_eq!(entries[0]["text"], "");
    assert_eq!(entries[1]["text"], notice);
}

#[tokio::test]
async fn echo_shutdown_discards_the_transcript_and_the_no_ops_resolve() {
    // Requirement 4 criteria 8, 10 and 11. `cancel` and
    // `permission_response` resolve with no effect, and `shutdown` drops
    // the transcript and nothing else.
    let backend = EchoBackend::new();
    run_turn(&backend, vec![json!({ "type": "text", "text": "hi" })]).await;
    assert_eq!(transcript_json(&backend).await.len(), 2);

    backend.cancel().await;
    backend
        .permission_response(json!("perm-1"), "allow".to_string())
        .await;
    assert_eq!(
        transcript_json(&backend).await.len(),
        2,
        "neither call touches the transcript"
    );

    backend.shutdown().await;
    assert!(
        transcript_json(&backend).await.is_empty(),
        "shutdown discards the transcript"
    );
    // Idempotent.
    backend.shutdown().await;
    assert!(transcript_json(&backend).await.is_empty());
}

#[test]
fn the_echo_text_is_the_join_prefixed_once_and_terminated_once() {
    // Requirement 12 criterion 3, and the derivation the transcript's user
    // entry is defined against.
    let blocks = vec![
        json!({ "type": "text", "text": "one" }),
        json!({ "type": "resource", "resource": { "uri": "file:///x", "text": "ignored" } }),
        json!({ "type": "text", "text": "two" }),
    ];
    assert_eq!(extract_user_text(&blocks).as_deref(), Some("one\ntwo"));

    let echo = user_echo_event(&blocks);
    assert_eq!(echo["type"], "append");
    assert_eq!(echo["role"], "user");
    assert_eq!(echo["text"], "> one\ntwo\n");
}

#[test]
fn the_echo_text_for_a_block_list_holding_no_text_block_is_the_bare_prefix() {
    // Requirement 12 criterion 12. A peer browser still sees a user turn
    // and still locks its composer.
    let image_only = vec![json!({ "type": "image", "mimeType": "image/png", "data": "AAAA" })];
    assert_eq!(extract_user_text(&image_only), None);
    assert_eq!(user_echo_event(&image_only)["text"], "> \n");

    assert_eq!(extract_user_text(&[]), None);
    assert_eq!(user_echo_event(&[])["text"], "> \n");

    // A text block whose text is empty is a thing said, not an absence.
    let empty_text = vec![json!({ "type": "text", "text": "" })];
    assert_eq!(extract_user_text(&empty_text).as_deref(), Some(""));
    assert_eq!(user_echo_event(&empty_text)["text"], "> \n");
}

#[test]
fn is_session_id_accepts_the_minted_form() {
    // Requirement 6 criterion 7: exactly 32 lowercase hexadecimal
    // characters, the form criterion 6's minting produces and nothing
    // else.
    for accepted in [
        "0123456789abcdef0123456789abcdef",
        &"0".repeat(32),
        &"f".repeat(32),
        &"a".repeat(32),
    ] {
        assert!(is_session_id(accepted), "should accept {accepted:?}");
    }
    assert!(is_session_id(&new_session_id()), "a minted id is accepted");
}

#[test]
fn is_session_id_rejects_everything_else() {
    // A client-chosen name would make a session anyone could find by
    // guessing it, so a name, a shorter or longer hex string, the minted
    // form in upper case, and anything carrying whitespace or a separator
    // are all refused.
    let minted = new_session_id();
    let mut upper = minted.clone();
    upper.make_ascii_uppercase();
    let mut with_slash = minted.clone();
    with_slash.replace_range(15..16, "/");
    let mut with_space = minted.clone();
    with_space.replace_range(15..16, " ");
    let refused: Vec<String> = vec![
        String::new(),
        " ".into(),
        "a".into(),
        "test".into(),
        "abc-1".into(),
        "abc_1".into(),
        "AbC-123_xyz".into(),
        "g".repeat(32),
        "-".repeat(32),
        "_".repeat(32),
        minted[..31].to_string(),
        format!("{minted}0"),
        upper,
        with_slash,
        with_space,
        format!(" {minted}"),
        format!("{minted}\n"),
        "..".into(),
        "../x".into(),
        "日本語".into(),
        "a".repeat(128),
    ];
    for refused in &refused {
        assert!(!is_session_id(refused), "should refuse {refused:?}");
    }
}

#[test]
fn decide_session_mints_accepts_and_refuses() {
    // Requirement 6 criteria 1, 2 and 8, through the pure decision the
    // upgrade handler and Property 6 share.
    let id = new_session_id();
    let mut upper = id.clone();
    upper.make_ascii_uppercase();

    assert_eq!(decide_session(None), SessionDecision::Mint);
    assert_eq!(decide_session(Some("")), SessionDecision::Mint);
    assert_eq!(decide_session(Some("   ")), SessionDecision::Mint);
    assert_eq!(decide_session(Some("\t\n ")), SessionDecision::Mint);
    assert_eq!(
        decide_session(Some(&format!(" {id} "))),
        SessionDecision::Accept(id.clone())
    );
    assert_eq!(
        decide_session(Some(&id)),
        SessionDecision::Accept(id.clone())
    );
    for refused in ["../x", "a b", "日本語", "abc-1", "test", &upper, &id[..31]] {
        assert_eq!(
            decide_session(Some(refused)),
            SessionDecision::Refuse,
            "{refused:?}"
        );
    }
}

/// Set in the child run of the cross-run uniqueness test below.
const CHILD_MODE: &str = "MEZAME_SESSION_ID_MINT_CHILD";

/// How many ids each run mints. Requirement 6 criterion 6 names the
/// figure.
const MINT_COUNT: usize = 10_000;

/// Each id the child prints sits behind this prefix on a line of its own.
/// libtest's single-threaded formatter leaves its `test <name> ... `
/// progress line open when the body starts, so the child also opens with
/// a newline. Without both, the first id is glued to that line, the
/// filter in the parent drops it, and the run comes up one short.
const ID_LINE: &str = "minted:";

#[test]
fn minted_session_ids_never_collide_within_or_across_process_runs() {
    // Requirement 6 criterion 6 bounds uniqueness past a single process
    // run: a browser holds its session id over a Mezame restart, and a
    // per-process guarantee would let a restart mint an id a browser
    // already holds. Verifying that needs two processes, so this test
    // re-executes its own binary twice with an environment guard. No new
    // binary surface, no filesystem state, no fresh checkout between runs.
    if std::env::var_os(CHILD_MODE).is_some() {
        let mut out = String::with_capacity(1 + MINT_COUNT * (ID_LINE.len() + 33));
        out.push('\n');
        for _ in 0..MINT_COUNT {
            out.push_str(ID_LINE);
            out.push_str(&new_session_id());
            out.push('\n');
        }
        print!("{out}");
        return;
    }

    let exe = std::env::current_exe().expect("the path of this test binary");
    let mint_in_a_child_process = || -> Vec<String> {
        let output = std::process::Command::new(&exe)
            .arg("--exact")
            .arg("minted_session_ids_never_collide_within_or_across_process_runs")
            .arg("--nocapture")
            .env(CHILD_MODE, "1")
            .output()
            .expect("re-executing this test binary");
        assert!(
            output.status.success(),
            "the child run failed: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The child's stdout carries libtest's own progress lines too.
        // Keep what sits behind the prefix, and nothing else; the checks
        // below hold every kept line to the minted form.
        String::from_utf8(output.stdout)
            .expect("the child prints UTF-8")
            .lines()
            .filter_map(|line| line.strip_prefix(ID_LINE))
            .map(str::to_string)
            .collect()
    };

    let first_run = mint_in_a_child_process();
    let second_run = mint_in_a_child_process();
    assert_eq!(
        first_run.len(),
        MINT_COUNT,
        "the first run minted its share"
    );
    assert_eq!(
        second_run.len(),
        MINT_COUNT,
        "the second run minted its share"
    );
    for id in first_run.iter().chain(second_run.iter()) {
        assert!(is_session_id(id), "every minted id is accepted: {id:?}");
        assert_eq!(id.len(), 32, "32 lowercase hex characters: {id:?}");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "lowercase hex only: {id:?}"
        );
    }

    let first: HashSet<String> = first_run.into_iter().collect();
    let second: HashSet<String> = second_run.into_iter().collect();
    assert_eq!(
        first.len(),
        MINT_COUNT,
        "no id repeats within the first run"
    );
    assert_eq!(
        second.len(),
        MINT_COUNT,
        "no id repeats within the second run"
    );
    assert!(
        first.is_disjoint(&second),
        "no id is shared between two process runs"
    );
}

#[tokio::test]
async fn echo_transcript_evicts_the_oldest_turns_past_its_byte_budget() {
    // A turn of 100 bytes of text records 200 bytes. Ten turns against a
    // 1000-byte budget keep the newest five: the oldest go first, a turn
    // at a time, and what remains is contiguous, in order, and inside the
    // budget, with timestamps still non-decreasing.
    let backend = EchoBackend::with_budget(1000, usize::MAX);
    for i in 0..10 {
        let text = format!("{i:0>100}");
        run_turn(&backend, vec![json!({ "type": "text", "text": text })]).await;
    }
    let entries = transcript_json(&backend).await;
    assert_eq!(entries.len(), 10, "five turns of two entries");
    assert_eq!(entries[0]["role"], "user");
    assert_eq!(
        entries[0]["text"],
        format!("{:0>100}", 5),
        "the oldest retained turn is the sixth"
    );
    assert_eq!(entries[9]["role"], "agent");
    assert_eq!(entries[9]["text"], format!("{:0>100}", 9));
    let total: usize = entries
        .iter()
        .map(|e| e["text"].as_str().expect("text").len())
        .sum();
    assert!(total <= 1000, "inside the budget, got {total}");
    let timestamps: Vec<i64> = entries
        .iter()
        .map(|e| e["timestamp"].as_i64().expect("timestamp"))
        .collect();
    assert!(timestamps.windows(2).all(|w| w[0] <= w[1]));
}

#[tokio::test]
async fn echo_transcript_always_keeps_the_newest_turn() {
    // A single turn larger than the whole budget is retained on its own,
    // and the next turn replaces it rather than joining it.
    let backend = EchoBackend::with_budget(100, usize::MAX);
    run_turn(
        &backend,
        vec![json!({ "type": "text", "text": "a".repeat(300) })],
    )
    .await;
    let entries = transcript_json(&backend).await;
    assert_eq!(entries.len(), 2, "the oversize turn is kept");
    assert_eq!(entries[0]["text"].as_str().map(str::len), Some(300));

    run_turn(
        &backend,
        vec![json!({ "type": "text", "text": "b".repeat(300) })],
    )
    .await;
    let entries = transcript_json(&backend).await;
    assert_eq!(entries.len(), 2, "the older oversize turn is evicted");
    assert!(entries[0]["text"]
        .as_str()
        .is_some_and(|t| t.starts_with('b')));
}

#[tokio::test]
async fn echo_transcript_evicts_past_its_entry_ceiling() {
    // Empty prompts add two entries and no bytes; the entry ceiling is
    // what bounds them.
    let backend = EchoBackend::with_budget(usize::MAX, 6);
    for _ in 0..5 {
        run_turn(&backend, vec![json!({ "type": "text", "text": "" })]).await;
    }
    assert_eq!(transcript_json(&backend).await.len(), 6);
}

#[tokio::test]
async fn echo_shipped_ceilings_are_the_documented_ones() {
    // The constants the wire protocol document quotes.
    assert_eq!(mezame::backend::TRANSCRIPT_BUDGET_BYTES, 16 * 1024 * 1024);
    assert_eq!(mezame::backend::TRANSCRIPT_MAX_ENTRIES, 10_000);
    assert_eq!(mezame::hub::MAX_PROMPT_TEXT_BYTES, 1024 * 1024);
    assert_eq!(mezame::ws::MAX_WS_MESSAGE_BYTES, 32 * 1024 * 1024);
    // And `new` runs under them: a small transcript is untouched.
    let backend = EchoBackend::new();
    run_turn(&backend, vec![json!({ "type": "text", "text": "hi" })]).await;
    assert_eq!(transcript_json(&backend).await.len(), 2);
}

#[test]
fn user_text_len_matches_the_derived_text() {
    // The hub's ceiling check must measure exactly what the echo and the
    // transcript are defined against, without building the join.
    let cases: Vec<Vec<Value>> = vec![
        vec![],
        vec![json!({ "type": "image", "mimeType": "image/png", "data": "AAAA" })],
        vec![json!({ "type": "text", "text": "" })],
        vec![
            json!({ "type": "text", "text": "" }),
            json!({ "type": "text", "text": "" }),
        ],
        vec![
            json!({ "type": "text", "text": "one" }),
            json!({ "type": "text", "text": "two" }),
        ],
        vec![
            json!({ "type": "text", "text": "a" }),
            json!({ "type": "resource", "resource": { "uri": "file:///x", "text": "no" } }),
            json!({ "type": "text", "text": "日本語" }),
        ],
        vec![
            json!({ "type": "text" }),
            json!({ "type": "text", "text": 5 }),
        ],
    ];
    for blocks in cases {
        let derived = extract_user_text(&blocks).map_or(0, |t| t.len());
        assert_eq!(user_text_len(&blocks), derived, "{blocks:?}");
    }
}
