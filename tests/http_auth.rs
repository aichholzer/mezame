//! The login layer over the router (Requirements 5 and 6 of the phase 2
//! spec): what passes without a cookie, what answers 401, the three
//! endpoints, the sliding renewal, the `Secure` rule, the limiter, and the
//! guard's `Sec-Fetch-Site` rule.

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use mezame::auth::{
    self, hash_password, verify_calls_for_test, Cookie, RateLimiter, COOKIE_LIFETIME, COOKIE_NAME,
    LOGIN_LIMIT,
};
use mezame::config::{Config, TransportConfig};
use mezame::http::{build_router, AppState, Clock, LOGIN_REQUIRED};
use mezame::hub::HubRegistry;
use mezame::store::crypto::{MasterKey, KEY_LEN};
use mezame::store::sqlite::SqliteStore;
use mezame::store::Role;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Notify};
use tower::ServiceExt;

const NOW: i64 = 1_800_000_000;

fn config(public_url: Option<&str>) -> Config {
    Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".to_string(),
            hosts: vec!["mezame.example.com".to_string()],
        }],
        version: 2,
        datastore: Default::default(),
        public_url: public_url.map(str::to_string),
        models: vec![],
        bedrock: None,
    }
}

/// A state with a clock fixed at `now` and a fresh limiter.
fn state_at(now: i64, public_url: Option<&str>) -> Arc<AppState> {
    let keys = MasterKey::from_bytes_for_test([9u8; KEY_LEN]).keys();
    let store = SqliteStore::open_in_memory(keys.clone()).unwrap();
    let (state_changes, _) = broadcast::channel(8);
    let clock: Clock = Arc::new(move || now);
    Arc::new(AppState {
        config: ArcSwap::from_pointee(config(public_url)),
        hubs: HubRegistry::new(),
        store: Arc::new(store),
        keys,
        limiter: RateLimiter::default(),
        clock,
        state_changes,
        shutdown: Arc::new(Notify::new()),
    })
}

async fn create_alice(state: &AppState) -> String {
    let hash = hash_password("correct horse battery").unwrap();
    state
        .store
        .create_user("alice", &hash, Role::Admin, NOW * 1000)
        .await
        .unwrap()
        .id
}

async fn send(
    state: &Arc<AppState>,
    req: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let res = build_router(state.clone())
        .oneshot(req)
        .await
        .expect("router responded");
    let status = res.status();
    let headers = res.headers().clone();
    let body = to_bytes(res.into_body(), 1 << 20).await.unwrap().to_vec();
    (status, headers, body)
}

fn login_request(username: &str, password: &str) -> Request<Body> {
    Request::post("/login")
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "username": username, "password": password }).to_string(),
        ))
        .unwrap()
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::get(path);
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, format!("{COOKIE_NAME}={cookie}"));
    }
    req.body(Body::empty()).unwrap()
}

fn set_cookie_of(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().to_string())
}

fn cookie_value_of(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches(&format!("{COOKIE_NAME}="))
        .to_string()
}

#[tokio::test]
async fn the_ui_shell_and_me_and_login_are_public_and_everything_else_is_401() {
    let state = state_at(NOW, None);
    for path in ["/", "/index.html", "/some/spa/route", "/sw.js"] {
        let (status, _, _) = send(&state, get(path, None)).await;
        assert_eq!(status, StatusCode::OK, "{path} is served without a cookie");
    }
    let (status, _, body) = send(&state, get("/me", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(String::from_utf8_lossy(&body), LOGIN_REQUIRED);
    for path in ["/state", "/history?session=x", "/state/events"] {
        let (status, _, body) = send(&state, get(path, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(String::from_utf8_lossy(&body), LOGIN_REQUIRED, "{path}");
    }
    let logout = Request::post("/logout")
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(&state, logout).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A tampered cookie is the same 401, with no other information.
    let (status, headers, body) = send(&state, get("/state", Some("garbage"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(String::from_utf8_lossy(&body), LOGIN_REQUIRED);
    assert!(set_cookie_of(&headers).is_none());
}

#[tokio::test]
async fn login_sets_the_cookie_and_answers_who_you_are() {
    let state = state_at(NOW, None);
    let id = create_alice(&state).await;
    let (status, headers, body) =
        send(&state, login_request("alice", "correct horse battery")).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({ "id": id, "name": "alice", "role": "admin" }));
    let set = set_cookie_of(&headers).expect("a Set-Cookie header");
    assert!(set.starts_with(&format!("{COOKIE_NAME}=")), "{set}");
    assert!(set.contains("; HttpOnly"), "{set}");
    assert!(set.contains("; SameSite=Lax"), "{set}");
    assert!(set.contains("; Path=/"), "{set}");
    assert!(
        set.contains(&format!("; Max-Age={}", COOKIE_LIFETIME.as_secs())),
        "{set}"
    );
    assert!(!set.contains("Secure"), "plain HTTP with no public URL");
    let value = cookie_value_of(&set);
    let cookie = auth::verify(&value, &state.keys.cookie, NOW).expect("a valid cookie");
    assert_eq!(cookie.user_id, id);
    assert_eq!(cookie.epoch, 0);
    assert_eq!(cookie.expiry, NOW + COOKIE_LIFETIME.as_secs() as i64);
    // The cookie opens the protected routes and `/me`.
    let (status, _, body) = send(&state, get("/me", Some(&value))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["name"],
        "alice"
    );
    let (status, _, _) = send(&state, get("/history?session=x", Some(&value))).await;
    assert_eq!(status, StatusCode::OK);
    // A trimmed name logs in too.
    let (status, _, _) = send(&state, login_request("  alice ", "correct horse battery")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_user_are_the_same_401_and_each_costs_one_verification() {
    let state = state_at(NOW, None);
    create_alice(&state).await;
    let before = verify_calls_for_test();
    let (status, headers, body) = send(&state, login_request("alice", "wrong password")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(set_cookie_of(&headers).is_none());
    let wrong_body = String::from_utf8_lossy(&body).to_string();
    let (status, _, body) = send(&state, login_request("nobody", "wrong password")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        String::from_utf8_lossy(&body),
        wrong_body,
        "one body for both"
    );
    assert_eq!(
        verify_calls_for_test() - before,
        2,
        "one verification each, the unknown user against the dummy hash"
    );
}

#[tokio::test]
async fn a_body_that_is_not_the_login_json_is_400() {
    let state = state_at(NOW, None);
    for body in [
        "",
        "not json",
        r#"{"user":"alice"}"#,
        r#"{"username":"alice"}"#,
        "[]",
    ] {
        let req = Request::post("/login")
            .header("sec-fetch-site", "same-origin")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, _, _) = send(&state, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    }
    // No content type at all.
    let req = Request::post("/login")
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(r#"{"username":"a","password":"b"}"#))
        .unwrap();
    let (status, _, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn logout_clears_the_cookie_on_this_device_only() {
    let state = state_at(NOW, None);
    let id = create_alice(&state).await;
    let value = auth::sign(&Cookie::issue(&id, 0, NOW), &state.keys.cookie);
    let req = Request::post("/logout")
        .header("sec-fetch-site", "same-origin")
        .header(header::COOKIE, format!("{COOKIE_NAME}={value}"))
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let set = set_cookie_of(&headers).expect("a clearing Set-Cookie");
    assert!(set.starts_with(&format!("{COOKIE_NAME}=;")), "{set}");
    assert!(set.contains("Max-Age=0"), "{set}");
    // The epoch did not move: the other device's cookie still works.
    let (status, _, _) = send(&state, get("/me", Some(&value))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_cookie_is_renewed_under_thirty_days_and_left_alone_above() {
    let id;
    let value;
    {
        let state = state_at(NOW, None);
        id = create_alice(&state).await;
        value = auth::sign(&Cookie::issue(&id, 0, NOW), &state.keys.cookie);
    }
    let day = 24 * 60 * 60;
    let lifetime = COOKIE_LIFETIME.as_secs() as i64;
    // 31 days left: nothing re-issued.
    let later = state_at(NOW + lifetime - 31 * day, None);
    let hash = hash_password("correct horse battery").unwrap();
    // The same user id in this store: re-create with the same row id is not
    // possible, so sign against the row this store holds instead.
    let _ = hash;
    let alice = create_alice(&later).await;
    let value31 = auth::sign(&Cookie::issue(&alice, 0, NOW), &later.keys.cookie);
    let (status, headers, _) = send(&later, get("/me", Some(&value31))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers_state, _) = send(&later, get("/state", Some(&value31))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(set_cookie_of(&headers).is_none() && set_cookie_of(&headers_state).is_none());
    // 29 days left: the protected route re-issues a fresh 90 days.
    let renewing = state_at(NOW + lifetime - 29 * day, None);
    let alice = create_alice(&renewing).await;
    let value29 = auth::sign(&Cookie::issue(&alice, 0, NOW), &renewing.keys.cookie);
    let (status, headers, _) = send(&renewing, get("/state", Some(&value29))).await;
    assert_eq!(status, StatusCode::OK);
    let set = set_cookie_of(&headers).expect("re-issued");
    let renewed = auth::verify(
        &cookie_value_of(&set),
        &renewing.keys.cookie,
        NOW + lifetime - 29 * day,
    )
    .expect("a valid renewed cookie");
    assert_eq!(renewed.expiry, NOW + lifetime - 29 * day + lifetime);
    assert_eq!(renewed.user_id, alice);
    let _ = (id, value);
}

#[tokio::test]
async fn secure_follows_the_forwarded_proto_or_the_public_url_and_never_the_bind() {
    let state = state_at(NOW, None);
    create_alice(&state).await;
    // Neither signal: plain.
    let (_, headers, _) = send(&state, login_request("alice", "correct horse battery")).await;
    assert!(!set_cookie_of(&headers).unwrap().contains("Secure"));
    // X-Forwarded-Proto: https.
    let mut req = login_request("alice", "correct horse battery");
    req.headers_mut()
        .insert("x-forwarded-proto", "https".parse().unwrap());
    let (_, headers, _) = send(&state, req).await;
    assert!(set_cookie_of(&headers).unwrap().ends_with("; Secure"));
    // A public URL over https.
    let https = state_at(NOW, Some("https://mezame.example.com"));
    create_alice(&https).await;
    let (_, headers, _) = send(&https, login_request("alice", "correct horse battery")).await;
    assert!(set_cookie_of(&headers).unwrap().ends_with("; Secure"));
    // A public URL over http: plain.
    let http = state_at(NOW, Some("http://mezame.lan:9510"));
    create_alice(&http).await;
    let (_, headers, _) = send(&http, login_request("alice", "correct horse battery")).await;
    assert!(!set_cookie_of(&headers).unwrap().contains("Secure"));
}

#[tokio::test]
async fn the_eleventh_attempt_in_a_minute_is_429_with_retry_after() {
    let state = state_at(NOW, None);
    create_alice(&state).await;
    for i in 0..LOGIN_LIMIT {
        let (status, _, _) = send(&state, login_request("alice", "wrong")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i}");
    }
    let before = verify_calls_for_test();
    let (status, headers, _) = send(&state, login_request("alice", "correct horse battery")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = headers
        .get(header::RETRY_AFTER)
        .expect("Retry-After")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry), "{retry}");
    assert_eq!(
        verify_calls_for_test(),
        before,
        "refused before the hash is checked"
    );
    // Another name is not affected.
    let (status, _, _) = send(&state, login_request("bob", "whatever")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_cookie_of_a_stale_epoch_is_refused_once_the_password_changes() {
    let state = state_at(NOW, None);
    let id = create_alice(&state).await;
    let value = auth::sign(&Cookie::issue(&id, 0, NOW), &state.keys.cookie);
    let (status, _, _) = send(&state, get("/state", Some(&value))).await;
    assert_eq!(status, StatusCode::OK);
    let new_hash = hash_password("a new password").unwrap();
    state.store.set_password_hash(&id, &new_hash).await.unwrap();
    let (status, _, _) = send(&state, get("/state", Some(&value))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the epoch moved on");
    let (status, _, _) = send(&state, get("/me", Some(&value))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A fresh login carries the new epoch and works.
    let (status, headers, _) = send(&state, login_request("alice", "a new password")).await;
    assert_eq!(status, StatusCode::OK);
    let fresh = cookie_value_of(&set_cookie_of(&headers).unwrap());
    assert_eq!(
        auth::verify(&fresh, &state.keys.cookie, NOW).unwrap().epoch,
        1
    );
    let (status, _, _) = send(&state, get("/state", Some(&fresh))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn sec_fetch_site_decides_a_write_with_no_origin() {
    let state = state_at(NOW, None);
    let id = create_alice(&state).await;
    let value = auth::sign(&Cookie::issue(&id, 0, NOW), &state.keys.cookie);
    let put = |site: Option<&str>, origin: Option<&str>, forwarded_host: Option<&str>| {
        let mut req = Request::put("/state")
            .header(header::COOKIE, format!("{COOKIE_NAME}={value}"))
            .header("content-type", "application/json");
        if let Some(site) = site {
            req = req.header("sec-fetch-site", site);
        }
        if let Some(origin) = origin {
            req = req.header("origin", origin);
        }
        if let Some(host) = forwarded_host {
            req = req.header("x-forwarded-host", host);
        }
        req.body(Body::from(r#"{"sessions":[]}"#)).unwrap()
    };
    // The write handler itself is not under test: a 204 or a 500 (no
    // writable HOME in this process) both mean the guard let it through.
    let through =
        |status: StatusCode| status != StatusCode::FORBIDDEN && status != StatusCode::UNAUTHORIZED;
    let (status, _, _) = send(&state, put(Some("same-origin"), None, None)).await;
    assert!(through(status), "same-origin passes: {status}");
    let (status, _, _) = send(&state, put(Some("none"), None, None)).await;
    assert!(through(status), "none passes: {status}");
    for refused in ["cross-site", "same-site", "anything"] {
        let (status, _, body) = send(&state, put(Some(refused), None, None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert!(String::from_utf8_lossy(&body).contains("Sec-Fetch-Site"));
    }
    let (status, _, _) = send(&state, put(None, None, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "neither header");
    // Origin wins over Sec-Fetch-Site and is compared against the host the
    // browser used: X-Forwarded-Host when a proxy rewrote Host.
    let mut req = put(
        Some("cross-site"),
        Some("http://mezame.example.com"),
        Some("mezame.example.com"),
    );
    req.headers_mut()
        .insert("host", "127.0.0.1:9510".parse().unwrap());
    let (status, _, _) = send(&state, req).await;
    assert!(
        through(status),
        "a matching Origin behind a proxy passes: {status}"
    );
    let mut req = put(
        None,
        Some("http://mezame.example.com"),
        Some("other.example.com"),
    );
    req.headers_mut()
        .insert("host", "127.0.0.1:9510".parse().unwrap());
    let (status, _, _) = send(&state, req).await;
    assert!(
        through(status),
        "a listed origin passes whatever the host says: {status}"
    );
    let mut req = put(
        None,
        Some("http://evil.example"),
        Some("mezame.example.com"),
    );
    req.headers_mut()
        .insert("host", "127.0.0.1:9510".parse().unwrap());
    let (status, _, _) = send(&state, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a foreign Origin is refused");
}
