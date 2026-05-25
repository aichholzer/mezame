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
