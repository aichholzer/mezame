//! Tests for the lockfile-stealing path of
//! `mezame::session::try_load_session`. Lives in its own integration
//! binary because it has to override `HOME`, which is process-wide.
//! Cargo runs each `tests/*.rs` file as a separate test binary, so the
//! override here cannot bleed into other test files. Within the file,
//! cargo still runs the individual tests concurrently, so we take a
//! file-scoped mutex around every test that touches `HOME` to keep
//! them serialised against each other.

use std::sync::OnceLock;
use std::time::Duration;

use mezame::agent::{from_io, Agent};
use mezame::session::{steal_stale_session_lock, try_load_session};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::time::timeout;

const STALE_LOCK_ERR: &str = "Session is active in another process (pid 1234)";

fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Spawn a short-lived child, wait for it to exit, return its PID. The
/// PID is guaranteed not to be live at the moment of the call: the
/// kernel only reuses PIDs after wrap-around, which (under normal CI
/// load) takes long enough that subsequent `pid_is_alive` checks
/// against the same PID will return false.
///
/// Why not `i32::MAX`: macOS treats out-of-range PIDs as "no such
/// process" so `kill(MAX, 0)` returns ESRCH, but on some Linux
/// configurations the same call returns EPERM, which `pid_is_alive`
/// then reports as "alive" out of conservatism. Using a real reaped
/// PID side-steps the platform difference.
fn reaped_child_pid() -> i64 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn `true`");
    let pid = child.id() as i64;
    let _ = child.wait().expect("wait for `true`");
    pid
}

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
    let _g = home_lock().lock().await;
    timeout(Duration::from_secs(2), async {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let sid = "fixture-session";

        let kiro_dir = home.join(".kiro/sessions/cli");
        std::fs::create_dir_all(&kiro_dir).expect("create kiro dir");
        let lock_path = kiro_dir.join(format!("{sid}.lock"));
        // Use a PID we know is dead: spawn a short-lived child and
        // reap it. After `wait()`, the kernel has freed the PID and
        // (on both Linux and macOS) won't reuse it immediately, since
        // PIDs are allocated sequentially. Using `i32::MAX` here was
        // not portable: some Linux runners treat very large PIDs
        // differently from macOS, and `pid_is_alive` would return a
        // truthy result and the steal would refuse to fire.
        let pid = reaped_child_pid();
        std::fs::write(
            &lock_path,
            json!({ "pid": pid, "started_at": "2026-01-01T00:00:00Z" }).to_string(),
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

// ---------- direct branch coverage of steal_stale_session_lock ----------
//
// The remaining cases call the function directly and never go near
// `try_load_session`, so they don't need the agent fixture above.

/// Set up an isolated `HOME` for the rest of the test and return the
/// lockfile path for `sid`. `HOME` is process-wide; this is safe here
/// because cargo runs each integration test file in its own binary and
/// the tests within share that file-level isolation.
fn isolate_home(sid: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let kiro_dir = tmp.path().join(".kiro/sessions/cli");
    std::fs::create_dir_all(&kiro_dir).expect("create kiro dir");
    std::env::set_var("HOME", tmp.path());
    let lock = kiro_dir.join(format!("{sid}.lock"));
    (tmp, lock)
}

#[tokio::test]
async fn returns_false_when_lockfile_is_missing() {
    let _g = home_lock().lock().await;
    timeout(Duration::from_secs(2), async {
        let (_tmp, lock_path) = isolate_home("missing");
        assert!(!lock_path.exists(), "tempdir should not contain the lock");
        assert!(!steal_stale_session_lock("missing"));
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn returns_false_when_lockfile_is_malformed_json() {
    let _g = home_lock().lock().await;
    timeout(Duration::from_secs(2), async {
        let (_tmp, lock_path) = isolate_home("malformed");
        std::fs::write(&lock_path, b"not json at all").expect("write lock");

        assert!(!steal_stale_session_lock("malformed"));
        assert!(
            lock_path.exists(),
            "malformed lockfile must be preserved, not deleted"
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn returns_false_when_lockfile_has_no_pid_field() {
    let _g = home_lock().lock().await;
    timeout(Duration::from_secs(2), async {
        let (_tmp, lock_path) = isolate_home("nopid");
        std::fs::write(
            &lock_path,
            json!({ "started_at": "2026-01-01T00:00:00Z" }).to_string(),
        )
        .expect("write lock");

        assert!(!steal_stale_session_lock("nopid"));
        assert!(lock_path.exists(), "lockfile without pid must be preserved");
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn returns_false_when_pid_is_alive() {
    let _g = home_lock().lock().await;
    timeout(Duration::from_secs(2), async {
        let (_tmp, lock_path) = isolate_home("live");
        // Use the test process's own pid: it's running this code, so
        // it must be alive, and `pid_is_alive` must return true.
        let pid = std::process::id() as i64;
        std::fs::write(
            &lock_path,
            json!({ "pid": pid, "started_at": "2026-01-01T00:00:00Z" }).to_string(),
        )
        .expect("write lock");

        assert!(!steal_stale_session_lock("live"));
        assert!(
            lock_path.exists(),
            "live-PID lockfile must be preserved (don't kill yourself)"
        );
    })
    .await
    .expect("test timed out");
}
