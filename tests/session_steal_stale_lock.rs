//! Tests for the lockfile-stealing path of
//! `mezame::session::try_load_session`. Lives in its own integration
//! binary because it has to override `HOME`, which is process-wide.
//! Cargo runs each `tests/*.rs` file as a separate test binary, so the
//! override here cannot bleed into other test files.

use std::time::Duration;

use mezame::agent::{from_io, Agent};
use mezame::session::try_load_session;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

const STALE_LOCK_ERR: &str = "Session is active in another process (pid 1234)";

fn spawn_fake_agent(responses: Vec<Result<Value, String>>) -> Agent {
    let (server_to_agent, agent_stdin) = tokio::io::duplex(8 * 1024);
    let (agent_stdout, server_reader) = tokio::io::duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);
    std::mem::forget(updates_rx);

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
                None => break,
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

    agent
}

#[tokio::test(start_paused = true)]
async fn dead_pid_lockfile_is_stolen_and_retry_succeeds() {
    timeout(Duration::from_secs(2), async {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let sid = "fixture-session";

        let kiro_dir = home.join(".kiro/sessions/cli");
        std::fs::create_dir_all(&kiro_dir).expect("create kiro dir");
        let lock_path = kiro_dir.join(format!("{sid}.lock"));
        // i32::MAX is virtually guaranteed to be outside the kernel's
        // PID range, so `pid_is_alive` will return false and the lock
        // is eligible for stealing.
        std::fs::write(
            &lock_path,
            json!({ "pid": i32::MAX, "started_at": "2026-01-01T00:00:00Z" }).to_string(),
        )
        .expect("write lockfile");

        // SAFETY: tests in this file are the only consumers of HOME,
        // and the test binary is its own cargo process per the
        // integration-test model.
        std::env::set_var("HOME", home);

        let agent = spawn_fake_agent(vec![
            // First attempt fails with the stale-lock error. Function
            // should call steal_stale_session_lock, find a dead PID,
            // remove the file, and retry.
            Err(STALE_LOCK_ERR.into()),
            Ok(json!({ "sessionId": sid })),
        ]);

        let result = try_load_session(&agent, sid, "/tmp")
            .await
            .expect("retry after steal succeeds");
        assert_eq!(result["sessionId"], sid);
        assert!(
            !lock_path.exists(),
            "lockfile should have been removed by steal"
        );
    })
    .await
    .expect("test timed out");
}
