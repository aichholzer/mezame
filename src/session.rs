//! Session resumption and stale-lock recovery.
//!
//! Kiro writes a `<session-id>.lock` file into `~/.kiro/sessions/cli/`
//! while an ACP process is attached to that session. Two ways this gets
//! in our way:
//!
//! 1. Dead-PID stale lock. A previous Mezame (or Kiro child) was SIGKILLed
//!    before its cooperative shutdown could run. The lockfile persists
//!    pointing at a PID that no longer exists.
//! 2. Live-PID transient contention. Browser reload causes the old WS
//!    handler to begin shutting down Kiro while the new WS handler is
//!    already trying to `session/load`. For a few hundred ms the old Kiro
//!    really is alive and really does own the session.
//!
//! `try_load_session` below handles both: it retries `session/load` with
//! a short back-off while the error is "Session is active in another
//! process", stealing the lockfile whenever the named PID is dead.

use serde_json::{json, Value};

use crate::agent::Agent;

/// Attempt to resume an existing ACP session, recovering from the stale
/// lock / shutdown-race conditions described above.
///
/// On success returns the full `session/load` result so the caller can
/// forward modes/models to the browser just like on `session/new`. On a
/// non-recoverable error, or if retries are exhausted, returns
/// `Err(last_error_message)`.
pub(crate) async fn try_load_session(agent: &Agent, sid: &str, cwd: &str) -> Result<Value, String> {
    // ~1.25s total budget: 5 attempts at 250ms spacing. Empirically covers
    // the cooperative shutdown path (500ms) plus a little headroom for
    // Kiro to actually release the lockfile after its child exits.
    const ATTEMPTS: u32 = 6;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

    let mut last_err = String::new();
    for attempt in 0..ATTEMPTS {
        let res = agent
            .request(
                "session/load",
                json!({
                    "sessionId": sid,
                    "cwd": cwd,
                    "mcpServers": []
                }),
            )
            .await;
        match res {
            Ok(value) => return Ok(value),
            Err(e) => {
                last_err = format!("{e}");
                if !is_stale_lock_error(&last_err) {
                    // Any other error (session truly missing, schema
                    // mismatch, etc.) is not going to fix itself with
                    // retries. Give up immediately.
                    break;
                }
                // Steal the lock if its PID is dead; this always makes
                // the next attempt succeed if the problem was purely a
                // stale lockfile. If the PID is alive we leave the lock
                // alone and let the shutdown-race back-off do its job.
                let stole = steal_stale_session_lock(sid);
                if stole {
                    eprintln!(
                        "Session {sid}: stale lock stolen on attempt {}.",
                        attempt + 1
                    );
                    // Don't burn a backoff sleep if we just cleared the
                    // blocker ourselves.
                    continue;
                }
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(BACKOFF).await;
                }
            }
        }
    }
    Err(last_err)
}

/// True if the agent's error message is the stale-PID lock case.
fn is_stale_lock_error(msg: &str) -> bool {
    msg.contains("Session is active in another process")
}

/// If the lockfile for `session_id` points at a dead PID, remove it and
/// return true. Any uncertainty (lockfile missing, unreadable, malformed,
/// PID still alive) returns false so we fall through to `session/new`.
fn steal_stale_session_lock(session_id: &str) -> bool {
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let path = std::path::PathBuf::from(home).join(format!(".kiro/sessions/cli/{session_id}.lock"));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    // Lockfile shape: {"pid":12345,"started_at":"..."}. Keep the parse
    // narrow; we don't care about the timestamp.
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    let Some(pid) = parsed.get("pid").and_then(Value::as_i64) else {
        return false;
    };
    if pid_is_alive(pid as i32) {
        return false;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {
            eprintln!(
                "Stole stale Kiro session lock (pid {pid}): {}",
                path.display()
            );
            true
        }
        Err(_) => false,
    }
}

/// Unix PID liveness check. `kill(pid, 0)` returns 0 if the process exists
/// and we can signal it, `-1` otherwise. On ESRCH (no such process) the
/// PID is definitely dead; on EPERM the process exists but we can't
/// signal it, which for our case means we should NOT steal the lock.
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    // `kill` with signal 0 does not send a signal, it only queries
    // existence.
    crate::unix::send_signal(pid, 0) == 0
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> bool {
    // Non-unix: don't risk stealing a lock we can't verify.
    true
}

/// Pull the `modes` and `models` blocks out of a `session/new` or
/// `session/load` result. Returns `None` when neither is present so the
/// WS handler can skip emitting the `session_info` event entirely.
///
/// The shape passed through is exactly what Kiro sends, so the browser
/// can key off `currentModeId` / `availableModes` / `currentModelId` /
/// `availableModels` without any translation.
pub(crate) fn extract_session_info(result: &Value) -> Option<Value> {
    let modes = result.get("modes").cloned();
    let models = result.get("models").cloned();
    if modes.is_none() && models.is_none() {
        return None;
    }
    Some(json!({
        "modes": modes,
        "models": models
    }))
}

/// Best-effort one-liner summary of an agent error string for the log.
pub(crate) fn short_reason(msg: &str) -> String {
    // Strip our wrapper and the JSON framing Kiro returns. The interesting
    // bits live in the `data` field of the JSON-RPC error.
    if let Some(start) = msg.find("\"data\":\"") {
        let rest = &msg[start + 8..];
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
        }
    }
    // Fallback: trim the generic prefixes so the user sees something
    // useful rather than three nested quote levels.
    msg.trim_start_matches("agent error: ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // forwarded payload, but the function should still return Some
        // because at least one was set.
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

    #[cfg(unix)]
    #[test]
    fn pid_is_alive_for_self_returns_true() {
        // Our own pid must be alive; this also exercises the unix FFI
        // helper end-to-end.
        let pid = std::process::id() as i32;
        assert!(pid_is_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn pid_is_alive_for_known_dead_pid_returns_false() {
        // PID 0 is the swapper / scheduler on Linux and not signalable
        // from userspace. Negative values are also illegal. We pick a
        // value that is virtually guaranteed not to exist.
        // Using the highest 32-bit positive value keeps us off any real
        // PID without invoking platform-specific PID limits.
        let bogus_pid = i32::MAX;
        assert!(!pid_is_alive(bogus_pid));
    }
}
