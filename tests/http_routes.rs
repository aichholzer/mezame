//! HTTP integration tests for the cloudflared transport.
//!
//! Drives `mezame::http::build_router` via `tower::ServiceExt::oneshot`
//! so we hit the real axum routing, the real handlers, and the embedded
//! UI bundle without binding a TCP port.
//!
//! Nothing here reads a file or the environment: the state lives in the
//! in-memory store `AppState::for_test` opens, so every case runs on its
//! own state with no lock.

mod support;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mezame::backend::{
    EntryBody, HistoryEntry, ToolCall, ToolCallStatus, ToolContent, ToolLocation,
};
use mezame::config::{Config, TransportConfig};
use mezame::http::{build_router, AppState};
use mezame::hub::HubRegistry;
use serde_json::{json, Value};
use std::sync::Arc;
use support::ScriptedBackend;
use tower::ServiceExt;

fn dummy_state() -> Arc<AppState> {
    state_with_hosts(&[])
}

/// A state whose transport lists `hosts`, the public names a tunnel or
/// proxy in front of Mezame carries.
fn state_with_hosts(hosts: &[&str]) -> Arc<AppState> {
    AppState::for_test(
        Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
                hosts: hosts.iter().map(|h| h.to_string()).collect(),
            }],
            version: 2,
            datastore: Default::default(),
            public_url: None,
            models: vec![],
        },
        HubRegistry::new(),
        8,
    )
}

/// What a logged-in browser's own page adds to a request: the cookie, and
/// `Sec-Fetch-Site: same-origin` when the request says nothing else about
/// where it came from (the guard refuses a write or an upgrade with neither
/// `Origin` nor `Sec-Fetch-Site`).
async fn as_browser(state: &AppState, mut req: Request<Body>) -> Request<Body> {
    let cookie = state.login_for_test("alice", "correct horse battery").await;
    req.headers_mut()
        .insert(axum::http::header::COOKIE, cookie.parse().unwrap());
    if !req.headers().contains_key("origin") && !req.headers().contains_key("sec-fetch-site") {
        req.headers_mut()
            .insert("sec-fetch-site", "same-origin".parse().unwrap());
    }
    req
}

/// Send a single request through the router as a logged-in browser and
/// return (status, body, headers).
async fn run_request(req: Request<Body>) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let state = dummy_state();
    let req = as_browser(&state, req).await;
    let app = build_router(state);
    let res = app.oneshot(req).await.expect("router did not respond");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = to_bytes(res.into_body(), 1024 * 1024)
        .await
        .expect("body read")
        .to_vec();
    (status, bytes, headers)
}

fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("response was not JSON")
}

// ---------- /history ----------

#[tokio::test]
async fn get_history_without_session_param_is_400() {
    let req = Request::get("/history").body(Body::empty()).unwrap();
    let (status, _, _) = run_request(req).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_history_with_an_empty_session_param_is_400() {
    // The other branch of Requirement 13 criterion 2, which had no case.
    // Both branches answer 400 with a plain-text body naming what is
    // missing.
    let req = Request::get("/history?session=")
        .body(Body::empty())
        .unwrap();
    let (status, bytes, headers) = run_request(req).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body = String::from_utf8(bytes).expect("a UTF-8 body");
    assert!(
        body.contains("session"),
        "the body names the missing parameter, got {body:?}"
    );
    let ct = headers
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        ct.starts_with("text/plain"),
        "the body is plain text, got {ct:?}"
    );
}

#[tokio::test]
async fn get_history_for_a_registered_hub_returns_its_transcript() {
    // Requirement 13 criterion 3, with criterion 5's `HOME` clause: the
    // answer is resolved from the registry alone, so it is the same answer
    // with no `HOME` set at all and no file read.
    //
    // Self-contained on purpose: its own state, its own registry, and the
    // attach held across the request so the hub cannot be torn down under
    // it. `run_request` is untouched.
    let transcript = vec![
        HistoryEntry {
            body: EntryBody::User {
                text: "ping".to_string(),
            },
            timestamp: 1_000,
            usage: None,
        },
        HistoryEntry {
            body: EntryBody::Agent {
                text: "ping".to_string(),
            },
            timestamp: 1_000,
            usage: None,
        },
        HistoryEntry {
            body: EntryBody::Thought {
                text: "thinking".to_string(),
            },
            timestamp: 1_100,
            usage: None,
        },
        HistoryEntry {
            body: EntryBody::Sys {
                text: "a notice".to_string(),
            },
            timestamp: 1_200,
            usage: None,
        },
        HistoryEntry {
            body: EntryBody::ToolCall(ToolCall {
                tool_call_id: "t-1".to_string(),
                title: "Read".to_string(),
                status: ToolCallStatus::Completed,
                kind: None,
                raw_input: json!({ "path": "/x" }),
                content: Some(vec![ToolContent::Text {
                    text: "ok".to_string(),
                }]),
                locations: Some(vec![ToolLocation {
                    path: "/x".to_string(),
                    line: Some(3),
                }]),
            }),
            timestamp: 1_300,
            usage: None,
        },
    ];

    let hubs = HubRegistry::new();
    let backend = Arc::new(ScriptedBackend::new().transcript(transcript));
    let _attached = hubs
        .register_for_test(
            backend,
            "hist-session".to_string(),
            json!({ "type": "ready", "sessionId": "hist-session" }),
            None,
        )
        .await;

    let state = AppState::for_test(
        Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
                hosts: vec![],
            }],
            version: 2,
            datastore: Default::default(),
            public_url: None,
            models: vec![],
        },
        hubs,
        8,
    );
    let cookie = state.login_for_test("alice", "correct horse battery").await;
    // The route answers for a row the user owns; the hub alone is not one.
    let alice = state.store.user_by_name("alice").await.unwrap().unwrap();
    state
        .store
        .create_session(&alice.id, "hist-session", None, 1_000)
        .await
        .unwrap();

    let app = build_router(state);
    let res = app
        .oneshot(
            Request::get("/history?session=hist-session")
                .header(axum::http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router did not respond");
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = to_bytes(res.into_body(), 1024 * 1024)
        .await
        .expect("body read")
        .to_vec();
    let entries = json_body(&bytes)["entries"].clone();
    let entries = entries.as_array().expect("an entries array");

    assert_eq!(entries.len(), 5, "every recorded entry, in order");
    assert_eq!(
        entries[0],
        json!({ "role": "user", "text": "ping", "timestamp": 1000 })
    );
    assert_eq!(
        entries[2],
        json!({ "role": "thought", "text": "thinking", "timestamp": 1100 })
    );
    assert_eq!(
        entries[4],
        json!({
            "role": "tool_call",
            "toolCallId": "t-1",
            "title": "Read",
            "status": "completed",
            "kind": null,
            "rawInput": { "path": "/x" },
            "content": [{ "type": "text", "text": "ok" }],
            "locations": [{ "path": "/x", "line": 3 }],
            "timestamp": 1300
        }),
        "a tool-call entry serialises to the closed shape"
    );
}

#[tokio::test]
async fn get_history_for_an_unknown_session_is_404_with_empty_entries() {
    // Phase 2 Requirement 6 criterion 4: an id with no row is answered the
    // way another user's row is, 404 with an empty array. A value holding
    // `/`, `\` or `..` is such an id; nothing is created.
    for id in ["nobody-here", "../etc/passwd", "a/b", "a%5Cb"] {
        let req = Request::get(format!("/history?session={id}"))
            .body(Body::empty())
            .unwrap();
        let (status, bytes, _) = run_request(req).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "id {id:?} answers 404");
        assert_eq!(json_body(&bytes), json!({ "entries": [] }), "id {id:?}");
    }
}

#[tokio::test]
async fn get_history_for_a_session_owned_by_someone_else_is_the_same_404() {
    let state = dummy_state();
    let bob = state.login_for_test("bob", "correct horse battery").await;
    let bob_row = state.store.user_by_name("bob").await.unwrap().unwrap();
    state
        .store
        .create_session(&bob_row.id, "bobs-session", None, 1_000)
        .await
        .unwrap();
    // Bob sees his (empty) history; Alice sees the 404 an unknown id gets.
    let res = build_router(state.clone())
        .oneshot(
            Request::get("/history?session=bobs-session")
                .header(axum::http::header::COOKIE, bob)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let req = as_browser(
        &state,
        Request::get("/history?session=bobs-session")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let res = build_router(state).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(res.into_body(), 4096).await.unwrap();
    assert_eq!(json_body(&bytes), json!({ "entries": [] }));
}

// ---------- SPA fallback / asset routing ----------

#[tokio::test]
async fn get_root_serves_index_html_with_no_cache() {
    let req = Request::get("/").body(Body::empty()).unwrap();
    let (status, bytes, headers) = run_request(req).await;

    assert_eq!(status, StatusCode::OK);
    let ct = headers.get("content-type").unwrap().to_str().unwrap();
    assert!(
        ct.starts_with("text/html"),
        "content-type was `{ct}`, expected text/html"
    );
    let cc = headers.get("cache-control").unwrap().to_str().unwrap();
    assert!(cc.contains("no-cache"), "cache-control was `{cc}`");
    assert!(!bytes.is_empty(), "index.html should not be empty");
}

#[tokio::test]
async fn get_hashed_asset_uses_long_max_age_and_js_content_type() {
    // The build script writes this stub when MEZAME_SKIP_UI_BUILD=1.
    let req = Request::get("/assets/main.abc123.js")
        .body(Body::empty())
        .unwrap();
    let (status, _, headers) = run_request(req).await;

    assert_eq!(status, StatusCode::OK);
    let ct = headers.get("content-type").unwrap().to_str().unwrap();
    assert!(
        ct.starts_with("application/javascript"),
        "content-type was `{ct}`"
    );
    let cc = headers.get("cache-control").unwrap().to_str().unwrap();
    assert!(
        cc.contains("max-age=31536000") && cc.contains("immutable"),
        "cache-control was `{cc}`"
    );
}

#[tokio::test]
async fn unknown_path_falls_back_to_index_html() {
    let req = Request::get("/some/spa/route").body(Body::empty()).unwrap();
    let (status, _, headers) = run_request(req).await;

    assert_eq!(status, StatusCode::OK);
    let ct = headers.get("content-type").unwrap().to_str().unwrap();
    assert!(
        ct.starts_with("text/html"),
        "SPA fallback should serve text/html, got `{ct}`"
    );
}

#[tokio::test]
async fn get_sw_js_uses_no_cache_headers() {
    let req = Request::get("/sw.js").body(Body::empty()).unwrap();
    let (status, _, headers) = run_request(req).await;

    assert_eq!(status, StatusCode::OK);
    let ct = headers.get("content-type").unwrap().to_str().unwrap();
    assert!(
        ct.starts_with("application/javascript"),
        "sw.js should be served as JS, got `{ct}`"
    );
    let cc = headers.get("cache-control").unwrap().to_str().unwrap();
    // The service worker must not be aggressively cached or the browser
    // can keep serving an outdated copy that never updates.
    assert!(cc.contains("no-cache"), "sw.js cache-control was `{cc}`");
}

#[tokio::test]
async fn top_level_static_file_uses_short_cache() {
    // `favicon.png` lives at dist root, not under `assets/`. It should
    // get the default short cache, not the year-long immutable one.
    let req = Request::get("/favicon.png").body(Body::empty()).unwrap();
    let (status, _, headers) = run_request(req).await;

    assert_eq!(status, StatusCode::OK);
    let ct = headers.get("content-type").unwrap().to_str().unwrap();
    assert_eq!(ct, "image/png");
    let cc = headers.get("cache-control").unwrap().to_str().unwrap();
    assert!(
        cc.contains("max-age=3600") && !cc.contains("immutable"),
        "top-level static cache-control was `{cc}`"
    );
}

// ---------- the Host and Origin checks ----------

/// Send one request through a router built on `state`.
async fn run_on(state: Arc<AppState>, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let req = as_browser(&state, req).await;
    let res = build_router(state)
        .oneshot(req)
        .await
        .expect("router did not respond");
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1024 * 1024)
        .await
        .expect("body read")
        .to_vec();
    (status, bytes)
}

#[tokio::test]
async fn a_request_for_a_hostname_this_server_does_not_serve_is_misdirected() {
    // DNS rebinding: a page at attacker.example, re-pointed at 127.0.0.1,
    // sends its own name in `Host`. Every route answers 421, the SPA
    // fallback included, and no handler runs: the PUT leaves no file.
    for (method, path) in [
        ("GET", "/state"),
        ("GET", "/history?session=x"),
        ("GET", "/"),
        ("GET", "/assets/app.js"),
        ("PUT", "/state"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "attacker.example:9510")
            .header("origin", "http://attacker.example:9510")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"settings":{}}"#))
            .unwrap();
        let (status, body, _) = run_request(req).await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{method} {path}");
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("attacker.example"),
            "the refusal names the host it refused"
        );
        assert!(
            text.contains("restart"),
            "the refusal says the config is read at startup: {text}"
        );
    }
}

#[tokio::test]
async fn requests_for_loopback_local_and_configured_names_are_served() {
    for host in [
        "127.0.0.1:9510",
        "localhost:9510",
        "[::1]:9510",
        "192.168.1.20:9510",
        "stefans-mac.local:9510",
    ] {
        let req = Request::get("/state")
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = run_request(req).await;
        assert_eq!(status, StatusCode::OK, "{host} names this server");
    }

    // The public hostname a tunnel passes through in `Host`: refused until
    // it is listed, served once it is.
    let req = || {
        Request::get("/state")
            .header("host", "mezame.example.com")
            .body(Body::empty())
            .unwrap()
    };
    let (status, _) = run_on(dummy_state(), req()).await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "unlisted");
    let (status, _) = run_on(state_with_hosts(&["mezame.example.com"]), req()).await;
    assert_eq!(status, StatusCode::OK, "listed under hosts");
}

#[tokio::test]
async fn a_write_from_another_origin_is_forbidden_and_leaves_no_trace() {
    // Cross-site: a page at evil.example fetches PUT /state at loopback.
    // The browser sends that page's `Origin`; the write is refused before
    // the handler, so no file is written and no `state_changes` tick fires.
    let state = dummy_state();
    let mut rx = state.state_changes.subscribe();
    let req = Request::put("/state")
        .header("host", "127.0.0.1:9510")
        .header("origin", "http://evil.example")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"settings":{}}"#))
        .unwrap();
    let (status, body) = run_on(state, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("evil.example"),
        "the refusal names the origin it refused"
    );
    assert!(
        text.contains("hosts"),
        "the refusal names the remedy: {text}"
    );
    assert!(rx.try_recv().is_err(), "no tick for a refused write");
}

#[tokio::test]
async fn a_write_from_the_page_this_server_served_goes_through() {
    let put = |origin: &str, host: &str| {
        Request::put("/state")
            .header("host", host)
            .header("origin", origin)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"settings":{}}"#))
            .unwrap()
    };
    let hosts = || state_with_hosts(&["mezame.example.com"]);

    for (origin, host) in [
        // The UI at loopback.
        ("http://127.0.0.1:9510", "127.0.0.1:9510"),
        // Behind a tunnel: the public name in both headers.
        ("https://mezame.example.com", "mezame.example.com"),
        // Behind a proxy that rewrote `Host` to the bind address: the
        // configured name in `Origin` is enough on its own.
        ("https://mezame.example.com", "127.0.0.1:9510"),
    ] {
        let (status, _) = run_on(hosts(), put(origin, host)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{origin} sent to {host}");
    }

    for (origin, host) in [
        // Another port on the same host is another page.
        ("http://127.0.0.1:8080", "127.0.0.1:9510"),
        // A sandboxed frame or a `file://` page.
        ("null", "127.0.0.1:9510"),
        // A name that is not listed, whatever `Host` says.
        ("https://evil.example", "mezame.example.com"),
    ] {
        let (status, _) = run_on(hosts(), put(origin, host)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin} sent to {host}");
    }
}

#[tokio::test]
async fn a_read_over_get_carries_no_origin_check() {
    // The browser withholds a cross-origin response on its own, and the
    // `Host` check covers the rebound page that would read it. A client
    // sending a stray `Origin` on a GET is served.
    let req = Request::get("/state")
        .header("host", "127.0.0.1:9510")
        .header("origin", "http://evil.example")
        .body(Body::empty())
        .unwrap();
    let (status, bytes, _) = run_request(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_body(&bytes),
        json!({ "sessions": [], "closed": [], "settings": {} })
    );
}

#[tokio::test]
async fn a_write_carrying_neither_origin_nor_sec_fetch_site_is_refused() {
    // Phase 0 let a request with no `Origin` through as a non-browser
    // client. Behind a login every write is a browser's, and a browser
    // sends `Origin` or `Sec-Fetch-Site` on every request the check covers;
    // neither means a client that is not a browser, refused 403 ahead of
    // the login layer (phase 2 Requirement 6 criterion 3). A script adds
    // `Sec-Fetch-Site: none`.
    let state = dummy_state();
    let cookie = state.login_for_test("alice", "correct horse battery").await;
    let bare = Request::put("/state")
        .header(axum::http::header::COOKIE, cookie.clone())
        .header("content-type", "application/json")
        .body(Body::from(r#"{"settings":{}}"#))
        .unwrap();
    let res = build_router(state.clone()).oneshot(bare).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = to_bytes(res.into_body(), 4096).await.unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("Sec-Fetch-Site"),
        "the body names the way out"
    );
    // The same write with the script's marker goes through.
    let marked = Request::put("/state")
        .header(axum::http::header::COOKIE, cookie)
        .header("sec-fetch-site", "none")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"settings":{}}"#))
        .unwrap();
    let res = build_router(state).oneshot(marked).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    // A plain GET is not covered: nothing beyond the cookie is needed.
    let (status, _, _) = run_request(Request::get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
}
