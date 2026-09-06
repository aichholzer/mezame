//! The seam's own behaviour: the shipped `EchoBackend`, the echo text
//! derivation both the Hub and a Backend are defined against, and the
//! session id form the upgrade handler binds a hub to.
//!
//! No hub and no socket is involved, and this file declares no support
//! module: every subject here is reachable from the library's public API
//! on its own.

use std::collections::HashSet;

use mezame::backend::{extract_user_text, user_echo_event, Backend, EchoBackend};
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
    // Requirement 4 criterion 5: every entry recorded for the life of the
    // hub, in order, with non-decreasing timestamps and no cap.
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
fn is_session_id_accepts_the_documented_form() {
    // Requirement 6 criterion 7: 1 to 128 characters drawn from the ASCII
    // letters, the digits, the hyphen and the underscore.
    for accepted in [
        "a",
        "0",
        "abc-1",
        "abc_1",
        "AbC-123_xyz",
        "-",
        "_",
        &"a".repeat(128),
    ] {
        assert!(is_session_id(accepted), "should accept {accepted:?}");
    }
    assert!(is_session_id(&new_session_id()), "a minted id is accepted");
}

#[test]
fn is_session_id_rejects_everything_else() {
    for refused in [
        "",
        " ",
        "a b",
        " a",
        "a\t",
        "a\n",
        "a/b",
        "a\\b",
        "..",
        "../x",
        "a/../b",
        "./a",
        "sessión",
        "日本語",
        "a.b",
        "a:b",
        "a%20b",
        &"a".repeat(129),
    ] {
        assert!(!is_session_id(refused), "should refuse {refused:?}");
    }
}

#[test]
fn decide_session_mints_accepts_and_refuses() {
    // Requirement 6 criteria 1, 2 and 8, through the pure decision the
    // upgrade handler and Property 6 share.
    assert_eq!(decide_session(None), SessionDecision::Mint);
    assert_eq!(decide_session(Some("")), SessionDecision::Mint);
    assert_eq!(decide_session(Some("   ")), SessionDecision::Mint);
    assert_eq!(decide_session(Some("\t\n ")), SessionDecision::Mint);
    assert_eq!(
        decide_session(Some(" abc-1 ")),
        SessionDecision::Accept("abc-1".to_string())
    );
    assert_eq!(
        decide_session(Some("abc-1")),
        SessionDecision::Accept("abc-1".to_string())
    );
    assert_eq!(decide_session(Some("../x")), SessionDecision::Refuse);
    assert_eq!(decide_session(Some("a b")), SessionDecision::Refuse);
    assert_eq!(decide_session(Some("日本語")), SessionDecision::Refuse);
}

/// Set in the child run of the cross-run uniqueness test below.
const CHILD_MODE: &str = "MEZAME_SESSION_ID_MINT_CHILD";

/// How many ids each run mints. Requirement 6 criterion 6 names the
/// figure.
const MINT_COUNT: usize = 10_000;

#[test]
fn minted_session_ids_never_collide_within_or_across_process_runs() {
    // Requirement 6 criterion 6 bounds uniqueness past a single process
    // run: a browser holds its session id over a Mezame restart, and a
    // per-process guarantee would let a restart mint an id a browser
    // already holds. Verifying that needs two processes, so this test
    // re-executes its own binary twice with an environment guard. No new
    // binary surface, no filesystem state, no fresh checkout between runs.
    if std::env::var_os(CHILD_MODE).is_some() {
        let mut out = String::with_capacity(MINT_COUNT * 33);
        for _ in 0..MINT_COUNT {
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
        // Keep the lines that are a minted id and nothing else.
        String::from_utf8(output.stdout)
            .expect("the child prints UTF-8")
            .lines()
            .filter(|line| line.len() == 32 && line.bytes().all(|b| b.is_ascii_hexdigit()))
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
