//! Integration tests for `GET /state/events`, the Server-Sent Events
//! stream that tells every attached browser to refetch `/state`.
//!
//! Driven through `mezame::http::build_router` with
//! `tower::ServiceExt::oneshot`, the same harness as
//! `tests/http_routes.rs`. The response body here is an open stream, and
//! `axum::body::to_bytes` would wait on it forever. Each test reads frames
//! off `into_data_stream()` under a timeout.
//!
//! No `HOME` mutation anywhere in this file: the handler only touches the
//! broadcast channel and the shutdown notify.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, BodyDataStream};
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use mezame::config::{Config, TransportConfig};
use mezame::http::{build_router, AppState};
use mezame::hub::HubRegistry;
use tokio::sync::{broadcast, Notify};
use tokio::time::timeout;
use tower::ServiceExt;

/// `capacity` is the broadcast buffer. A small one makes a lagging
/// receiver reachable without pushing thousands of messages.
fn test_state(capacity: usize) -> Arc<AppState> {
    let (state_changes, _) = broadcast::channel(capacity);
    Arc::new(AppState {
        config: Arc::new(Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
                hosts: vec![],
            }],
        }),
        hubs: HubRegistry::new(),
        state_changes,
        shutdown: Arc::new(Notify::new()),
    })
}

/// Open the stream and hand back its body.
///
/// The handler subscribes to the broadcast in its own body, before axum
/// has a response to return, and this future resolves after that. A tick
/// fired once it returns is therefore queued for the stream, never lost.
///
/// `oneshot` consumes the router, which drops the only other
/// `Arc<AppState>` once this returns. That is load-bearing for
/// `stream_ends_when_the_last_sender_is_dropped` below.
async fn open_stream(state: Arc<AppState>) -> BodyDataStream {
    let app = build_router(state);
    let req = Request::get("/state/events").body(Body::empty()).unwrap();
    let res = app.oneshot(req).await.expect("router responded");
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get("content-type")
        .expect("SSE response sets content-type")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "content-type was `{ct}`"
    );
    res.into_body().into_data_stream()
}

/// Read one frame, failing the test on timeout or on a closed stream.
async fn next_frame(stream: &mut BodyDataStream) -> String {
    let chunk = timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a frame within 5s")
        .expect("stream still open")
        .expect("frame read cleanly");
    String::from_utf8(chunk.to_vec()).expect("frame is utf8")
}

#[tokio::test]
async fn emits_state_changed_when_a_tick_fires() {
    let state = test_state(8);
    let mut stream = open_stream(state.clone()).await;

    // `send` reporting a receiver count above zero is itself the proof
    // that the handler subscribed before returning its response.
    let receivers = state
        .state_changes
        .send(())
        .expect("a receiver is attached");
    assert_eq!(receivers, 1);

    let frame = next_frame(&mut stream).await;
    assert!(
        frame.contains("event: state_changed"),
        "frame was `{frame}`"
    );
}

#[tokio::test]
async fn emits_one_event_per_tick() {
    let state = test_state(8);
    let mut stream = open_stream(state.clone()).await;

    state.state_changes.send(()).expect("receiver attached");
    let first = next_frame(&mut stream).await;
    assert!(first.contains("state_changed"), "first was `{first}`");

    state.state_changes.send(()).expect("receiver attached");
    let second = next_frame(&mut stream).await;
    assert!(second.contains("state_changed"), "second was `{second}`");
}

#[tokio::test]
async fn a_lagged_receiver_skips_ahead_and_keeps_streaming() {
    // Capacity 2 with 8 ticks queued before the first poll drops the
    // receiver behind, and `recv` reports `Lagged`. The handler swallows it
    // and delivers the next retained tick. A browser that fell behind
    // still refetches, which is all the event means.
    let state = test_state(2);
    let mut stream = open_stream(state.clone()).await;

    for _ in 0..8 {
        state.state_changes.send(()).expect("receiver attached");
    }

    let frame = next_frame(&mut stream).await;
    assert!(
        frame.contains("event: state_changed"),
        "frame was `{frame}`"
    );
}

#[tokio::test]
async fn stream_ends_when_shutdown_fires() {
    // The shutdown arm exists to stop axum's graceful drain waiting on a
    // request future that never resolves. Ctrl+C would hang without it.
    let state = test_state(8);
    let shutdown = state.shutdown.clone();
    let mut stream = open_stream(state.clone()).await;

    // `notify_waiters` only wakes a task already parked on `notified()`.
    // Fire it from a second task once the stream has been polled and is
    // waiting.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        shutdown.notify_waiters();
    });

    let ended = timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream should end within 5s");
    assert!(
        ended.is_none(),
        "shutdown must end the SSE stream, got {ended:?}"
    );
}

#[tokio::test]
async fn stream_ends_when_the_last_sender_is_dropped() {
    // `open_stream` leaves the test holding the only remaining
    // `Arc<AppState>`. Dropping it drops the broadcast sender, `recv`
    // reports `Closed`, and the stream ends. A leak here would hold the
    // connection open against a server that is already gone.
    let state = test_state(8);
    let mut stream = open_stream(state.clone()).await;
    drop(state);

    let ended = timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream should end within 5s");
    assert!(
        ended.is_none(),
        "a closed channel must end the SSE stream, got {ended:?}"
    );
}
