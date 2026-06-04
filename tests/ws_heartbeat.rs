//! Regression tests for the WebSocket heartbeat in
//! `mezame::ws::run_attach_loop`. Covers the half-open-socket leak
//! reported in GitHub issue #4: a peer that vanishes without a TCP
//! FIN (laptop sleep, Wi-Fi drop, reverse proxy dropping an idle
//! upstream) leaves a socket that yields nothing on `stream.next()`,
//! so without a heartbeat the attach loop blocks forever and the
//! agent subprocess tree is never reaped.
//!
//! The loop is driven directly with a fake stream so we never need a
//! real socket. Short heartbeat durations keep the tests fast.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message};
use futures_util::Stream;
use mezame::agent::from_io;
use mezame::hub::HubRegistry;
use mezame::ws::run_attach_loop;
use serde_json::{json, Value};
use tokio::io::duplex;
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

const SESSION_ID: &str = "hb-session";

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

/// A stream that never yields, modelling a half-open socket: the peer
/// is gone but the TCP connection is still `ESTABLISHED`, so the WS
/// stream produces neither data, error, nor close.
fn silent_stream() -> impl Stream<Item = Result<Message, Infallible>> + Unpin {
    Box::pin(futures_util::stream::pending())
}

/// Wrap an mpsc receiver as a `Stream` of `Result<Message, _>`.
fn channel_stream(
    rx: mpsc::UnboundedReceiver<Message>,
) -> impl Stream<Item = Result<Message, Infallible>> + Unpin {
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|m| (Ok(m), rx))
    }))
}

/// Register a hub and return the pieces `run_attach_loop` needs.
///
/// The duplex pipe ends are returned and must be held alive by the
/// caller: dropping the agent's stdout pipe signals EOF, which closes
/// the hub's updates channel, shuts the hub down, and drops the
/// broadcast sender. That would make `run_attach_loop` exit instantly
/// via the `Closed` arm and defeat the heartbeat test.
async fn attach() -> (
    HubRegistry,
    mpsc::Sender<mezame::hub::HubCommand>,
    tokio::sync::broadcast::Receiver<Arc<Value>>,
    u64,
    (tokio::io::DuplexStream, tokio::io::DuplexStream),
) {
    let registry = HubRegistry::new();
    let (server_to_agent, agent_stdin) = duplex(8 * 1024);
    let (agent_stdout, server_reader) = duplex(8 * 1024);
    let (agent, updates_rx) = from_io(server_to_agent, server_reader);
    let attached = registry
        .register_for_test(
            Arc::new(agent),
            SESSION_ID.into(),
            updates_rx,
            ready_event(),
            None,
        )
        .await;
    (
        registry,
        attached.commands.clone(),
        attached.outbound.resubscribe(),
        attached.attach_id,
        // Keep both pipe ends alive for the duration of the test so
        // the hub's updates channel stays open and the broadcast
        // sender is not dropped.
        (agent_stdin, agent_stdout),
    )
}

#[tokio::test]
async fn silent_socket_is_evicted_after_the_heartbeat_timeout() {
    let (_registry, commands, outbound, attach_id, _pipes) = attach().await;
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel::<Message>();
    let mut stream = silent_stream();

    // 50 ms ping interval, 150 ms silence budget. A live peer would
    // be pinged ~3 times; our silent stream answers none, so the loop
    // must break shortly after 150 ms.
    let started = Instant::now();
    let done = timeout(
        Duration::from_secs(2),
        run_attach_loop(
            &mut stream,
            &to_ws_tx,
            outbound,
            commands,
            attach_id,
            Duration::from_millis(50),
            Duration::from_millis(150),
        ),
    )
    .await;

    assert!(
        done.is_ok(),
        "attach loop did not exit for a half-open socket; the leak is back"
    );
    // It should not exit early (before the timeout) and not hang.
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "loop exited before the silence budget elapsed"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "loop took too long to evict a silent peer"
    );
}

#[tokio::test]
async fn pings_are_sent_to_the_peer_on_the_interval() {
    let (_registry, commands, outbound, attach_id, _pipes) = attach().await;
    let (to_ws_tx, mut to_ws_rx) = mpsc::unbounded_channel::<Message>();
    let mut stream = silent_stream();

    let handle = tokio::spawn(async move {
        run_attach_loop(
            &mut stream,
            &to_ws_tx,
            outbound,
            commands,
            attach_id,
            Duration::from_millis(40),
            Duration::from_millis(500),
        )
        .await;
    });

    // At least one Ping frame should reach the sink before the
    // timeout evicts the (silent) peer.
    let frame = timeout(Duration::from_secs(1), to_ws_rx.recv())
        .await
        .expect("a frame within 1s")
        .expect("channel open");
    assert!(
        matches!(frame, Message::Ping(_)),
        "expected a Ping frame, got {frame:?}"
    );

    let _ = timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn an_active_peer_is_not_evicted() {
    // A peer that keeps sending frames (here, periodic pongs) resets
    // the liveness clock and is never evicted. We drive it for well
    // past the timeout, then close it cleanly and confirm the loop
    // only exited on the close, not on a heartbeat eviction.
    let (_registry, commands, outbound, attach_id, _pipes) = attach().await;
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel::<Message>();
    let (browser_tx, browser_rx) = mpsc::unbounded_channel::<Message>();
    let mut stream = channel_stream(browser_rx);

    // Feed a pong every 30 ms for ~250 ms (timeout is 120 ms, so a
    // silent peer would have been evicted twice over), then close.
    let feeder = tokio::spawn(async move {
        for _ in 0..8 {
            if browser_tx.send(Message::Pong(Vec::new())).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let _ = browser_tx.send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: "bye".into(),
        })));
    });

    let started = Instant::now();
    let done = timeout(
        Duration::from_secs(2),
        run_attach_loop(
            &mut stream,
            &to_ws_tx,
            outbound,
            commands,
            attach_id,
            Duration::from_millis(40),
            Duration::from_millis(120),
        ),
    )
    .await;

    assert!(done.is_ok(), "loop did not exit on the eventual close");
    // The loop must have survived past the timeout (the active peer
    // kept it alive), exiting only when the close frame arrived
    // around 240 ms.
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "active peer was evicted early; heartbeat is too aggressive"
    );
    let _ = timeout(Duration::from_secs(1), feeder).await;
}
