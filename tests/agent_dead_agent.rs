//! Regression tests for GitHub issue #9: an agent that dies with a
//! request outstanding used to leave that request waiting forever.
//!
//! The chain the hang produced: `Agent::request` parks on a oneshot whose
//! sender sits in the shared pending table, and the stdout reader task
//! ended at EOF without touching that table. The sender was never dropped. The blocked future holds an `Arc<Agent>`, the `Agent` holds the
//! `tokio::process::Child`, and tokio reaps a `Child` only when the `Child`
//! is dropped. The pid therefore stayed `<defunct>` for the life of the
//! process, one slot per failed session against the service's `TasksMax`.
//!
//! Two triggers empty that table: the stdout reader at EOF, and the reaper
//! task when the process exits. The second covers a grandchild holding the
//! agent's stdout open, where EOF never arrives.
//!
//! Each test puts a hard timeout around the call under test. A regression
//! shows up as that timeout expiring, which is the failure the field report
//! describes.

use std::sync::Arc;
use std::time::Duration;

use mezame::agent::{from_io, spawn_agent, Agent};
use mezame::config::{Config, TransportConfig};
use mezame::session::pid_is_alive;
use serde_json::{json, Value};
use tokio::io::duplex;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Longer than any of these tests needs. Reaching it means something hung.
const PATIENCE: Duration = Duration::from_secs(10);

fn sh(script: &str) -> Config {
    Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".to_string(),
        }],
        agent_cmd: "/bin/sh".to_string(),
        agent_args: vec!["-c".to_string(), script.to_string()],
    }
}

/// Start a shell "agent" that announces its own pid on stdout before doing
/// whatever `after` says. The pid arrives through the updates channel, where
/// any line that is not a JSON-RPC response lands.
///
/// Reading the pid off the child is what lets these tests check reaping
/// without `Agent` exposing its child pid. The receiver comes back so the
/// caller can hold it for the length of the test, as the hub does in
/// production.
async fn start(after: &str) -> (Agent, i32, mpsc::UnboundedReceiver<Value>) {
    let script = format!(r#"echo "{{\"pid\":$$}}"; {after}"#);
    let (agent, mut updates) = spawn_agent(&sh(&script)).await.expect("spawn /bin/sh");
    let msg = timeout(PATIENCE, updates.recv())
        .await
        .expect("the child announces its pid")
        .expect("updates channel open");
    let pid = msg["pid"].as_i64().expect("pid in the announcement") as i32;
    (agent, pid, updates)
}

/// Poll until `pid` is gone. Reaping runs on a tokio task, and a single
/// check would race it. Returns false when the pid outlives the bound.
async fn wait_until_reaped(pid: i32) -> bool {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline {
        if !pid_is_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn request_fails_when_the_agent_dies_with_it_in_flight() {
    // The exact shape of the field report. The child execs cleanly, our
    // `initialize` write lands in the pipe, and only then does the child die
    // without answering. Under an exhausted `TasksMax` that is `kiro-cli
    // acp` panicking on thread creation with EAGAIN a moment after exec.
    let (agent, _pid, _updates) = start("sleep 0.3; exit 101").await;

    let result = timeout(PATIENCE, agent.request("initialize", json!({})))
        .await
        .expect("request must resolve when the agent dies, and not hang");

    let err = result.expect_err("a dead agent cannot have answered");
    assert!(
        err.to_string().contains("closed before replying"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn every_outstanding_request_fails_when_the_agent_dies() {
    // The pending table can hold several waiters. All of them have to wake.
    let (agent, _pid, _updates) = start("sleep 0.3; exit 101").await;
    let agent = Arc::new(agent);

    let mut handles = Vec::new();
    for _ in 0..4 {
        let agent = Arc::clone(&agent);
        handles.push(tokio::spawn(async move {
            agent.request("session/new", json!({ "cwd": "/tmp" })).await
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        let result = timeout(PATIENCE, handle)
            .await
            .unwrap_or_else(|_| panic!("request {i} hung"))
            .expect("task did not panic");
        assert!(result.is_err(), "request {i} should have failed");
    }
}

#[tokio::test]
async fn a_request_raised_after_the_agent_closed_fails_at_once() {
    // Once the reader has seen EOF the pending table is gone, and a fresh
    // request has to fail without writing to a dead pipe.
    let (agent, _pid, mut updates) = start("exit 101").await;

    // Sync point with no sleep in it. The reader task empties the pending
    // table and then returns, dropping the updates sender as it goes. A
    // `None` here means the table is already gone.
    let ended = timeout(PATIENCE, updates.recv())
        .await
        .expect("the reader task ends when stdout closes");
    assert!(ended.is_none(), "the updates channel should be closed");

    let result = timeout(PATIENCE, agent.request("initialize", json!({})))
        .await
        .expect("a closed agent must reject a new request immediately");
    let err = result.expect_err("request against a closed agent must fail");
    assert!(
        err.to_string().contains("closed before replying"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn the_child_is_reaped_once_the_failed_request_releases_the_agent() {
    // End to end for the leak. The request resolves, its caller drops the
    // handle, and the pid disappears. Before the fix the request never
    // resolved, this scope never exited, and the pid stayed `<defunct>`.
    let pid = {
        let (agent, pid, _updates) = start("sleep 0.3; exit 101").await;
        let result = timeout(PATIENCE, agent.request("initialize", json!({})))
            .await
            .expect("request resolves");
        assert!(result.is_err());
        pid
    };

    assert!(
        wait_until_reaped(pid).await,
        "pid {pid} is still present: the child was never collected"
    );
}

#[tokio::test]
async fn shutdown_collects_the_child_that_ignores_stdin_eof() {
    // `shutdown` waits briefly for a cooperative exit, kills the group, then
    // waits again. Without that second wait the pid stays `<defunct>` until
    // the `Agent` itself is dropped, which in production can be a long way
    // off. `sh -c` took its script from the command line and reads nothing
    // from stdin. The EOF passes it by.
    let (agent, pid, _updates) = start("sleep 60").await;

    timeout(PATIENCE, agent.shutdown(Some("s1")))
        .await
        .expect("shutdown completes");

    assert!(
        !pid_is_alive(pid),
        "pid {pid} should be collected by the time shutdown returns"
    );
    // The handle is deliberately still alive above: the point is that
    // holding it no longer keeps a dead child around.
    drop(agent);
}

#[tokio::test]
async fn from_io_fails_an_outstanding_request_when_stdout_ends() {
    // The test constructor shares the production reader's behaviour. A
    // suite driving `from_io` sees the same closed-agent semantics.
    let (server_to_agent, _agent_stdin) = duplex(8 * 1024);
    let (agent_stdout, server_reader) = duplex(8 * 1024);
    let (agent, _updates) = from_io(server_to_agent, server_reader);
    let agent = Arc::new(agent);

    let pending = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move { agent.request("initialize", json!({})).await })
    };

    // Let the request register before the stream ends.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(agent_stdout);

    let result = timeout(PATIENCE, pending)
        .await
        .expect("the request must resolve when stdout ends")
        .expect("task did not panic");
    assert!(result.is_err(), "a closed stdout must fail the request");
}

#[tokio::test]
async fn from_io_rejects_a_request_raised_after_stdout_ended() {
    let (server_to_agent, _agent_stdin) = duplex(8 * 1024);
    let (agent_stdout, server_reader) = duplex(8 * 1024);
    let (agent, _updates) = from_io(server_to_agent, server_reader);

    drop(agent_stdout);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let result = timeout(PATIENCE, agent.request("initialize", json!({})))
        .await
        .expect("must not hang");
    let err = result.expect_err("a closed agent cannot answer");
    assert!(
        err.to_string().contains("closed before replying"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn a_request_fails_when_the_child_exits_with_its_stdout_held_open() {
    // The case stdout EOF cannot cover, and the reason the `Child` lives in
    // a reaper task. The agent forks a grandchild that inherits its stdout
    // and then exits. The write end of that pipe stays open. The reader
    // task sits on a stream that never ends, and only the child's own exit
    // says the request can never be answered.
    //
    // MCP servers launched through `npx`/`npm` are this shape, which is what
    // makes it worth covering.
    let (agent, pid, _updates) = start("sleep 30 & sleep 0.3; exit 0").await;

    let result = timeout(PATIENCE, agent.request("initialize", json!({})))
        .await
        .expect("the child's exit must resolve the request, and not hang");
    let err = result.expect_err("a dead child cannot have answered");
    assert!(
        err.to_string().contains("closed before replying"),
        "unexpected error: {err}"
    );

    // The direct child is collected even though its stdout is still held.
    assert!(
        wait_until_reaped(pid).await,
        "pid {pid} outlived its own exit"
    );
}

#[tokio::test]
async fn the_child_is_collected_as_soon_as_it_exits() {
    // The reaper parks on `child.wait()`. The pid is released when the
    // process exits. Holding the `Agent` afterwards no longer keeps a
    // `<defunct>` entry against the service's `TasksMax`, which is the
    // behaviour issue #9 reported.
    let (agent, pid, _updates) = start("exit 101").await;

    assert!(
        wait_until_reaped(pid).await,
        "pid {pid} is still present while the Agent is held"
    );
    // Held deliberately across the assertion above.
    drop(agent);
}
