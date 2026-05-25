//! Integration tests for `mezame::http::parse_kiro_history`.

use mezame::http::parse_kiro_history;
use serde_json::{json, Value};

#[test]
fn pairs_prompt_with_assistant_reply() {
    let raw = concat!(
        r#"{"kind":"Prompt","data":{"content":[{"kind":"text","data":"hello"}],"meta":{"timestamp":1700000000}}}"#,
        "\n",
        r#"{"kind":"AssistantMessage","data":{"content":[{"kind":"text","data":"hi back"}]}}"#,
        "\n",
    );
    let entries = parse_kiro_history(raw);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].get("role"), Some(&json!("user")));
    assert_eq!(entries[0].get("text"), Some(&json!("hello")));
    // Timestamp converted from seconds to ms.
    assert_eq!(
        entries[0].get("timestamp"),
        Some(&json!(1_700_000_000_000_i64))
    );
    assert_eq!(entries[1].get("role"), Some(&json!("agent")));
    assert_eq!(entries[1].get("text"), Some(&json!("hi back")));
    // Assistant inherits the prompt's timestamp.
    assert_eq!(
        entries[1].get("timestamp"),
        Some(&json!(1_700_000_000_000_i64))
    );
}

#[test]
fn skips_unknown_kinds_and_blank_lines() {
    let raw = concat!(
        "\n",
        r#"{"kind":"ToolResults","data":{}}"#,
        "\n",
        r#"{"kind":"Prompt","data":{"content":[{"kind":"text","data":"q"}],"meta":{"timestamp":1}}}"#,
        "\n",
        "   \n",
        r#"{"kind":"AssistantMessage","data":{"content":[{"kind":"text","data":"a"}]}}"#,
        "\n",
    );
    let entries = parse_kiro_history(raw);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].get("role"), Some(&json!("user")));
    assert_eq!(entries[1].get("role"), Some(&json!("agent")));
}

#[test]
fn skips_malformed_json_lines() {
    let raw = concat!(
        "this is not json\n",
        r#"{"kind":"Prompt","data":{"content":[{"kind":"text","data":"q"}],"meta":{"timestamp":2}}}"#,
        "\n",
    );
    let entries = parse_kiro_history(raw);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].get("text"), Some(&json!("q")));
}

#[test]
fn assistant_before_any_prompt_has_null_timestamp() {
    // Documented edge case: if the on-disk log starts with an assistant
    // message, no Prompt has set a baseline timestamp yet, so the entry
    // is emitted with `timestamp: null`.
    let raw = r#"{"kind":"AssistantMessage","data":{"content":[{"kind":"text","data":"orphan"}]}}"#;
    let entries = parse_kiro_history(raw);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].get("role"), Some(&json!("agent")));
    assert_eq!(entries[0].get("timestamp"), Some(&Value::Null));
}

#[test]
fn drops_prompt_with_no_text_blocks() {
    // A prompt that contained only an image (no text content) has
    // nothing useful to render in history.
    let raw = concat!(
        r#"{"kind":"Prompt","data":{"content":[{"kind":"image","data":"..."}],"meta":{"timestamp":1}}}"#,
        "\n",
    );
    let entries = parse_kiro_history(raw);
    assert!(entries.is_empty());
}

#[test]
fn handles_empty_input() {
    assert!(parse_kiro_history("").is_empty());
    assert!(parse_kiro_history("\n\n  \n").is_empty());
}
