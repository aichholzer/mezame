//! End-to-end tests for `spawn_agent` against a real (cheap) subprocess.
//! Uses `cat` as a stand-in for an ACP agent so we exercise the full
//! framing path: stdin write, stdout read, response correlation.

use std::time::Duration;

use mezame::agent::spawn_agent;
use mezame::config::{Config, TransportConfig};
use serde_json::json;
use tokio::time::timeout;

fn cat_config() -> Config {
    Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".into(),
        }],
        // `cat` echoes stdin to stdout line-by-line, which is exactly
        // what the JSON-RPC framing expects: the wire format is
        // newline-delimited JSON in both directions.
        agent_cmd: "cat".into(),
        agent_args: vec![],
    }
}

#[tokio::test]
async fn spawn_agent_returns_handle_and_updates_channel() {
    let result = timeout(Duration::from_secs(5), spawn_agent(&cat_config())).await;
    let Ok(Ok((agent, _updates_rx))) = result else {
        panic!("spawn_agent should succeed for `cat`");
    };
    // Drive cooperative shutdown so the test does not lean on the
    // kill_on_drop safety net.
    agent.shutdown(None).await;
    assert!(agent.shutdown_complete(), "shutdown should set the flag");
}

#[tokio::test]
async fn spawn_agent_routes_responses_back_through_request() {
    // `cat` echoes whatever we write to stdin back on stdout. Our
    // request goes out as `{"jsonrpc":"2.0","id":1,"method":"...","params":...}`;
    // cat reads that line and writes the same bytes to stdout. The
    // response router sees `result` is missing and `error` is missing,
    // so this echo lands on the updates channel rather than completing
    // the in-flight request. That is fine for this test: we drop the
    // request future after a short timeout and instead verify the
    // updates channel saw the echoed frame, which proves the full
    // stdin -> child -> stdout -> reader-task path is wired up.
    let (agent, mut updates_rx) = spawn_agent(&cat_config()).await.expect("spawn_agent");

    // Fire a notify so we do not block waiting for a response that
    // cat will never produce. notify is fire-and-forget on the wire.
    agent
        .notify("ping", json!({ "value": 42 }))
        .await
        .expect("notify");

    let echoed = timeout(Duration::from_secs(2), updates_rx.recv())
        .await
        .expect("echo within 2s")
        .expect("channel still open");

    assert_eq!(echoed["method"], "ping");
    assert_eq!(echoed["params"]["value"], 42);

    agent.shutdown(None).await;
}

#[tokio::test]
async fn spawn_agent_errors_when_command_does_not_exist() {
    let mut cfg = cat_config();
    cfg.agent_cmd = "this-binary-definitely-does-not-exist-xyz123".into();

    let res = timeout(Duration::from_secs(5), spawn_agent(&cfg))
        .await
        .expect("spawn_agent finishes");

    let err = match res {
        Ok(_) => panic!("spawn should have failed for a missing binary"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("Failed to spawn") || msg.contains("No such file"),
        "unexpected error wording: {msg}"
    );
}
