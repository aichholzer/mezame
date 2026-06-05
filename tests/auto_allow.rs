//! Tests for the auto-allow-permissions feature.
//!
//! Two layers are covered:
//!   - the pure helpers (`pick_allow_option`, `auto_allow_from_state`),
//!     which need no IO and pin the option-selection and state-parsing
//!     conventions, and
//!   - the hub interception end-to-end: with the flag on, a
//!     `session/request_permission` is answered straight back to the
//!     agent with an allow option and no `permission_request` card is
//!     broadcast to subscribers; with the flag off it is forwarded as a
//!     card exactly as before.
//!
//! The hub reads the flag from `state.json` under `$HOME`, a
//! process-global. Tests that set it take a file-scoped mutex (same
//! pattern as `tests/config_paths.rs`) and write a real state file into
//! a `TempDir` so the read path is exercised for real.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use mezame::agent::{from_io, Agent};
use mezame::config::{auto_allow_from_state, read_auto_allow_permissions};
use mezame::hub::{pick_allow_option, HubRegistry};
use serde_json::{json, Value};
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::timeout;

const SESSION_ID: &str = "test-session";

// ---------- pure helpers ----------

#[test]
fn pick_allow_option_prefers_allow_once() {
    let msg = json!({
        "params": { "options": [
            { "optionId": "rej", "name": "Reject", "kind": "reject_once" },
            { "optionId": "ok", "name": "Allow", "kind": "allow_once" }
        ] }
    });
    assert_eq!(pick_allow_option(&msg).as_deref(), Some("ok"));
}

#[test]
fn pick_allow_option_accepts_allow_always() {
    let msg = json!({
        "params": { "options": [
            { "optionId": "always", "name": "Always allow", "kind": "allow_always" }
        ] }
    });
    assert_eq!(pick_allow_option(&msg).as_deref(), Some("always"));
}

#[test]
fn pick_allow_option_falls_back_to_name_when_kind_absent() {
    // No `kind` on either option: match on the human name containing
    // "allow" (case-insensitive).
    let msg = json!({
        "params": { "options": [
            { "optionId": "no", "name": "Deny" },
            { "optionId": "yes", "name": "ALLOW this" }
        ] }
    });
    assert_eq!(pick_allow_option(&msg).as_deref(), Some("yes"));
}

#[test]
fn pick_allow_option_returns_none_when_no_allow() {
    // Reject-only set: we never auto-pick a reject, so the caller is
    // told to prompt the human instead.
    let msg = json!({
        "params": { "options": [
            { "optionId": "r1", "name": "Reject", "kind": "reject_once" },
            { "optionId": "r2", "name": "Reject always", "kind": "reject_always" }
        ] }
    });
    assert_eq!(pick_allow_option(&msg), None);
}

#[test]
fn pick_allow_option_ignores_non_allow_kind_even_if_name_says_allow() {
    // An explicit non-allow `kind` wins over the name heuristic: the
    // name fallback only applies when `kind` is absent.
    let msg = json!({
        "params": { "options": [
            { "optionId": "trap", "name": "Allow", "kind": "reject_once" }
        ] }
    });
    assert_eq!(pick_allow_option(&msg), None);
}

#[test]
fn pick_allow_option_returns_none_on_malformed() {
    assert_eq!(pick_allow_option(&json!({})), None);
    assert_eq!(pick_allow_option(&json!({ "params": {} })), None);
    assert_eq!(
        pick_allow_option(&json!({ "params": { "options": [] } })),
        None
    );
}

#[test]
fn auto_allow_from_state_reads_nested_flag() {
    assert!(auto_allow_from_state(
        &json!({ "settings": { "autoAllowPermissions": true } })
    ));
    assert!(!auto_allow_from_state(
        &json!({ "settings": { "autoAllowPermissions": false } })
    ));
}

#[test]
fn auto_allow_from_state_defaults_false_when_absent() {
    assert!(!auto_allow_from_state(&json!({})));
    assert!(!auto_allow_from_state(&json!({ "settings": {} })));
    // Wrong type → default, not a panic.
    assert!(!auto_allow_from_state(
        &json!({ "settings": { "autoAllowPermissions": "yes" } })
    ));
}

// ---------- state.json read path ----------

fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Point `$HOME` at `dir` and write `state.json` with the given
/// settings body (or nothing when `body` is `None`).
fn seed_state(dir: &std::path::Path, body: Option<&Value>) {
    std::env::set_var("HOME", dir);
    let mezame_dir = dir.join(".mezame");
    std::fs::create_dir_all(&mezame_dir).unwrap();
    if let Some(v) = body {
        std::fs::write(
            mezame_dir.join("state.json"),
            serde_json::to_string_pretty(v).unwrap(),
        )
        .unwrap();
    }
}

#[tokio::test]
async fn read_auto_allow_true_when_state_says_so() {
    let _g = home_lock().lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    seed_state(
        tmp.path(),
        Some(&json!({ "settings": { "autoAllowPermissions": true } })),
    );
    assert!(read_auto_allow_permissions().await);
}

#[tokio::test]
async fn read_auto_allow_false_when_file_missing() {
    let _g = home_lock().lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    seed_state(tmp.path(), None); // dir exists, no state.json
    assert!(!read_auto_allow_permissions().await);
}

#[tokio::test]
async fn read_auto_allow_false_on_malformed_json() {
    let _g = home_lock().lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    std::env::set_var("HOME", tmp.path());
    let mezame_dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&mezame_dir).unwrap();
    std::fs::write(mezame_dir.join("state.json"), "{ not json").unwrap();
    assert!(!read_auto_allow_permissions().await);
}

// ---------- hub interception end-to-end ----------

/// Build an `Agent` from a duplex pipe. The returned `agent_stdin` lets
/// the test read back exactly what the hub wrote to the agent (used to
/// assert the auto-allow `respond`). `inject_tx` pushes agent→mezame
/// frames. No auto-reply machinery here: the only thing we write to the
/// agent in these tests is a permission request, and we want to read
/// the hub's reply verbatim.
fn make_agent() -> (
    Agent,
    mpsc::UnboundedReceiver<Value>,
    mpsc::UnboundedSender<Value>,
    tokio::io::DuplexStream,
) {
    let (server_to_agent, agent_stdin) = duplex(8 * 1024);
    let (agent_stdout, server_reader) = duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);

    let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut stdout = agent_stdout;
        while let Some(value) = inject_rx.recv().await {
            let line = format!("{value}\n");
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    (agent, updates_rx, inject_tx, agent_stdin)
}

fn ready_event() -> Value {
    json!({
        "type": "ready",
        "sessionId": SESSION_ID,
        "resumed": false,
        "cwd": "/tmp",
        "promptCapabilities": {},
        "buildId": "test"
    })
}

fn permission_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "session/request_permission",
        "params": {
            "sessionId": SESSION_ID,
            "toolCall": { "title": "Write file", "kind": "edit" },
            "options": [
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
            ]
        }
    })
}

#[tokio::test]
async fn auto_allow_on_answers_agent_and_suppresses_card() {
    let _g = home_lock().lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    seed_state(
        tmp.path(),
        Some(&json!({ "settings": { "autoAllowPermissions": true } })),
    );

    let registry = HubRegistry::new();
    let (agent, updates_rx, inject, agent_stdin) = make_agent();
    let mut attached = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;

    inject.send(permission_request()).expect("inject");

    // The hub should write a JSON-RPC response selecting the allow
    // option straight back to the agent's stdin.
    let mut reader = BufReader::new(agent_stdin);
    let mut line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("agent received a reply within 2s")
        .expect("read ok");
    let reply: Value = serde_json::from_str(line.trim()).expect("valid JSON-RPC");
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
    assert_eq!(reply["result"]["outcome"]["optionId"], "allow");

    // And no permission_request card should ever be broadcast.
    let broadcast = timeout(Duration::from_millis(300), attached.outbound.recv()).await;
    assert!(
        broadcast.is_err(),
        "no card should be broadcast when auto-allow is on, got {:?}",
        broadcast
    );
}

#[tokio::test]
async fn auto_allow_off_forwards_card_as_before() {
    let _g = home_lock().lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    // Explicitly off.
    seed_state(
        tmp.path(),
        Some(&json!({ "settings": { "autoAllowPermissions": false } })),
    );

    let registry = HubRegistry::new();
    let (agent, updates_rx, inject, _agent_stdin) = make_agent();
    let mut attached = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;

    inject.send(permission_request()).expect("inject");

    let event = timeout(Duration::from_secs(2), attached.outbound.recv())
        .await
        .expect("a card is broadcast within 2s when auto-allow is off")
        .expect("channel open");
    assert_eq!((*event)["type"], "permission_request");
    assert_eq!((*event)["id"], 7);
}
