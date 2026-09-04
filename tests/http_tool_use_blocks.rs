//! Integration tests for `mezame::http::emit_tool_use_blocks` and
//! `mezame::http::merge_tool_results`, the two halves of rehydrating a
//! tool call from Kiro's session JSONL.
//!
//! Kiro records the `toolUse` on the assistant message that triggered the
//! call and the result on a later `ToolResults` entry. `emit` turns the
//! first into a `tool_call` history entry with null status and content,
//! and `merge` patches those in when the second arrives. The wire shape
//! matches the live `tool_call` event, and the client pushes the same log
//! entry on rehydrate as it does mid-turn.

use mezame::http::{emit_tool_use_blocks, merge_tool_results};
use serde_json::{json, Value};

/// A `tool_call` history entry as `emit_tool_use_blocks` would have
/// produced it, for driving `merge_tool_results` directly.
fn tool_call_entry(id: &str) -> Value {
    json!({
        "role": "tool_call",
        "toolCallId": id,
        "title": "web_search",
        "status": Value::Null,
        "content": Value::Null
    })
}

// ---------- emit_tool_use_blocks ----------

#[test]
fn emits_one_entry_per_tool_use_block() {
    let data = json!({ "content": [
        { "kind": "toolUse", "data": { "toolUseId": "tu-1", "name": "read", "input": { "p": "a" } } },
        { "kind": "toolUse", "data": { "toolUseId": "tu-2", "name": "grep", "input": { "q": "b" } } }
    ] });
    let mut out = Vec::new();
    emit_tool_use_blocks(&data, Some(1_700_000_000_000), &mut out);

    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["role"], "tool_call");
    assert_eq!(out[0]["toolCallId"], "tu-1");
    assert_eq!(out[0]["title"], "read");
    assert_eq!(out[0]["rawInput"]["p"], "a");
    assert_eq!(out[0]["timestamp"], 1_700_000_000_000_i64);
    // Status and content stay null until the ToolResults entry lands.
    assert!(out[0]["status"].is_null());
    assert!(out[0]["content"].is_null());
    assert_eq!(out[1]["toolCallId"], "tu-2");
    assert_eq!(out[1]["title"], "grep");
}

#[test]
fn emits_nothing_when_there_is_no_content_array() {
    // `content` absent, and `content` present as the wrong type. An
    // assistant message with neither shape contributes no tool cards.
    let mut out = Vec::new();
    emit_tool_use_blocks(&json!({}), None, &mut out);
    emit_tool_use_blocks(&json!({ "content": "not an array" }), None, &mut out);
    assert!(out.is_empty());
}

#[test]
fn skips_blocks_that_are_not_tool_uses() {
    let data = json!({ "content": [
        { "kind": "text", "data": "prose" },
        { "kind": "thinking", "data": { "text": "reasoning" } },
        { "kind": "toolUse", "data": { "toolUseId": "tu-3", "name": "read" } }
    ] });
    let mut out = Vec::new();
    emit_tool_use_blocks(&data, None, &mut out);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["toolCallId"], "tu-3");
}

#[test]
fn skips_a_tool_use_with_no_id() {
    // The id is the only key `merge_tool_results` can match on. A block
    // without one could never receive its result, and a card stuck on
    // "pending" forever is worse than no card.
    let data = json!({ "content": [
        { "kind": "toolUse" },
        { "kind": "toolUse", "data": {} },
        { "kind": "toolUse", "data": { "name": "read" } },
        { "kind": "toolUse", "data": { "toolUseId": "tu-4" } }
    ] });
    let mut out = Vec::new();
    emit_tool_use_blocks(&data, None, &mut out);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["toolCallId"], "tu-4");
}

#[test]
fn falls_back_to_a_generic_title_when_the_name_is_absent() {
    let data = json!({ "content": [
        { "kind": "toolUse", "data": { "toolUseId": "tu-5" } }
    ] });
    let mut out = Vec::new();
    emit_tool_use_blocks(&data, None, &mut out);

    assert_eq!(out[0]["title"], "tool");
    assert!(out[0]["rawInput"].is_null());
}

// ---------- merge_tool_results ----------

#[test]
fn merges_status_and_content_into_the_matching_entry() {
    let mut out = vec![tool_call_entry("tu-1")];
    let data = json!({ "content": [{
        "kind": "toolResult",
        "data": {
            "toolUseId": "tu-1",
            "status": "success",
            "content": [{ "kind": "text", "data": "the answer" }]
        }
    }] });
    merge_tool_results(&data, &mut out);

    assert_eq!(out[0]["status"], "success");
    assert_eq!(out[0]["content"][0]["data"], "the answer");
    // Fields the result says nothing about are left alone.
    assert_eq!(out[0]["title"], "web_search");
}

#[test]
fn merges_nothing_when_there_is_no_content_array() {
    let mut out = vec![tool_call_entry("tu-1")];
    merge_tool_results(&json!({}), &mut out);
    merge_tool_results(&json!({ "content": 7 }), &mut out);
    assert!(out[0]["status"].is_null());
}

#[test]
fn skips_blocks_that_are_not_tool_results() {
    let mut out = vec![tool_call_entry("tu-1")];
    let data = json!({ "content": [
        { "kind": "text", "data": "chatter" },
        { "kind": "toolResult", "data": { "toolUseId": "tu-1", "status": "error" } }
    ] });
    merge_tool_results(&data, &mut out);

    assert_eq!(out[0]["status"], "error");
}

#[test]
fn skips_a_tool_result_with_no_id() {
    let mut out = vec![tool_call_entry("tu-1")];
    let data = json!({ "content": [
        { "kind": "toolResult", "data": { "status": "orphaned" } },
        { "kind": "toolResult" },
        { "kind": "toolResult", "data": { "toolUseId": "tu-1", "status": "success" } }
    ] });
    merge_tool_results(&data, &mut out);

    assert_eq!(out[0]["status"], "success");
}

#[test]
fn walks_past_entries_that_are_not_tool_calls() {
    // The search runs backwards over the history built so far, which in a
    // real replay holds user and agent entries after the tool call.
    let mut out = vec![
        tool_call_entry("tu-1"),
        json!({ "role": "agent", "text": "done" }),
        json!({ "role": "user", "text": "next question" }),
    ];
    let data = json!({ "content": [{
        "kind": "toolResult",
        "data": { "toolUseId": "tu-1", "status": "success" }
    }] });
    merge_tool_results(&data, &mut out);

    assert_eq!(out[0]["status"], "success");
    // The non-tool entries are untouched.
    assert_eq!(out[1]["text"], "done");
    assert!(out[2].get("status").is_none());
}

#[test]
fn walks_past_tool_calls_with_a_different_id() {
    let mut out = vec![tool_call_entry("tu-1"), tool_call_entry("tu-other")];
    let data = json!({ "content": [{
        "kind": "toolResult",
        "data": { "toolUseId": "tu-1", "status": "success" }
    }] });
    merge_tool_results(&data, &mut out);

    assert_eq!(out[0]["status"], "success");
    assert!(out[1]["status"].is_null(), "the other card is left alone");
}

#[test]
fn drops_a_result_whose_id_matches_nothing() {
    // Documented behaviour: a result with no preceding toolUse cannot be
    // rendered as a card, and it is discarded without touching the rest.
    let mut out = vec![tool_call_entry("tu-1")];
    let data = json!({ "content": [{
        "kind": "toolResult",
        "data": { "toolUseId": "tu-99", "status": "success" }
    }] });
    merge_tool_results(&data, &mut out);

    assert!(out[0]["status"].is_null());
}

#[test]
fn merges_the_most_recent_matching_card_when_an_id_repeats() {
    // Reverse iteration: a re-run of the same tool call patches the latest
    // card, matching the live reducer.
    let mut out = vec![tool_call_entry("tu-1"), tool_call_entry("tu-1")];
    let data = json!({ "content": [{
        "kind": "toolResult",
        "data": { "toolUseId": "tu-1", "status": "success" }
    }] });
    merge_tool_results(&data, &mut out);

    assert!(out[0]["status"].is_null(), "the earlier card is left alone");
    assert_eq!(out[1]["status"], "success");
}
