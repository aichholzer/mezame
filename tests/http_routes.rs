//! HTTP integration tests for the cloudflared transport.
//!
//! Drives `mezame::http::build_router` via `tower::ServiceExt::oneshot`
//! so we hit the real axum routing, the real handlers, and the embedded
//! UI bundle without binding a TCP port.
//!
//! Several tests mutate `HOME` so `state_path()` and the history reader
//! resolve into a tempdir. Cargo runs tests in parallel by default; a
//! single process-wide `Mutex` serialises every test in this file so
//! the env var is never observed mid-swap.

mod support;

use std::path::Path;
use std::sync::OnceLock;

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
use tempfile::TempDir;
use tokio::sync::{broadcast, Mutex, Notify};
use tower::ServiceExt;

fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn dummy_state() -> Arc<AppState> {
    state_with_hosts(&[])
}

/// A state whose transport lists `hosts`, the public names a tunnel or
/// proxy in front of Mezame carries.
fn state_with_hosts(hosts: &[&str]) -> Arc<AppState> {
    let (state_changes, _) = broadcast::channel(8);
    Arc::new(AppState {
        config: Arc::new(Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
                hosts: hosts.iter().map(|h| h.to_string()).collect(),
            }],
        }),
        hubs: HubRegistry::new(),
        state_changes,
        shutdown: Arc::new(Notify::new()),
    })
}

/// Send a single request through the router and return (status, body).
async fn run_request(req: Request<Body>) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let app = build_router(dummy_state());
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

// SAFETY: every test in this file takes `home_lock()` before touching the
// env, and the unsafe set/remove calls below never race. Rust 2024 will
// require `unsafe { std::env::set_var(...) }`. This crate is on 2021, and
// the helpers document the contract in the meantime.
fn set_home(p: &Path) {
    std::env::set_var("HOME", p);
}

fn unset_home() {
    std::env::remove_var("HOME");
}

// ---------- /state ----------

#[tokio::test]
async fn get_state_with_no_file_returns_empty_object() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let req = Request::get("/state").body(Body::empty()).unwrap();
    let (status, bytes, _) = run_request(req).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&bytes), json!({}));
}

#[tokio::test]
async fn put_state_then_get_state_round_trip() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let payload = json!({ "sessions": [{ "id": "s1", "label": "1" }] });
    let req = Request::put("/state")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();
    let (status, _, _) = run_request(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Confirm the file actually landed where state_path() expects.
    let state_file = tmp.path().join(".mezame/state.json");
    assert!(state_file.exists(), "state.json should exist after PUT");

    let req = Request::get("/state").body(Body::empty()).unwrap();
    let (status, bytes, _) = run_request(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&bytes), payload);
}

#[tokio::test]
async fn put_state_fires_state_changed_broadcast() {
    // Two browsers cooperating: the second one is subscribed to the
    // broadcast and should receive a tick the moment the first writes
    // a new state. Without this, peer browsers only see another
    // browser's new session after a manual reload.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let state = dummy_state();
    let mut rx = state.state_changes.subscribe();
    let app = build_router(state);

    let payload = json!({ "sessions": [{ "id": "s1", "label": "1" }] });
    let req = Request::put("/state")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();
    let res = app.oneshot(req).await.expect("router responded");
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // The tick must have been queued by the time put_state returned.
    rx.try_recv().expect("state_changes should have ticked");
}

// ---------- /history ----------

#[tokio::test]
async fn get_history_without_session_param_is_400() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let req = Request::get("/history").body(Body::empty()).unwrap();
    let (status, _, _) = run_request(req).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_history_with_an_empty_session_param_is_400() {
    // The other branch of Requirement 13 criterion 2, which had no case.
    // Both branches answer 400 with a plain-text body naming what is
    // missing.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

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
    let _g = home_lock().lock().await;
    unset_home();

    let transcript = vec![
        HistoryEntry {
            body: EntryBody::User {
                text: "ping".to_string(),
            },
            timestamp: 1_000,
        },
        HistoryEntry {
            body: EntryBody::Agent {
                text: "ping".to_string(),
            },
            timestamp: 1_000,
        },
        HistoryEntry {
            body: EntryBody::Thought {
                text: "thinking".to_string(),
            },
            timestamp: 1_100,
        },
        HistoryEntry {
            body: EntryBody::Sys {
                text: "a notice".to_string(),
            },
            timestamp: 1_200,
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

    let (state_changes, _) = broadcast::channel(8);
    let state = Arc::new(AppState {
        config: Arc::new(Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
                hosts: vec![],
            }],
        }),
        hubs,
        state_changes,
        shutdown: Arc::new(Notify::new()),
    });

    let app = build_router(state);
    let res = app
        .oneshot(
            Request::get("/history?session=hist-session")
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
async fn get_history_for_an_unknown_session_returns_empty_entries() {
    // Requirement 13 criterion 4. This is what a value holding `/`, `\`
    // or `..` gets too: no such value can be a registry key, so no
    // separate validation step is needed. Nothing is created.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    for id in ["nobody-here", "../etc/passwd", "a/b", "a%5Cb"] {
        let req = Request::get(format!("/history?session={id}"))
            .body(Body::empty())
            .unwrap();
        let (status, bytes, _) = run_request(req).await;
        assert_eq!(status, StatusCode::OK, "id {id:?} answers 200");
        assert_eq!(json_body(&bytes), json!({ "entries": [] }), "id {id:?}");
    }
}

// ---------- SPA fallback / asset routing ----------

#[tokio::test]
async fn get_root_serves_index_html_with_no_cache() {
    let _g = home_lock().lock().await;
    // No HOME mutation needed; the asset path does not touch the
    // filesystem. We still take the lock so we observe a stable env.
    unset_home();

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
    let _g = home_lock().lock().await;

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
    let _g = home_lock().lock().await;

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
    let _g = home_lock().lock().await;

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
    let _g = home_lock().lock().await;

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

// ---------- error paths ----------

#[tokio::test]
async fn get_state_returns_500_when_the_state_file_cannot_be_read() {
    // A `state.json` that exists as a directory fails the read with
    // something other than `NotFound`, which is the one error the handler
    // absorbs into an empty object. Anything else is reported.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    std::fs::create_dir_all(tmp.path().join(".mezame/state.json")).unwrap();

    let req = Request::get("/state").body(Body::empty()).unwrap();
    let (status, _, _) = run_request(req).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn put_state_returns_500_when_the_parent_cannot_be_created() {
    // `.mezame` occupied by a regular file makes `create_dir_all` fail.
    // The write is refused and the caller is told, and no partial state
    // file is left behind.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    std::fs::write(tmp.path().join(".mezame"), b"not a directory").unwrap();

    let payload = json!({ "sessions": [] });
    let req = Request::put("/state")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();
    let (status, _, _) = run_request(req).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn get_state_serves_an_empty_object_for_malformed_json() {
    // A hand-edited or truncated `state.json` resolves to `{}`. The
    // browser then rebuilds its session list from scratch, and a corrupt
    // file never wedges the UI behind a 500.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("state.json"), "{ truncated").unwrap();

    let req = Request::get("/state").body(Body::empty()).unwrap();
    let (status, bytes, _) = run_request(req).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&bytes), json!({}));
}

// ---------- the Host and Origin checks ----------

/// Send one request through a router built on `state`.
async fn run_on(state: Arc<AppState>, req: Request<Body>) -> (StatusCode, Vec<u8>) {
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
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

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
            .body(Body::from(r#"{"sessions":[]}"#))
            .unwrap();
        let (status, body, _) = run_request(req).await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{method} {path}");
        assert!(
            String::from_utf8_lossy(&body).contains("attacker.example"),
            "the refusal names the host it refused"
        );
    }
    assert!(
        !tmp.path().join(".mezame/state.json").exists(),
        "the PUT never reached its handler"
    );
}

#[tokio::test]
async fn requests_for_loopback_local_and_configured_names_are_served() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

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
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let state = dummy_state();
    let mut rx = state.state_changes.subscribe();
    let req = Request::put("/state")
        .header("host", "127.0.0.1:9510")
        .header("origin", "http://evil.example")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"sessions":[]}"#))
        .unwrap();
    let (status, body) = run_on(state, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        String::from_utf8_lossy(&body).contains("evil.example"),
        "the refusal names the origin it refused"
    );
    assert!(
        !tmp.path().join(".mezame/state.json").exists(),
        "the write never reached its handler"
    );
    assert!(rx.try_recv().is_err(), "no tick for a refused write");
}

#[tokio::test]
async fn a_write_from_the_page_this_server_served_goes_through() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let put = |origin: &str, host: &str| {
        Request::put("/state")
            .header("host", host)
            .header("origin", origin)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"sessions":[]}"#))
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
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let req = Request::get("/state")
        .header("host", "127.0.0.1:9510")
        .header("origin", "http://evil.example")
        .body(Body::empty())
        .unwrap();
    let (status, bytes, _) = run_request(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&bytes), json!({}));
}

#[tokio::test]
async fn a_client_sending_neither_header_is_served() {
    // Every other case in this file sends neither `Host` nor `Origin`,
    // which no browser does: hyper refuses an HTTP/1.1 request without a
    // `Host` before this layer runs, and a browser attaches `Origin` to
    // every request the check covers. The two absences mean a client that
    // is not a browser, and the checks are aimed at browsers.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let req = Request::put("/state")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"sessions":[]}"#))
        .unwrap();
    let (status, _, _) = run_request(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
