//! `/state`, `PUT /state` and `/sessions/{id}` per user (phase 2
//! Requirement 7 criteria 1 to 3 and 6, Requirement 6 criteria 1 and 8):
//! two users see their own rows and settings alone, the shapes and limits
//! of the three routes, and the tick that reaches the owner.
//!
//! Driven through `build_router` with `oneshot` over the in-memory store
//! `AppState::for_test` opens. The archive path over a live hub is
//! covered in `tests/ws_upgrade.rs`; here a registered hub is enough to
//! see that a close reaches it.

mod support;

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use mezame::config::{Config, TransportConfig};
use mezame::http::{
    build_router, session_patch_of, settings_of_body, AppState, SessionPatch,
    SESSION_TITLE_MAX_CHARS, SETTINGS_MAX_BYTES,
};
use mezame::hub::HubRegistry;
use mezame::store::ARCHIVED_LIST_MAX;
use serde_json::{json, Value};
use support::ScriptedBackend;
use tower::ServiceExt;

const T0: i64 = 1_700_000_000_000;

fn state() -> Arc<AppState> {
    state_with(HubRegistry::new())
}

fn state_with(hubs: HubRegistry) -> Arc<AppState> {
    AppState::for_test(
        Config {
            transports: vec![TransportConfig::Cloudflared {
                bind: "127.0.0.1:0".to_string(),
                hosts: vec![],
            }],
            version: 2,
            datastore: Default::default(),
            public_url: None,
            models: vec![],
            bedrock: None,
        },
        hubs,
        16,
    )
}

/// A user's cookie and id, the user created on first use.
async fn user(state: &AppState, name: &str) -> (String, String) {
    let cookie = state.login_for_test(name, "correct horse battery").await;
    let id = state
        .store
        .user_by_name(name)
        .await
        .unwrap()
        .expect("the user exists")
        .id;
    (cookie, id)
}

async fn send(
    state: &Arc<AppState>,
    cookie: &str,
    req: axum::http::request::Builder,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let req = req
        .header(header::COOKIE, cookie)
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/json")
        .body(match body {
            Some(body) => Body::from(body.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let res = build_router(state.clone())
        .oneshot(req)
        .await
        .expect("the router answers");
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap().to_vec();
    (status, bytes)
}

async fn send_raw(
    state: &Arc<AppState>,
    cookie: &str,
    req: axum::http::request::Builder,
    body: &str,
) -> StatusCode {
    let req = req
        .header(header::COOKIE, cookie)
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    build_router(state.clone())
        .oneshot(req)
        .await
        .expect("the router answers")
        .status()
}

async fn get_state(state: &Arc<AppState>, cookie: &str) -> Value {
    let (status, bytes) = send(state, cookie, Request::get("/state"), None).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&bytes).expect("JSON")
}

fn ids(list: &Value) -> Vec<&str> {
    list.as_array()
        .expect("an array")
        .iter()
        .map(|row| row["id"].as_str().expect("an id"))
        .collect()
}

#[tokio::test]
async fn two_users_see_only_their_own_sessions_and_settings() {
    let state = state();
    let (alice, alice_id) = user(&state, "alice").await;
    let (bob, bob_id) = user(&state, "bob").await;
    for (owner, id, when) in [
        (&alice_id, "a1", T0),
        (&alice_id, "a2", T0 + 1),
        (&bob_id, "b1", T0 + 2),
    ] {
        state
            .store
            .create_session(owner, id, None, when)
            .await
            .unwrap();
    }
    state
        .store
        .set_settings(&alice_id, &json!({ "theme": "dark" }))
        .await
        .unwrap();

    let alices = get_state(&state, &alice).await;
    assert_eq!(ids(&alices["sessions"]), vec!["a1", "a2"], "by creation");
    assert_eq!(alices["closed"], json!([]));
    assert_eq!(alices["settings"], json!({ "theme": "dark" }));

    let bobs = get_state(&state, &bob).await;
    assert_eq!(ids(&bobs["sessions"]), vec!["b1"]);
    assert_eq!(bobs["settings"], json!({}), "the default settings object");
}

#[tokio::test]
async fn the_state_document_has_the_shape_of_requirement_7() {
    let state = state();
    let (alice, alice_id) = user(&state, "alice").await;
    state
        .store
        .create_session(&alice_id, "open", None, T0)
        .await
        .unwrap();
    state
        .store
        .set_title("titled", "Named", T0 + 5)
        .await
        .unwrap_or(());
    state
        .store
        .create_session(&alice_id, "titled", None, T0 + 1)
        .await
        .unwrap();
    state
        .store
        .set_title("titled", "Named", T0 + 5)
        .await
        .unwrap();
    // More archived rows than the list carries, closed in order.
    for n in 0..(ARCHIVED_LIST_MAX + 2) {
        let id = format!("closed{n:02}");
        state
            .store
            .create_session(&alice_id, &id, None, T0 + 10 + n as i64)
            .await
            .unwrap();
        state
            .store
            .set_archived(&id, true, T0 + 1_000 + n as i64)
            .await
            .unwrap();
    }

    let doc = get_state(&state, &alice).await;
    assert_eq!(
        doc["sessions"],
        json!([
            { "id": "open", "title": null, "created": T0, "updated": T0 },
            { "id": "titled", "title": "Named", "created": T0 + 1, "updated": T0 + 5 },
        ])
    );
    let closed = doc["closed"].as_array().unwrap();
    assert_eq!(closed.len(), ARCHIVED_LIST_MAX, "at most twenty");
    assert_eq!(
        closed[0],
        json!({
            "id": format!("closed{:02}", ARCHIVED_LIST_MAX + 1),
            "title": null,
            "closedAt": T0 + 1_000 + (ARCHIVED_LIST_MAX + 1) as i64
        }),
        "the newest archived first"
    );
    assert_eq!(closed[ARCHIVED_LIST_MAX - 1]["id"], "closed02");
    assert_eq!(
        doc.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["closed", "sessions", "settings"]
    );
}

#[tokio::test]
async fn put_state_replaces_the_settings_within_the_cap_and_refuses_any_other_body() {
    let state = state();
    let (alice, alice_id) = user(&state, "alice").await;
    let mut ticks = state.state_changes.subscribe();

    let settings = json!({ "theme": "dark", "sendOnEnter": true, "idleSuspendMinutes": 10 });
    let (status, _) = send(
        &state,
        &alice,
        Request::put("/state"),
        Some(json!({ "settings": settings })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        ticks.try_recv().unwrap(),
        alice_id,
        "one tick for the owner"
    );
    assert!(ticks.try_recv().is_err());
    assert_eq!(get_state(&state, &alice).await["settings"], settings);
    assert_eq!(state.store.settings(&alice_id).await.unwrap(), settings);

    // The cap: a value that serialises past it is refused and the old
    // settings stay.
    let big = json!({ "settings": { "pad": "x".repeat(SETTINGS_MAX_BYTES) } });
    let (status, _) = send(&state, &alice, Request::put("/state"), Some(big)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let fits = json!({ "settings": { "pad": "x".repeat(SETTINGS_MAX_BYTES - 10) } });
    let (status, _) = send(&state, &alice, Request::put("/state"), Some(fits)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let _ = ticks.try_recv();

    for other in [
        r#"{"settings": []}"#,
        r#"{"settings": "dark"}"#,
        r#"{"settings": {}, "sessions": []}"#,
        r#"{"sessions": []}"#,
        r#"[]"#,
        r#"not json"#,
        r#""#,
    ] {
        let status = send_raw(&state, &alice, Request::put("/state"), other).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{other:?}");
        assert!(ticks.try_recv().is_err(), "no tick for {other:?}");
    }
    assert_eq!(
        state.store.settings(&alice_id).await.unwrap()["pad"]
            .as_str()
            .map(str::len),
        Some(SETTINGS_MAX_BYTES - 10),
        "a refused body changed nothing"
    );
    // The pure half.
    assert_eq!(
        settings_of_body(br#"{"settings":{"a":1}}"#),
        Some(json!({ "a": 1 }))
    );
    assert_eq!(settings_of_body(br#"{"settings":{"a":1},"b":2}"#), None);
}

#[tokio::test]
async fn patch_renames_within_the_limits_and_refuses_other_shapes() {
    let state = state();
    let (alice, alice_id) = user(&state, "alice").await;
    state
        .store
        .create_session(&alice_id, "s1", None, T0)
        .await
        .unwrap();
    let mut ticks = state.state_changes.subscribe();

    let (status, _) = send(
        &state,
        &alice,
        Request::patch("/sessions/s1"),
        Some(json!({ "title": "  New name  " })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(ticks.try_recv().unwrap(), alice_id);
    let row = state.store.session("s1").await.unwrap().unwrap();
    assert_eq!(row.title.as_deref(), Some("New name"), "trimmed");
    assert!(row.updated > T0, "a rename touches `updated`");

    let longest = "t".repeat(SESSION_TITLE_MAX_CHARS);
    let (status, _) = send(
        &state,
        &alice,
        Request::patch("/sessions/s1"),
        Some(json!({ "title": longest })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let _ = ticks.try_recv();

    for bad in [
        json!({ "title": "" }),
        json!({ "title": "   " }),
        json!({ "title": "t".repeat(SESSION_TITLE_MAX_CHARS + 1) }),
        json!({ "title": 5 }),
        json!({ "title": "x", "archived": true }),
        json!({ "archived": "yes" }),
        json!({ "renamed": "x" }),
        json!([]),
    ] {
        let (status, _) = send(
            &state,
            &alice,
            Request::patch("/sessions/s1"),
            Some(bad.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert!(
            ticks.try_recv().is_err(),
            "a failed PATCH fires no tick: {bad}"
        );
    }
    assert_eq!(
        state
            .store
            .session("s1")
            .await
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some(longest.as_str()),
        "nothing changed"
    );
    // The pure half.
    assert_eq!(
        session_patch_of(br#"{"title":" a "}"#),
        Some(SessionPatch::Title("a".to_string()))
    );
    assert_eq!(
        session_patch_of(br#"{"archived":false}"#),
        Some(SessionPatch::Archived(false))
    );
    assert_eq!(session_patch_of(b"{}"), None);
}

#[tokio::test]
async fn archive_restore_and_delete_answer_the_owner_and_404_everyone_else() {
    let state = state();
    let (alice, alice_id) = user(&state, "alice").await;
    let (bob, bob_id) = user(&state, "bob").await;
    state
        .store
        .create_session(&alice_id, "mine", None, T0)
        .await
        .unwrap();
    state
        .store
        .create_session(&bob_id, "his", None, T0)
        .await
        .unwrap();
    let mut ticks = state.state_changes.subscribe();

    let (status, _) = send(
        &state,
        &alice,
        Request::patch("/sessions/mine"),
        Some(json!({ "archived": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(ticks.try_recv().unwrap(), alice_id);
    let doc = get_state(&state, &alice).await;
    assert_eq!(doc["sessions"], json!([]));
    assert_eq!(doc["closed"][0]["id"], "mine");
    assert!(doc["closed"][0]["closedAt"].is_i64());

    let (status, _) = send(
        &state,
        &alice,
        Request::patch("/sessions/mine"),
        Some(json!({ "archived": false })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(ticks.try_recv().unwrap(), alice_id);
    assert_eq!(
        ids(&get_state(&state, &alice).await["sessions"]),
        vec!["mine"]
    );

    // Another user's row and an unknown one: the same 404, no tick.
    for (path, body) in [
        ("/sessions/his", Some(json!({ "archived": true }))),
        ("/sessions/his", Some(json!({ "title": "stolen" }))),
        ("/sessions/nowhere", Some(json!({ "archived": true }))),
    ] {
        let (status, bytes) = send(&state, &alice, Request::patch(path), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(String::from_utf8_lossy(&bytes), "no such session\n");
        assert!(ticks.try_recv().is_err());
    }
    for path in ["/sessions/his", "/sessions/nowhere"] {
        let (status, _) = send(&state, &alice, Request::delete(path), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(ticks.try_recv().is_err());
    }
    assert_eq!(
        state.store.session("his").await.unwrap().unwrap().title,
        None,
        "bob's row is untouched"
    );
    assert_eq!(ids(&get_state(&state, &bob).await["sessions"]), vec!["his"]);

    // Delete forgets the row; the owner is ticked once.
    let (status, _) = send(&state, &alice, Request::delete("/sessions/mine"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(ticks.try_recv().unwrap(), alice_id);
    assert!(state.store.session("mine").await.unwrap().is_none());
    assert_eq!(get_state(&state, &alice).await["sessions"], json!([]));
    // And is a 404 from then on.
    let (status, _) = send(&state, &alice, Request::delete("/sessions/mine"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn archiving_or_deleting_a_session_closes_its_registered_hub() {
    let hubs = HubRegistry::new();
    let state = state_with(hubs);
    let (alice, alice_id) = user(&state, "alice").await;
    for id in ["live1", "live2"] {
        state
            .store
            .create_session(&alice_id, id, None, T0)
            .await
            .unwrap();
        let _attached = state
            .hubs
            .register_for_test(
                Arc::new(ScriptedBackend::new()),
                id.to_string(),
                json!({ "type": "ready", "sessionId": id }),
                None,
            )
            .await;
    }
    assert!(state.hubs.is_registered_for_test("live1").await);

    let (status, _) = send(
        &state,
        &alice,
        Request::patch("/sessions/live1"),
        Some(json!({ "archived": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(gone(&state, "live1").await, "the archive closed the hub");
    assert!(
        state.hubs.is_registered_for_test("live2").await,
        "the other hub is untouched"
    );

    let (status, _) = send(&state, &alice, Request::delete("/sessions/live2"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(gone(&state, "live2").await, "the delete closed the hub");
}

/// Poll until no hub is registered under `id`, within five seconds.
async fn gone(state: &AppState, id: &str) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if !state.hubs.is_registered_for_test(id).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn the_session_routes_are_behind_the_login() {
    let state = state();
    for req in [
        Request::patch("/sessions/x"),
        Request::delete("/sessions/x"),
        Request::put("/state"),
    ] {
        let req = req
            .header("sec-fetch-site", "same-origin")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"archived":true}"#))
            .unwrap();
        let res = build_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
