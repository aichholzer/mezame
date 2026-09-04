//! Integration tests for `mezame::http::find_tool_result`.
//!
//! The scanner walks a Kiro session JSONL backwards looking for the
//! `toolResult` block matching a `toolUseId`. It is what `GET
//! /tool-result` answers from, and the browser polls that endpoint after a
//! live `tool_call_update` flipped a tool to `completed` without streaming
//! any content. Every skip below decides between the user seeing a result
//! and seeing a 404, which is why each one is pinned separately.

use mezame::http::find_tool_result;
use serde_json::json;

/// A well-formed `ToolResults` line for `id`, with `status` and a single
/// text content block.
fn results_line(id: &str, status: &str, text: &str) -> String {
    json!({
        "kind": "ToolResults",
        "data": { "content": [{
            "kind": "toolResult",
            "data": {
                "toolUseId": id,
                "status": status,
                "content": [{ "kind": "text", "data": text }]
            }
        }] }
    })
    .to_string()
}

#[test]
fn returns_status_and_content_for_a_matching_id() {
    let raw = results_line("tu-1", "success", "the answer");
    let found = find_tool_result(&raw, "tu-1").expect("tu-1 should be found");
    assert_eq!(found["status"], "success");
    assert_eq!(found["content"][0]["data"], "the answer");
}

#[test]
fn returns_none_for_an_empty_document() {
    assert!(find_tool_result("", "tu-1").is_none());
    assert!(find_tool_result("\n\n   \n", "tu-1").is_none());
}

#[test]
fn skips_blank_lines_and_unparseable_json() {
    // The noise sits after the match in file order. The scan runs
    // backwards, and this arrangement makes it walk over every skip
    // before reaching the result.
    let raw = format!(
        "{}\n\n   \nnot json at all\n{{ unterminated\n\n",
        results_line("tu-2", "success", "ok")
    );
    let found = find_tool_result(&raw, "tu-2").expect("tu-2 should survive the noise");
    assert_eq!(found["status"], "success");
}

#[test]
fn skips_entries_that_are_not_tool_results() {
    // Prompt and AssistantMessage lines sit between the results in a real
    // log, including a toolUse block whose id matches the one being asked
    // about.
    let raw = format!(
        "{}\n{}\n{}\n",
        results_line("tu-3", "success", "found it"),
        json!({ "kind": "Prompt", "data": { "content": [] } }),
        json!({ "kind": "AssistantMessage", "data": { "content": [{
            "kind": "toolUse",
            "data": { "toolUseId": "tu-3", "name": "web_search" }
        }] } })
    );
    let found = find_tool_result(&raw, "tu-3").expect("tu-3 should be found");
    assert_eq!(found["content"][0]["data"], "found it");
}

#[test]
fn skips_a_tool_results_entry_with_no_content_array() {
    // `data.content` absent, and `data.content` present but not an array.
    let raw = format!(
        "{}\n{}\n{}\n",
        results_line("tu-4", "success", "still here"),
        json!({ "kind": "ToolResults", "data": {} }),
        json!({ "kind": "ToolResults", "data": { "content": "not an array" } })
    );
    let found = find_tool_result(&raw, "tu-4").expect("tu-4 should be found");
    assert_eq!(found["content"][0]["data"], "still here");
}

#[test]
fn skips_blocks_that_are_not_tool_results() {
    // A `ToolResults` entry can hold blocks of other kinds alongside the
    // one being looked for.
    let raw = json!({
        "kind": "ToolResults",
        "data": { "content": [
            { "kind": "text", "data": "chatter" },
            { "kind": "toolResult", "data": {
                "toolUseId": "tu-5", "status": "error", "content": []
            } }
        ] }
    })
    .to_string();
    let found = find_tool_result(&raw, "tu-5").expect("tu-5 should be found");
    assert_eq!(found["status"], "error");
}

#[test]
fn skips_a_tool_result_block_with_no_data() {
    let raw = format!(
        "{}\n",
        json!({
            "kind": "ToolResults",
            "data": { "content": [
                { "kind": "toolResult" },
                { "kind": "toolResult", "data": {
                    "toolUseId": "tu-6", "status": "success", "content": []
                } }
            ] }
        })
    );
    let found = find_tool_result(&raw, "tu-6").expect("tu-6 should be found");
    assert_eq!(found["status"], "success");
}

#[test]
fn returns_none_when_no_id_matches() {
    // The document holds results, none of them the one asked about. This
    // is the 404 the browser polls against while Kiro has yet to flush the
    // turn.
    let raw = format!(
        "{}\n{}\n",
        results_line("tu-7", "success", "a"),
        results_line("tu-8", "success", "b")
    );
    assert!(find_tool_result(&raw, "tu-9").is_none());
}

#[test]
fn returns_the_last_match_when_an_id_repeats() {
    // A turn that re-ran the same tool leaves the earlier result in the
    // file. The scanner walks backwards and the newest one wins, matching
    // what the live stream would have shown.
    let raw = format!(
        "{}\n{}\n",
        results_line("tu-10", "error", "first attempt"),
        results_line("tu-10", "success", "second attempt")
    );
    let found = find_tool_result(&raw, "tu-10").expect("tu-10 should be found");
    assert_eq!(found["status"], "success");
    assert_eq!(found["content"][0]["data"], "second attempt");
}

#[test]
fn missing_status_and_content_come_back_as_null() {
    // Both fields are optional on the wire. The endpoint reports null for
    // an absent one, and the client renders an empty card.
    let raw = json!({
        "kind": "ToolResults",
        "data": { "content": [{
            "kind": "toolResult",
            "data": { "toolUseId": "tu-11" }
        }] }
    })
    .to_string();
    let found = find_tool_result(&raw, "tu-11").expect("tu-11 should be found");
    assert!(found["status"].is_null());
    assert!(found["content"].is_null());
}
