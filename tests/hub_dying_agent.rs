//! Hub-level regression tests for GitHub issue #9.
//!
//! `attach_or_create` reaches `build_hub`, which spawns the agent and runs
//! the ACP handshake. Two ways that handshake can fail to finish, and both
//! used to hold the future forever:
//!
//! - the agent dies without answering, covered by the pending-table fix in
//!   `agent.rs` and asserted in `tests/agent_dead_agent.rs`
//! - the agent stays alive and answers nothing, covered by
//!   `NEGOTIATION_TIMEOUT`
//!
//! A future that never returns keeps its `Arc<Agent>`, and tokio collects a
//! child only when the `Child` is dropped. The pid sat `<defunct>` against
//! the service's `TasksMax` until the unit was restarted.
//!
//! The child writes its pid to a file here. Negotiation never completes in
//! these tests, and there is no session to read it back through.

use std::sync::Arc;
use std::time::Duration;

use mezame::config::{Config, TransportConfig};
use mezame::hub::HubRegistry;
use mezame::session::pid_is_alive;
use tempfile::TempDir;
use tokio::time::timeout;

const PATIENCE: Duration = Duration::from_secs(20);

fn sh(script: String) -> Arc<Config> {
    Arc::new(Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".to_string(),
        }],
        agent_cmd: "/bin/sh".to_string(),
        agent_args: vec!["-c".to_string(), script],
    })
}

/// Poll until `pid` is gone. Returns false when it outlives the bound.
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

fn read_pid(path: &std::path::Path) -> i32 {
    let raw = std::fs::read_to_string(path).expect("the child wrote its pid");
    raw.trim().parse().expect("pid parses")
}

#[tokio::test]
async fn attach_fails_when_the_agent_dies_during_negotiation() {
    // The child execs, our `initialize` lands, and it dies unanswered.
    // `attach_or_create` has to surface that as an error promptly. Note the
    // absence of `tokio::time::pause` here: this path must resolve on its
    // own, well inside `NEGOTIATION_TIMEOUT`.
    let tmp = TempDir::new().unwrap();
    let pidfile = tmp.path().join("agent.pid");
    let cfg = sh(format!(
        r#"echo $$ > "{}"; sleep 0.3; exit 101"#,
        pidfile.display()
    ));

    let registry = HubRegistry::new();
    let result = timeout(PATIENCE, registry.attach_or_create(cfg, None, None, "t1"))
        .await
        .expect("attach must resolve when the agent dies, and not hang");

    assert!(
        result.is_err(),
        "attach should fail when negotiation cannot complete"
    );

    let pid = read_pid(&pidfile);
    assert!(
        wait_until_reaped(pid).await,
        "pid {pid} is still present: the agent was never collected"
    );
}

#[tokio::test(start_paused = true)]
async fn attach_gives_up_on_an_agent_that_never_answers() {
    // The child execs and then sits there. Nothing closes stdout, the
    // pending table is never emptied, and only `NEGOTIATION_TIMEOUT` ends
    // the wait.
    //
    // `start_paused` lets the 60s bound be reached without spending it: the
    // runtime auto-advances its clock whenever every task is parked, and a
    // task blocked on the child's stdout counts as parked.
    //
    // No pid assertion here. Virtual time reaches the bound within
    // milliseconds of real time. The child is killed on the error path
    // before it has run a single command, and there is no pid on disk to
    // check. Collection on that path is the same `Agent` drop that
    // `tests/agent_dead_agent.rs` pins down.
    let cfg = sh("sleep 3600".to_string());

    let registry = HubRegistry::new();
    // The outer bound has to sit above `NEGOTIATION_TIMEOUT`. Auto-advance
    // jumps to the nearest deadline, and a shorter bound here would fire
    // first and mask the behaviour under test. At 120s it fires only when
    // the negotiation bound has gone missing. That regression then shows up
    // as a failure, and the suite does not hang.
    let result = timeout(
        Duration::from_secs(120),
        registry.attach_or_create(cfg, None, None, "t2"),
    )
    .await
    .expect("negotiation must be bounded");

    // `AttachedHub` is not `Debug`. This cannot go through `expect_err`.
    let err = match result {
        Ok(_) => panic!("a silent agent must not be attached to"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("did not finish negotiation"),
        "expected the negotiation timeout, got: {err}"
    );
}

#[tokio::test]
async fn attach_fails_when_the_agent_binary_is_missing() {
    // The spawn itself fails. No child exists to collect. Included to keep
    // the three negotiation outcomes side by side.
    let cfg = Arc::new(Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".to_string(),
        }],
        agent_cmd: "/nonexistent/mezame-not-an-agent".to_string(),
        agent_args: vec![],
    });

    let registry = HubRegistry::new();
    let result = timeout(PATIENCE, registry.attach_or_create(cfg, None, None, "t3"))
        .await
        .expect("must not hang");
    assert!(result.is_err(), "a missing binary must fail the attach");
}
