//! The three upgrade arms, over a real socket.
//!
//! `ws_upgrade` cannot be driven through `tower::ServiceExt::oneshot`:
//! axum's `WebSocketUpgrade` extractor takes a `hyper::upgrade::OnUpgrade`
//! extension off the request and rejects with 426 when it is absent, and
//! only a real server connection inserts one. A synthetic request never
//! reaches the handler. So these cases bind a listener on an ephemeral
//! port, serve the real router on it, and connect as a browser would.
//!
//! This is also the one test that reaches `handle_ws`, and through it
//! `take_outbound`, `build_hub` and the shipped `EchoBackend`, end to end.
//! Every case here runs on the production serve path, `http::serve_with`,
//! so the accept loop, the connection task and the upgrade hand-off are
//! covered by the same handshakes; three cases at the end drive its
//! header-read timeout and its shutdown directly.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mezame::config::{Config, TransportConfig};
use mezame::http::{build_router, serve_with, AppState, HEADER_READ_TIMEOUT};
use mezame::hub::{HubRegistry, GRACE_PERIOD, MAX_PROMPT_TEXT_BYTES};
use mezame::ws::MAX_WS_MESSAGE_BYTES;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot, Notify};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HOST, ORIGIN};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Ids of the minted form, the only form an upgrade accepts.
const ACCEPTED_ID: &str = "fedcba9876543210fedcba9876543210";
const SHARED_ID: &str = "0123456789abcdef0123456789abcdef";

/// A router serving on an ephemeral port, with the state it was built
/// from so a case can inspect the registry.
struct Server {
    addr: std::net::SocketAddr,
    state: Arc<AppState>,
}

async fn serve() -> Server {
    serve_with_registry(HubRegistry::new()).await
}

/// `serve` around a caller-supplied registry, so a case can start from a
/// small capacity.
async fn serve_with_registry(hubs: HubRegistry) -> Server {
    serve_configured(hubs, HEADER_READ_TIMEOUT).await
}

/// `serve` with a caller-supplied header-read timeout, so a case can wait
/// it out in milliseconds.
async fn serve_with_header_timeout(header_read_timeout: Duration) -> Server {
    serve_configured(HubRegistry::new(), header_read_timeout).await
}

/// The state every server here is built from.
fn state_with_registry(hubs: HubRegistry) -> Arc<AppState> {
    let (state_changes, _) = broadcast::channel(8);
    Arc::new(AppState {
        config: Arc::new(Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
                hosts: vec![],
            }],
        }),
        hubs,
        state_changes,
        shutdown: Arc::new(Notify::new()),
    })
}

async fn serve_configured(hubs: HubRegistry, header_read_timeout: Duration) -> Server {
    let state = state_with_registry(hubs);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("the bound address");
    let app = build_router(state.clone());
    tokio::spawn(async move {
        let _ = serve_with(listener, app, header_read_timeout, std::future::pending()).await;
    });
    Server { addr, state }
}

/// Connect to `path` as a browser would.
async fn connect(server: &Server, path: &str) -> Socket {
    let url = format!("ws://{}{path}", server.addr);
    let (socket, _response) = timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .expect("the handshake completes within 5s")
        .expect("the handshake is accepted");
    socket
}

/// The next text frame on `socket` as JSON, within five seconds.
async fn next_text(socket: &mut Socket) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(message))) = timeout(Duration::from_secs(5), socket.next()).await else {
            break;
        };
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("a text frame is JSON");
        }
    }
    panic!("no text frame arrived");
}

/// Connect to `path` and return the first text frame as JSON.
async fn first_frame(server: &Server, path: &str) -> Value {
    let mut socket = connect(server, path).await;
    next_text(&mut socket).await
}

/// A `prompt` command holding one text block of `text`.
fn prompt_frame(text: &str) -> Message {
    Message::Text(
        json!({ "type": "prompt", "blocks": [{ "type": "text", "text": text }] }).to_string(),
    )
}

#[tokio::test]
async fn an_upgrade_with_no_session_parameter_mints_one() {
    // Requirement 6 criterion 1 and Requirement 10 criteria 1 to 8. The
    // minted id is 32 lowercase hex characters, the first frame is
    // `ready`, and a hub is registered under that id.
    let server = serve().await;
    let ready = first_frame(&server, "/ws").await;

    assert_eq!(ready["type"], "ready");
    let session_id = ready["sessionId"].as_str().expect("a sessionId string");
    assert_eq!(session_id.len(), 32, "16 bytes of entropy as hex");
    assert!(
        session_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "lowercase hex only, got {session_id:?}"
    );
    assert_eq!(ready["resumed"], true, "every attach is a join");
    assert_eq!(ready["busy"], false, "no turn is in flight on a fresh hub");
    assert_eq!(
        ready["promptCapabilities"],
        serde_json::json!({ "image": true, "audio": false, "embeddedContext": true })
    );
    assert!(
        ready["cwd"].as_str().is_some_and(|c| c.starts_with('/')),
        "the working directory is absolute, got {}",
        ready["cwd"]
    );
    assert!(
        ready["buildId"].as_str().is_some_and(|b| !b.is_empty()),
        "the handler stamps a non-empty buildId"
    );
    assert!(
        ready.get("resumeFailedFor").is_none(),
        "the field is gone from the event"
    );
    assert!(
        server.state.hubs.is_registered_for_test(session_id).await,
        "a hub is registered under the minted id"
    );
}

#[tokio::test]
async fn an_upgrade_with_an_accepted_session_parameter_uses_it_verbatim() {
    // Requirement 6 criterion 2: the trimmed value is the session id, and
    // nothing is minted.
    let server = serve().await;
    let ready = first_frame(&server, &format!("/ws?session=%20{ACCEPTED_ID}%20")).await;

    assert_eq!(ready["type"], "ready");
    assert_eq!(
        ready["sessionId"], ACCEPTED_ID,
        "the surrounding whitespace is trimmed off"
    );
    assert!(server.state.hubs.is_registered_for_test(ACCEPTED_ID).await);
    assert!(
        !server
            .state
            .hubs
            .is_registered_for_test(&format!(" {ACCEPTED_ID} "))
            .await,
        "the untrimmed value is never a key"
    );
}

#[tokio::test]
async fn an_upgrade_with_a_refused_session_parameter_is_rejected_before_the_handshake() {
    // Requirement 6 criteria 7 and 8. Only the minted form is accepted:
    // a path segment, a name a user typed, and the minted form in upper
    // case are all refused ahead of the handshake, so no WebSocket exists,
    // `attach_or_create` never runs, and no hub is created for the request.
    let server = serve().await;
    for refused in ["../x", "test", "FEDCBA9876543210FEDCBA9876543210"] {
        let url = format!("ws://{}/ws?session={refused}", server.addr);
        let outcome = timeout(Duration::from_secs(5), connect_async(&url))
            .await
            .expect("the attempt settles within 5s");

        match outcome {
            Ok(_) => panic!("the handshake for {refused:?} should have been refused"),
            Err(WsError::Http(response)) => {
                assert_eq!(
                    response.status(),
                    400,
                    "a value Mezame binds no hub to is a bad request: {refused:?}"
                );
            }
            Err(other) => panic!("expected an HTTP 400 for {refused:?}, got {other:?}"),
        }

        assert!(
            !server.state.hubs.is_registered_for_test(refused).await,
            "no hub is created for a refused id: {refused:?}"
        );
    }
}

#[tokio::test]
async fn two_attaches_naming_one_id_share_a_hub_over_real_sockets() {
    // Requirement 7 criterion 2 over the transport: the second browser
    // sees the same `ready` the first did, bar the per-attach fields.
    let server = serve().await;
    let first = first_frame(&server, &format!("/ws?session={SHARED_ID}")).await;
    let second = first_frame(&server, &format!("/ws?session={SHARED_ID}")).await;

    assert_eq!(first["sessionId"], SHARED_ID);
    assert_eq!(second["sessionId"], SHARED_ID);
    assert_eq!(first["cwd"], second["cwd"]);
    assert_eq!(first["buildId"], second["buildId"]);
    assert_eq!(
        first["promptCapabilities"], second["promptCapabilities"],
        "one value per process"
    );
    assert_eq!(second["busy"], false);
}

#[tokio::test]
async fn a_message_past_the_ceiling_ends_the_connection_and_spares_the_session() {
    // The library default of 64 MiB is gone. A frame announcing more than
    // MAX_WS_MESSAGE_BYTES is refused before its payload is read, the
    // connection ends with nothing broadcast, and a reconnect finds the
    // session as it was.
    let server = serve().await;
    let mut socket = connect(&server, "/ws").await;
    let ready = next_text(&mut socket).await;
    let session_id = ready["sessionId"]
        .as_str()
        .expect("a sessionId")
        .to_string();

    // The JSON around the text pushes the message over the ceiling.
    let oversize = prompt_frame(&"a".repeat(MAX_WS_MESSAGE_BYTES));
    // The send may fail part-way: the server closes as soon as it has
    // read the frame header, which is the point.
    let _ = timeout(Duration::from_secs(10), socket.send(oversize)).await;

    let ended = timeout(Duration::from_secs(10), async {
        loop {
            match socket.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break true,
                Some(Ok(Message::Text(text))) => {
                    eprintln!("unexpected frame after an oversize message: {text}");
                    break false;
                }
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .expect("the connection settles within 10s");
    assert!(
        ended,
        "the server ends the connection and sends nothing else on it"
    );

    // The session is untouched: still registered, and a reconnect joins it.
    assert!(server.state.hubs.is_registered_for_test(&session_id).await);
    let again = first_frame(&server, &format!("/ws?session={session_id}")).await;
    assert_eq!(again["type"], "ready");
    assert_eq!(again["sessionId"], session_id);
    assert_eq!(
        again["busy"], false,
        "no turn was started by the refused frame"
    );
}

#[tokio::test]
async fn a_prompt_past_the_text_ceiling_is_answered_with_an_error_over_a_real_socket() {
    // Inside the message ceiling but past the prompt text ceiling: the
    // hub answers the sender with one `error`, the attach loop forwards it
    // because it is stamped for this attach, no echo follows, and the
    // session takes the next prompt.
    let server = serve().await;
    let mut socket = connect(&server, "/ws").await;
    let ready = next_text(&mut socket).await;
    assert_eq!(ready["type"], "ready");

    socket
        .send(prompt_frame(&"t".repeat(MAX_PROMPT_TEXT_BYTES + 1)))
        .await
        .expect("a message inside the ceiling is accepted");
    let error = next_text(&mut socket).await;
    assert_eq!(error["type"], "error");
    assert!(error["message"]
        .as_str()
        .is_some_and(|m| m.contains(&MAX_PROMPT_TEXT_BYTES.to_string())));
    assert!(
        error["_target"].is_u64(),
        "delivered because it is stamped for this attach"
    );

    socket
        .send(prompt_frame("hello"))
        .await
        .expect("the next prompt is sent");
    let echo = next_text(&mut socket).await;
    assert_eq!(echo["type"], "append");
    assert_eq!(echo["role"], "user");
    assert_eq!(echo["text"], "> hello\n");
    let reply = next_text(&mut socket).await;
    assert_eq!(reply["role"], "agent");
    assert_eq!(reply["text"], "hello");
    let done = next_text(&mut socket).await;
    assert_eq!(done["type"], "prompt_done");
}

/// A handshake attempt with `header` set to `value`, as a browser's own
/// page or a hostile one would send it: the socket, or the HTTP status
/// the server refused the handshake with.
async fn connect_with_header(
    server: &Server,
    path: &str,
    header: tokio_tungstenite::tungstenite::http::HeaderName,
    value: &str,
) -> Result<Socket, u16> {
    let url = format!("ws://{}{path}", server.addr);
    let mut request = url.into_client_request().expect("a client request");
    request
        .headers_mut()
        .insert(header, value.parse().expect("a header value"));
    match timeout(Duration::from_secs(5), connect_async(request))
        .await
        .expect("the attempt settles within 5s")
    {
        Ok((socket, _response)) => Ok(socket),
        Err(WsError::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("expected a socket or an HTTP refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn an_upgrade_from_another_origin_is_forbidden_before_the_handshake() {
    // Browsers apply no same-origin policy to a WebSocket handshake: a page
    // at evil.example can open `ws://127.0.0.1:9510/ws` and, before this
    // check, read and drive the session it named. The handshake carries
    // that page's `Origin`; the upgrade is refused with a 403 ahead of the
    // handshake, and no hub is created for the id it asked for.
    let server = serve().await;
    let id = "00112233445566778899aabbccddeeff";
    let status = connect_with_header(
        &server,
        &format!("/ws?session={id}"),
        ORIGIN,
        "http://evil.example",
    )
    .await
    .expect_err("the handshake should have been refused");
    assert_eq!(status, 403, "another page's origin is forbidden");
    assert!(
        !server.state.hubs.is_registered_for_test(id).await,
        "no hub is created for a refused upgrade"
    );
}

#[tokio::test]
async fn an_upgrade_from_the_page_this_server_served_is_accepted() {
    // The shipped UI builds its socket URL from `location.host`, so its
    // `Origin` names the same host and port the handshake is sent to.
    let server = serve().await;
    let origin = format!("http://{}", server.addr);
    let mut socket = connect_with_header(&server, "/ws", ORIGIN, &origin)
        .await
        .expect("the page this server served is accepted");
    let ready = next_text(&mut socket).await;
    assert_eq!(ready["type"], "ready");
}

#[tokio::test]
async fn an_upgrade_for_a_hostname_this_server_does_not_serve_is_misdirected() {
    // DNS rebinding: a page at attacker.example, re-pointed at 127.0.0.1,
    // sends its own name in `Host`, and its `Origin` matches it. The
    // `Host` check answers 421 before `Origin` is even consulted.
    let server = serve().await;
    let status = connect_with_header(&server, "/ws", HOST, "attacker.example:9510")
        .await
        .expect_err("the handshake should have been refused");
    assert_eq!(status, 421, "a name this server does not serve");
}

#[tokio::test]
async fn an_upgrade_for_a_new_session_is_answered_503_before_the_handshake_when_the_registry_is_full(
) {
    // Requirement 6 criterion 10. With every slot held, a new session is
    // refused ahead of the handshake with a 503 and a `Retry-After` of one
    // grace period, so the browser backs off instead of seeing a socket
    // that closes at once. A live session is joined as before.
    let server = serve_with_registry(HubRegistry::with_capacity_for_test(1)).await;
    let mut first = connect(&server, "/ws").await;
    let ready = next_text(&mut first).await;
    let first_id = ready["sessionId"]
        .as_str()
        .expect("a sessionId string")
        .to_string();

    let url = format!("ws://{}/ws", server.addr);
    match timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .expect("the attempt settles within 5s")
    {
        Ok(_) => panic!("a second session should have been refused"),
        Err(WsError::Http(response)) => {
            assert_eq!(response.status(), 503, "the registry is full");
            assert_eq!(
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
                Some(GRACE_PERIOD.as_secs().to_string().as_str()),
                "the browser is told when a slot may free"
            );
        }
        Err(other) => panic!("expected an HTTP 503, got {other:?}"),
    }

    let again = first_frame(&server, &format!("/ws?session={first_id}")).await;
    assert_eq!(
        again["sessionId"], first_id,
        "a live session is always joinable"
    );
    assert!(server.state.hubs.is_registered_for_test(&first_id).await);
    drop(first);
}

/// Read from a raw connection until the server closes it, or until
/// `patience` passes. `Some(bytes)` is what arrived before the close;
/// `None` means the server was still holding the connection open.
async fn read_until_closed(stream: &mut TcpStream, patience: Duration) -> Option<Vec<u8>> {
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return Some(all),
            Ok(Ok(n)) => all.extend_from_slice(&buf[..n]),
            Ok(Err(_)) => return Some(all),
            Err(_) => return None,
        }
    }
}

#[tokio::test]
async fn a_connection_that_sends_no_request_is_closed_after_the_header_read_timeout() {
    // A peer that opens a connection and never sends a request head used
    // to hold it, and its descriptor, forever: axum's serve set no timer
    // on hyper, which drops the timeout it has no timer for. With the
    // timer set, the connection is closed once the timeout passes.
    let server = serve_with_header_timeout(Duration::from_millis(200)).await;
    let mut stream = TcpStream::connect(server.addr)
        .await
        .expect("connect to the server");
    let started = tokio::time::Instant::now();
    let closed = read_until_closed(&mut stream, Duration::from_secs(3)).await;
    assert!(
        closed.is_some(),
        "the server must close a connection that sends no request head"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "the close waits for the timeout, not less"
    );
    assert!(
        closed.is_some_and(|bytes| bytes.is_empty()),
        "nothing is written to a peer that sent nothing"
    );
}

#[tokio::test]
async fn an_idle_keep_alive_connection_is_closed_after_the_header_read_timeout() {
    // hyper re-arms the header timer for every request head on a kept-alive
    // connection, so the same timeout is the idle limit between requests.
    // `/history` with an unknown id answers from memory and touches no
    // file, so the case reads nothing under `HOME`.
    let server = serve_with_header_timeout(Duration::from_millis(200)).await;
    let mut stream = TcpStream::connect(server.addr)
        .await
        .expect("connect to the server");
    stream
        .write_all(b"GET /history?session=x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .expect("send one request");

    let started = tokio::time::Instant::now();
    let bytes = read_until_closed(&mut stream, Duration::from_secs(3))
        .await
        .expect("the server must close an idle keep-alive connection");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "the request was answered before the idle close, got {text:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "the idle close waits for the timeout"
    );
}

#[tokio::test]
async fn shutdown_closes_the_listener_and_serve_returns() {
    // The shutdown future resolves: the accept loop stops, the listener is
    // dropped so a new connection is refused, open connections are asked
    // to finish, and `serve` returns. The signal is a oneshot, which
    // stores its value: a `Notify::notify_waiters` sent before the serve
    // task first polls its shutdown future would be lost.
    let state = state_with_registry(HubRegistry::new());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("the bound address");
    let app = build_router(state.clone());
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        serve_with(listener, app, HEADER_READ_TIMEOUT, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    // One live connection first, so the shutdown has something to drain.
    let server = Server { addr, state };
    let mut socket = connect(&server, "/ws").await;
    let ready = next_text(&mut socket).await;
    assert_eq!(ready["type"], "ready");

    shutdown_tx.send(()).expect("the serve task is waiting");
    let outcome = timeout(Duration::from_secs(2), serving)
        .await
        .expect("serve returns within 2s of the shutdown")
        .expect("the serve task did not panic");
    assert!(outcome.is_ok(), "serve returns Ok on a clean shutdown");
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "the listener is closed: a new connection is refused"
    );
    drop(socket);
}
