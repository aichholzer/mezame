//! Integration tests for the public helpers in `mezame::session`.

use mezame::session::{extract_session_info, is_stale_lock_error, short_reason};
use serde_json::{json, Value};

#[test]
fn stale_lock_error_recognises_kiro_phrase() {
    assert!(is_stale_lock_error(
        "Agent error: Session is active in another process (pid 1234)"
    ));
}

#[test]
fn stale_lock_error_rejects_unrelated_messages() {
    assert!(!is_stale_lock_error("Agent error: Some other failure"));
    assert!(!is_stale_lock_error(""));
}

#[test]
fn extract_session_info_returns_none_when_absent() {
    let v = json!({ "sessionId": "abc" });
    assert!(extract_session_info(&v).is_none());
}

#[test]
fn extract_session_info_passes_through_modes_and_models() {
    let v = json!({
        "sessionId": "abc",
        "modes": { "currentModeId": "kiro_default", "availableModes": [] },
        "models": { "currentModelId": "claude-sonnet", "availableModels": [] }
    });
    let info = extract_session_info(&v).expect("info");
    assert_eq!(
        info.get("modes").and_then(|m| m.get("currentModeId")),
        Some(&json!("kiro_default"))
    );
    assert_eq!(
        info.get("models").and_then(|m| m.get("currentModelId")),
        Some(&json!("claude-sonnet"))
    );
}

#[test]
fn extract_session_info_includes_a_present_field_with_other_null() {
    // Only `modes` present: `models` should land as JSON null in the
    // forwarded payload, but the function should still return Some.
    let v = json!({ "modes": { "currentModeId": "x" } });
    let info = extract_session_info(&v).expect("info");
    assert!(info.get("modes").is_some());
    assert_eq!(info.get("models"), Some(&Value::Null));
}

#[test]
fn short_reason_unwraps_jsonrpc_data_field() {
    let raw = "Agent error: {\"code\":-32603,\"message\":\"Internal\",\"data\":\"Session is active in another process (pid 1234)\"}";
    assert_eq!(
        short_reason(raw),
        "Session is active in another process (pid 1234)"
    );
}

#[test]
fn short_reason_falls_back_to_trimmed_message() {
    assert_eq!(short_reason("agent error: boom"), "boom");
    assert_eq!(short_reason("plain message"), "plain message");
}
