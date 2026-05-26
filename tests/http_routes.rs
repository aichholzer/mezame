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

use std::path::Path;
use std::sync::OnceLock;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mezame::config::{Config, TransportConfig};
use mezame::http::build_router;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tower::ServiceExt;

fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn dummy_config() -> Arc<Config> {
    Arc::new(Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".to_string(),
        }],
        agent_cmd: "/bin/true".to_string(),
        agent_args: vec![],
    })
}

/// Send a single request through the router and return (status, body).
async fn run_request(req: Request<Body>) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let app = build_router(dummy_config());
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
// env, so the unsafe set/remove calls below never race. Rust 2024 will
// require `unsafe { std::env::set_var(...) }`; we are on 2021, but
// keeping the calls behind helpers documents the contract.
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
async fn get_history_with_traversal_id_is_400() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    // Each candidate exercises a distinct guard in the handler.
    let candidates = [
        "/history?session=..%2Fetc%2Fpasswd",
        "/history?session=foo%2F..%2Fbar",
        "/history?session=",
        "/history?session=..",
    ];
    for url in candidates {
        let req = Request::get(url).body(Body::empty()).unwrap();
        let (status, _, _) = run_request(req).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "expected 400 for traversal/empty id `{url}`"
        );
    }
}

#[tokio::test]
async fn get_history_with_no_fixture_returns_empty_entries() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let req = Request::get("/history?session=00000000-0000-0000-0000-000000000000")
        .body(Body::empty())
        .unwrap();
    let (status, bytes, _) = run_request(req).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&bytes), json!({ "entries": [] }));
}

#[tokio::test]
async fn get_history_with_fixture_returns_parsed_entries() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    // Drop a Kiro JSONL fixture in place. Two turns: a Prompt with a
    // timestamp and an AssistantMessage that should inherit it.
    let dir = tmp.path().join(".kiro/sessions/cli");
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "abc";
    let jsonl = "\
{\"kind\":\"Prompt\",\"data\":{\"content\":[{\"kind\":\"text\",\"data\":\"hello\"}],\"meta\":{\"timestamp\":1700000000}}}
{\"kind\":\"AssistantMessage\",\"data\":{\"content\":[{\"kind\":\"text\",\"data\":\"world\"}]}}
";
    std::fs::write(dir.join(format!("{sid}.jsonl")), jsonl).unwrap();

    let req = Request::get(format!("/history?session={sid}"))
        .body(Body::empty())
        .unwrap();
    let (status, bytes, _) = run_request(req).await;
    assert_eq!(status, StatusCode::OK);

    let body = json_body(&bytes);
    let entries = body.get("entries").and_then(Value::as_array).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["role"], "user");
    assert_eq!(entries[0]["text"], "hello");
    assert_eq!(entries[0]["timestamp"], 1_700_000_000_000_i64);
    assert_eq!(entries[1]["role"], "agent");
    assert_eq!(entries[1]["text"], "world");
    assert_eq!(entries[1]["timestamp"], 1_700_000_000_000_i64);
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
