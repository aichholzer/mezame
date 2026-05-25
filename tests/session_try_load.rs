//! Tests for `mezame::session::try_load_session` retry and back-off
//! behaviour. Lives in its own integration binary so the
//! `tokio::time::pause` mode can drive virtual time without
//! interfering with anything else.
//!
//! The lockfile-stealing path is covered separately in
//! `session_steal_stale_lock.rs` because it has to override `HOME`,
//! and `HOME` is a process-wide environment variable.

use std::time::Duration;

use mezame::agent::{from_io, Agent};
use mezame::session::try_load_session;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

const STALE_LOCK_ERR: &str = "Session is active in another process (pid 1234)";

/// Spawn a fake agent that replies to each `session/load` request with
/// the next item from `responses`. Each item is either an `Ok(Value)`
/// (returns as `result`) or an `Err(String)` (returns as `error.data`).
/// When `responses` runs out, the helper hangs.
fn spawn_fake_agent(
    responses: Vec<Result<Value, String>>,
) -> (Agent, std::sync::Arc<tokio::sync::Notify>) {
    let (server_to_agent, agent_stdin) = tokio::io::duplex(8 * 1024);
    let (agent_stdout, server_reader) = tokio::io::duplex(8 * 1024);
    let (agent, _updates_rx) = from_io(server_to_agent, server_reader);
    // Leak the receiver: we don't care about updates here, but we must
    // keep it alive so the from_io reader doesn't shut its sender.
    std::mem::forget(_updates_rx);

    let done = std::sync::Arc::new(tokio::sync::Notify::new());
    let done_clone = done.clone();

    tokio::spawn(async move {
        let mut stdin = BufReader::new(agent_stdin);
        let mut stdout = agent_stdout;
        let mut iter = responses.into_iter();
        loop {
            let mut line = String::new();
            if stdin.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let req: Value = match serde_json::from_str(line.trim_end()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = req["id"].clone();
            let next = match iter.next() {
                Some(r) => r,
                None => {
                    done_clone.notify_one();
                    break;
                }
            };
            let frame = match next {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(msg) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32000, "message": "Internal", "data": msg }
                }),
            };
            stdout
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        }
    });

    (agent, done)
}

#[tokio::test]
async fn returns_ok_on_first_attempt() {
    timeout(Duration::from_secs(2), async {
        let (agent, _done) =
            spawn_fake_agent(vec![Ok(json!({ "sessionId": "abc", "modes": null }))]);

        let result = try_load_session(&agent, "abc", "/tmp")
            .await
            .expect("first-attempt success");
        assert_eq!(result["sessionId"], "abc");
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn breaks_immediately_on_non_recoverable_error() {
    timeout(Duration::from_secs(2), async {
        let (agent, _done) = spawn_fake_agent(vec![
            Err("Unknown session".into()),
            // If retries kick in this would unblock and pass; the test
            // should never consume this entry.
            Ok(json!({ "sessionId": "abc" })),
        ]);

        let err = try_load_session(&agent, "abc", "/tmp")
            .await
            .expect_err("non-recoverable error must surface");
        assert!(err.contains("Unknown session"), "got: {err}");
    })
    .await
    .expect("test timed out");
}

#[tokio::test(start_paused = true)]
async fn retries_through_stale_lock_until_success() {
    timeout(Duration::from_secs(2), async {
        let (agent, _done) = spawn_fake_agent(vec![
            Err(STALE_LOCK_ERR.into()),
            Err(STALE_LOCK_ERR.into()),
            Ok(json!({ "sessionId": "after-retry" })),
        ]);

        // `try_load_session` will sleep ~250ms between attempts. With
        // virtual time paused (start_paused = true on the runtime), we
        // need to drive the clock forward; but the function does the
        // sleeps inside its own future, which is in the same runtime.
        // tokio's auto-advance handles this for us when no other tasks
        // are runnable.
        let result = try_load_session(&agent, "after-retry", "/tmp")
            .await
            .expect("retried success");
        assert_eq!(result["sessionId"], "after-retry");
    })
    .await
    .expect("test timed out");
}

#[tokio::test(start_paused = true)]
async fn exhausts_attempts_and_returns_last_error() {
    timeout(Duration::from_secs(2), async {
        // Six attempts in the function. Provide six failing responses
        // so the agent never satisfies the request.
        let (agent, _done) = spawn_fake_agent(vec![
            Err(STALE_LOCK_ERR.into()),
            Err(STALE_LOCK_ERR.into()),
            Err(STALE_LOCK_ERR.into()),
            Err(STALE_LOCK_ERR.into()),
            Err(STALE_LOCK_ERR.into()),
            Err(STALE_LOCK_ERR.into()),
        ]);

        let err = try_load_session(&agent, "doomed", "/tmp")
            .await
            .expect_err("attempt budget exhausted");
        assert!(
            err.contains("Session is active in another process"),
            "last error not surfaced: {err}"
        );
    })
    .await
    .expect("test timed out");
}
