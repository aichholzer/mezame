//! Integration tests for `mezame::http::extract_thinking_blocks`.
//!
//! Kiro nests reasoning text one level deeper than plain text blocks:
//! `{ "kind": "thinking", "data": { "text": "..." } }`. Every thinking
//! block in one assistant message collapses into a single history entry,
//! matching the single collapsible the live stream produces.

use mezame::http::extract_thinking_blocks;
use serde_json::json;

#[test]
fn returns_the_text_of_a_single_thinking_block() {
    let data = json!({
        "content": [{ "kind": "thinking", "data": { "text": "let me check" } }]
    });
    assert_eq!(
        extract_thinking_blocks(&data).as_deref(),
        Some("let me check")
    );
}

#[test]
fn joins_several_thinking_blocks_with_a_newline() {
    let data = json!({
        "content": [
            { "kind": "thinking", "data": { "text": "first thought" } },
            { "kind": "thinking", "data": { "text": "second thought" } }
        ]
    });
    assert_eq!(
        extract_thinking_blocks(&data).as_deref(),
        Some("first thought\nsecond thought")
    );
}

#[test]
fn skips_non_thinking_kinds() {
    let data = json!({
        "content": [
            { "kind": "text", "data": "the answer" },
            { "kind": "thinking", "data": { "text": "the reasoning" } },
            { "kind": "toolUse", "data": { "toolUseId": "tu-1" } }
        ]
    });
    assert_eq!(
        extract_thinking_blocks(&data).as_deref(),
        Some("the reasoning")
    );
}

#[test]
fn ignores_an_empty_thinking_text() {
    // An empty string contributes nothing and must not leave a stray
    // separator newline behind.
    let data = json!({
        "content": [
            { "kind": "thinking", "data": { "text": "" } },
            { "kind": "thinking", "data": { "text": "real" } },
            { "kind": "thinking", "data": { "text": "" } }
        ]
    });
    assert_eq!(extract_thinking_blocks(&data).as_deref(), Some("real"));
}

#[test]
fn ignores_a_thinking_block_with_no_nested_text() {
    // `data` absent, `data.text` absent, and `data.text` of the wrong
    // type all resolve to a skipped block.
    let data = json!({
        "content": [
            { "kind": "thinking" },
            { "kind": "thinking", "data": {} },
            { "kind": "thinking", "data": { "text": 42 } },
            { "kind": "thinking", "data": { "text": "kept" } }
        ]
    });
    assert_eq!(extract_thinking_blocks(&data).as_deref(), Some("kept"));
}

#[test]
fn returns_none_when_every_thinking_text_is_empty() {
    let data = json!({
        "content": [
            { "kind": "thinking", "data": { "text": "" } },
            { "kind": "thinking", "data": { "text": "" } }
        ]
    });
    assert!(extract_thinking_blocks(&data).is_none());
}

#[test]
fn returns_none_when_there_is_no_thinking_block() {
    let data = json!({ "content": [{ "kind": "text", "data": "answer only" }] });
    assert!(extract_thinking_blocks(&data).is_none());
}

#[test]
fn returns_none_for_empty_or_absent_content() {
    assert!(extract_thinking_blocks(&json!({ "content": [] })).is_none());
    assert!(extract_thinking_blocks(&json!({})).is_none());
    // `content` present but not an array.
    assert!(extract_thinking_blocks(&json!({ "content": "nope" })).is_none());
}
