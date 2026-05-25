//! Round-trip tests for the JSON-RPC framing in `mezame::agent`.
//!
//! Builds an `Agent` from a `tokio::io::duplex` pipe, drives the wire
//! by reading bytes the framing helpers wrote and by writing canned
//! responses back. No real subprocess.

use std::time::Duration;

use mezame::agent::from_io;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

/// Convenience: build an agent and the test-side handles for its stdio.
/// `agent_stdin` is what the agent writes to (we read from it to see
/// the framing); `agent_stdout` is what we write to in order to drive
/// the response router.
fn pipes() -> (
    mezame::agent::Agent,
    UnboundedReceiver<Value>,
    BufReader<tokio::io::DuplexStream>,
    tokio::io::DuplexStream,
) {
    let (server_to_agent, agent_stdin) = tokio::io::duplex(8 * 1024);
    let (agent_stdout, server_reader) = tokio::io::duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);
    (agent, updates_rx, BufReader::new(agent_stdin), agent_stdout)
}

/// Read one newline-delimited JSON message off the agent's stdin.
async fn read_one_line(stdin: &mut BufReader<tokio::io::DuplexStream>) -> Value {
    let mut line = String::new();
    stdin.read_line(&mut line).await.expect("read failed");
    assert!(
        line.ends_with('\n'),
        "framed message did not end with a newline: {line:?}"
    );
    serde_json::from_str(line.trim_end()).expect("agent stdin emitted invalid JSON")
}

#[tokio::test]
async fn request_happy_path_returns_result() {
    timeout(Duration::from_secs(2), async {
        let (agent, _updates_rx, mut agent_stdin, mut agent_stdout) = pipes();

        let request_fut =
            tokio::spawn(async move { agent.request("session/new", json!({"cwd": "/"})).await });

        let framed = read_one_line(&mut agent_stdin).await;
        assert_eq!(framed["jsonrpc"], "2.0");
        assert_eq!(framed["method"], "session/new");
        assert_eq!(framed["params"], json!({"cwd": "/"}));
        let id = framed["id"].as_i64().expect("numeric id");
        assert_eq!(id, 1, "first request must allocate id=1");

        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": { "sessionId": "abc" } });
        agent_stdout
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();

        let result = request_fut.await.unwrap().expect("request returned Ok");
        assert_eq!(result, json!({ "sessionId": "abc" }));
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn request_error_response_propagates_to_caller() {
    timeout(Duration::from_secs(2), async {
        let (agent, _updates_rx, mut agent_stdin, mut agent_stdout) = pipes();

        let request_fut = tokio::spawn(async move { agent.request("doomed", json!({})).await });

        let framed = read_one_line(&mut agent_stdin).await;
        let id = framed["id"].as_i64().unwrap();

        let reply = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": "Internal", "data": "specifics" }
        });
        agent_stdout
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();

        let err = request_fut
            .await
            .unwrap()
            .expect_err("request should have errored");
        let s = format!("{err}");
        assert!(s.contains("specifics"), "error did not carry data: {s}");
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn respond_writes_well_formed_result_line() {
    timeout(Duration::from_secs(2), async {
        let (agent, _updates_rx, mut agent_stdin, _agent_stdout) = pipes();

        agent
            .respond(json!(7), json!({ "ok": true }))
            .await
            .unwrap();

        let framed = read_one_line(&mut agent_stdin).await;
        assert_eq!(framed["jsonrpc"], "2.0");
        assert_eq!(framed["id"], 7);
        assert_eq!(framed["result"], json!({ "ok": true }));
        assert!(
            framed.get("method").is_none(),
            "respond should not carry a method"
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn notify_writes_line_without_id() {
    timeout(Duration::from_secs(2), async {
        let (agent, _updates_rx, mut agent_stdin, _agent_stdout) = pipes();

        agent
            .notify("session/cancel", json!({ "sessionId": "abc" }))
            .await
            .unwrap();

        let framed = read_one_line(&mut agent_stdin).await;
        assert_eq!(framed["jsonrpc"], "2.0");
        assert_eq!(framed["method"], "session/cancel");
        assert_eq!(framed["params"], json!({ "sessionId": "abc" }));
        assert!(
            framed.get("id").is_none(),
            "notify must not carry an id (got {:?})",
            framed.get("id")
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn out_of_order_responses_route_to_correct_futures() {
    timeout(Duration::from_secs(2), async {
        let (agent, _updates_rx, mut agent_stdin, mut agent_stdout) = pipes();
        let agent = std::sync::Arc::new(agent);

        // Fire two concurrent requests. The framing helper assigns ids
        // monotonically (1 then 2) so we know what to expect on the
        // wire regardless of when the futures resolve.
        let a1 = agent.clone();
        let fut1 = tokio::spawn(async move { a1.request("m1", json!({})).await });
        let a2 = agent.clone();
        let fut2 = tokio::spawn(async move { a2.request("m2", json!({})).await });

        let framed1 = read_one_line(&mut agent_stdin).await;
        let framed2 = read_one_line(&mut agent_stdin).await;
        assert_eq!(framed1["id"], 1);
        assert_eq!(framed2["id"], 2);

        // Reply to id=2 first, then id=1. Both futures must still
        // resolve with their own results.
        let reply2 = json!({ "jsonrpc": "2.0", "id": 2, "result": "second" });
        let reply1 = json!({ "jsonrpc": "2.0", "id": 1, "result": "first" });
        agent_stdout
            .write_all(format!("{reply2}\n{reply1}\n").as_bytes())
            .await
            .unwrap();

        let r1 = fut1.await.unwrap().expect("fut1");
        let r2 = fut2.await.unwrap().expect("fut2");
        assert_eq!(r1, json!("first"));
        assert_eq!(r2, json!("second"));
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn unmatched_messages_land_on_updates_channel() {
    timeout(Duration::from_secs(2), async {
        let (_agent, mut updates_rx, _agent_stdin, mut agent_stdout) = pipes();

        // A notification (no id) and a response with an id we never
        // sent: both must end up on the updates channel.
        let notif = json!({ "jsonrpc": "2.0", "method": "session/update", "params": {} });
        let stray = json!({ "jsonrpc": "2.0", "id": 999, "result": "orphan" });
        agent_stdout
            .write_all(format!("{notif}\n{stray}\n").as_bytes())
            .await
            .unwrap();

        let first = updates_rx.recv().await.expect("updates dropped");
        let second = updates_rx.recv().await.expect("updates dropped");

        assert_eq!(first["method"], "session/update");
        assert_eq!(second["id"], 999);
        assert_eq!(second["result"], "orphan");
    })
    .await
    .expect("test timed out");
}
