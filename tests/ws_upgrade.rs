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

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mezame::config::{Config, TransportConfig};
use mezame::http::{build_router, AppState};
use mezame::hub::{HubRegistry, MAX_PROMPT_TEXT_BYTES};
use mezame::ws::MAX_WS_MESSAGE_BYTES;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Notify};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A router serving on an ephemeral port, with the state it was built
/// from so a case can inspect the registry.
struct Server {
    addr: std::net::SocketAddr,
    state: Arc<AppState>,
}

async fn serve() -> Server {
    let (state_changes, _) = broadcast::channel(8);
    let state = Arc::new(AppState {
        config: Arc::new(Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
            }],
        }),
        hubs: HubRegistry::new(),
        state_changes,
        shutdown: Arc::new(Notify::new()),
    });

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("the bound address");
    let app = build_router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
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
    let ready = first_frame(&server, "/ws?session=%20abc-1%20").await;

    assert_eq!(ready["type"], "ready");
    assert_eq!(
        ready["sessionId"], "abc-1",
        "the surrounding whitespace is trimmed off"
    );
    assert!(server.state.hubs.is_registered_for_test("abc-1").await);
    assert!(
        !server.state.hubs.is_registered_for_test(" abc-1 ").await,
        "the untrimmed value is never a key"
    );
}

#[tokio::test]
async fn an_upgrade_with_a_refused_session_parameter_is_rejected_before_the_handshake() {
    // Requirement 6 criterion 8. The refusal happens ahead of the
    // handshake, so no WebSocket exists, `attach_or_create` never runs,
    // and no hub is created for the request.
    let server = serve().await;
    let url = format!("ws://{}/ws?session=../x", server.addr);
    let outcome = timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .expect("the attempt settles within 5s");

    match outcome {
        Ok(_) => panic!("the handshake should have been refused"),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(
                response.status(),
                400,
                "a value Mezame binds no hub to is a bad request"
            );
        }
        Err(other) => panic!("expected an HTTP 400, got {other:?}"),
    }

    assert!(
        !server.state.hubs.is_registered_for_test("../x").await,
        "no hub is created for a refused id"
    );
}

#[tokio::test]
async fn two_attaches_naming_one_id_share_a_hub_over_real_sockets() {
    // Requirement 7 criterion 2 over the transport: the second browser
    // sees the same `ready` the first did, bar the per-attach fields.
    let server = serve().await;
    let first = first_frame(&server, "/ws?session=shared-1").await;
    let second = first_frame(&server, "/ws?session=shared-1").await;

    assert_eq!(first["sessionId"], "shared-1");
    assert_eq!(second["sessionId"], "shared-1");
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
