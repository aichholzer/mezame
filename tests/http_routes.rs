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
    let (state_changes, _) = broadcast::channel(8);
    Arc::new(AppState {
        config: Arc::new(Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
            }],
            agent_cmd: "/bin/true".to_string(),
            agent_args: vec![],
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
            }],
            agent_cmd: "/bin/true".to_string(),
            agent_args: vec![],
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
