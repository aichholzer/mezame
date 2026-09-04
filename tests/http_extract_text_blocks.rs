//! Integration tests for `mezame::http::extract_text_blocks`.

use mezame::http::extract_text_blocks;
use serde_json::json;

#[test]
fn concatenates_text_kinds_with_newline() {
    let data = json!({
        "content": [
            { "kind": "text", "data": "first line" },
            { "kind": "text", "data": "second line" }
        ]
    });
    assert_eq!(
        extract_text_blocks(&data).as_deref(),
        Some("first line\nsecond line")
    );
}

#[test]
fn skips_non_text_kinds() {
    let data = json!({
        "content": [
            { "kind": "tool_call", "data": "ignored" },
            { "kind": "text", "data": "kept" },
            { "kind": "image", "data": "ignored too" }
        ]
    });
    assert_eq!(extract_text_blocks(&data).as_deref(), Some("kept"));
}

#[test]
fn returns_none_when_no_text() {
    let data = json!({
        "content": [{ "kind": "tool_call", "data": "x" }]
    });
    assert!(extract_text_blocks(&data).is_none());
}

#[test]
fn returns_none_for_empty_content() {
    let data = json!({ "content": [] });
    assert!(extract_text_blocks(&data).is_none());
}

#[test]
fn returns_none_when_content_missing() {
    let data = json!({});
    assert!(extract_text_blocks(&data).is_none());
}

#[test]
fn ignores_empty_text_blocks() {
    // An empty `data` string contributes nothing and must not leave a
    // stray separator newline behind.
    let data = json!({
        "content": [
            { "kind": "text", "data": "" },
            { "kind": "text", "data": "real" },
            { "kind": "text", "data": "" }
        ]
    });
    assert_eq!(extract_text_blocks(&data).as_deref(), Some("real"));
}

#[test]
fn ignores_text_blocks_whose_data_is_not_a_string() {
    // Kiro only ever writes a string here. A shape change on their side
    // should drop the block and leave the rest of the turn readable.
    let data = json!({
        "content": [
            { "kind": "text", "data": 42 },
            { "kind": "text", "data": { "nested": "no" } },
            { "kind": "text" },
            { "kind": "text", "data": "kept" }
        ]
    });
    assert_eq!(extract_text_blocks(&data).as_deref(), Some("kept"));
}

#[test]
fn returns_none_when_every_text_block_is_empty() {
    let data = json!({
        "content": [
            { "kind": "text", "data": "" },
            { "kind": "text", "data": "" }
        ]
    });
    assert!(extract_text_blocks(&data).is_none());
}

#[test]
fn returns_none_when_content_is_not_an_array() {
    let data = json!({ "content": "not an array" });
    assert!(extract_text_blocks(&data).is_none());
}
