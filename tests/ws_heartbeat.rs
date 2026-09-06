//! Regression tests for the WebSocket heartbeat in
//! `mezame::ws::run_attach_loop`. Covers the half-open-socket leak
//! reported in GitHub issue #4: a peer that vanishes without a TCP
//! FIN (laptop sleep, Wi-Fi drop, reverse proxy dropping an idle
//! upstream) leaves a socket that yields nothing on `stream.next()`,
//! so without a heartbeat the attach loop blocks forever and the
//! session is never reclaimed.
//!
//! The loop is driven directly with a fake stream so we never need a
//! real socket. Short heartbeat durations keep the tests fast.

mod support;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message};
use futures_util::Stream;
use mezame::hub::{AttachedHub, HubRegistry};
use mezame::ws::run_attach_loop;
use serde_json::{json, Value};
use support::{Release, ScriptedBackend, ScriptedTurn};
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
/// is gone and the TCP connection is still `ESTABLISHED`. The WS stream
/// produces no data, no error and no close.
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

/// Register a hub around a scripted Backend and return the attach.
///
/// The caller holds the `AttachedHub` for the length of the test: dropping
/// it decrements the subscriber count and arms the grace timer, and the
/// hub's broadcast sender is what `run_attach_loop` reads from.
async fn attach(backend: Arc<ScriptedBackend>) -> (HubRegistry, AttachedHub) {
    let registry = HubRegistry::new();
    let attached = registry
        .register_for_test(backend, SESSION_ID.into(), ready_event(), None)
        .await;
    (registry, attached)
}

#[tokio::test]
async fn silent_socket_is_evicted_after_the_heartbeat_timeout() {
    let (_registry, mut attached) = attach(Arc::new(ScriptedBackend::new())).await;
    let outbound = attached.take_outbound();
    let commands = attached.commands.clone();
    let attach_id = attached.attach_id;
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel::<Message>();
    let mut stream = silent_stream();

    // 50 ms ping interval, 150 ms silence budget. A live peer would be
    // pinged about 3 times. The silent stream answers none of them and
    // the loop must break shortly after 150 ms.
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
    let (_registry, mut attached) = attach(Arc::new(ScriptedBackend::new())).await;
    let outbound = attached.take_outbound();
    let commands = attached.commands.clone();
    let attach_id = attached.attach_id;
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
    let (_registry, mut attached) = attach(Arc::new(ScriptedBackend::new())).await;
    let outbound = attached.take_outbound();
    let commands = attached.commands.clone();
    let attach_id = attached.attach_id;
    let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel::<Message>();
    let (browser_tx, browser_rx) = mpsc::unbounded_channel::<Message>();
    let mut stream = channel_stream(browser_rx);

    // Feed a pong every 30 ms for about 250 ms, then close. The timeout
    // is 120 ms, and a silent peer would have been evicted twice over in
    // that span.
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

#[tokio::test]
async fn a_frame_targeted_at_this_attach_is_forwarded_with_its_target_field() {
    // Requirement 8 criterion 16. Criteria 5 and 6 state when a frame is
    // dropped and when it is forwarded untargeted, and a loop that dropped
    // every targeted frame would satisfy both. The `_target` field stays
    // on the frame, as it does today; the client ignores it.
    //
    // The whole path runs here: a prompt arrives as a text frame, the hub
    // stamps the card the turn streams, and the loop forwards it to this
    // attach's sink.
    let card = json!({
        "type": "permission_request",
        "id": "perm-7",
        "title": "Allow the write?",
        "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" }]
    });
    let backend = Arc::new(ScriptedBackend::with_turn(ScriptedTurn::pending(vec![
        card,
    ])));
    let (_registry, mut attached) = attach(backend.clone()).await;
    let outbound = attached.take_outbound();
    let commands = attached.commands.clone();
    let attach_id = attached.attach_id;

    let (to_ws_tx, mut to_ws_rx) = mpsc::unbounded_channel::<Message>();
    let (browser_tx, browser_rx) = mpsc::unbounded_channel::<Message>();
    let mut stream = channel_stream(browser_rx);
    let handle = tokio::spawn(async move {
        run_attach_loop(
            &mut stream,
            &to_ws_tx,
            outbound,
            commands,
            attach_id,
            Duration::from_secs(5),
            Duration::from_secs(30),
        )
        .await;
    });

    browser_tx
        .send(Message::Text(
            json!({ "type": "prompt", "blocks": [{ "type": "text", "text": "write it" }] })
                .to_string(),
        ))
        .expect("send the prompt frame");

    // Read the sink until the card arrives, skipping the user echo.
    let mut targeted: Option<Value> = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let Ok(Some(frame)) = timeout(Duration::from_millis(200), to_ws_rx.recv()).await else {
            continue;
        };
        let Message::Text(text) = frame else { continue };
        let value: Value = serde_json::from_str(&text).expect("the sink carries JSON");
        if value["type"] == "permission_request" {
            targeted = Some(value);
            break;
        }
    }

    let targeted = targeted.expect("the targeted card reaches this attach's sink");
    assert_eq!(
        targeted["_target"].as_u64(),
        Some(attach_id),
        "the frame keeps its `_target` field on the way to the sink"
    );
    assert_eq!(targeted["id"], "perm-7");
    assert_eq!(targeted["title"], "Allow the write?");

    backend.release_turn(Release::Ok);
    drop(browser_tx);
    let _ = timeout(Duration::from_secs(2), handle).await;
}
