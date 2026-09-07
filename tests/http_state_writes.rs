//! Writes to `/state` under contention and under failure (Requirement 14
//! criteria 4 and 10). The round-trip and broadcast cases live in
//! `tests/http_routes.rs`; these are the ones about the file itself.
//!
//! Every case mutates `HOME`, so a process-wide mutex serialises them.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::future::join_all;
use mezame::config::{Config, TransportConfig};
use mezame::http::{build_router, AppState};
use mezame::hub::HubRegistry;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::{broadcast, Mutex, Notify};
use tower::ServiceExt;

fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn set_home(p: &Path) {
    std::env::set_var("HOME", p);
}

fn state() -> Arc<AppState> {
    let (state_changes, _) = broadcast::channel(8);
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

fn put(body: Value) -> Request<Body> {
    Request::put("/state")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// The temporary siblings left in `dir`, if any.
fn temps_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read the directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_put_state_writers_all_get_204_and_leave_one_complete_file() {
    // Two browsers syncing at the same moment used to race on one fixed
    // temporary name: one renamed a file the other had just truncated,
    // the loser's rename failed with a 500, and a fresh page load then
    // restored nothing until the next sync. This case drives eight writers
    // through the router at once and holds every one to a 204 and the
    // file to a complete document. The writes are small and rarely
    // overlap on the blocking pool, so the race itself is forced at the
    // helper level in `tests/config_fs.rs`; this is the end-to-end shape.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());
    let pad = "x".repeat(8192);

    for round in 0..8u32 {
        let app_state = state();
        let writes = (0..8u32).map(|writer| {
            let app = build_router(app_state.clone());
            let body = json!({ "round": round, "writer": writer, "pad": pad });
            async move {
                app.oneshot(put(body))
                    .await
                    .expect("router did not respond")
                    .status()
            }
        });
        for status in join_all(writes).await {
            assert_eq!(status, StatusCode::NO_CONTENT, "round {round}");
        }

        let raw = std::fs::read_to_string(tmp.path().join(".mezame/state.json"))
            .expect("the state file exists");
        let value: Value = serde_json::from_str(&raw).expect("the file is a complete document");
        assert_eq!(
            value["round"], round,
            "the file holds one of this round's writes"
        );
        assert!(value["writer"].as_u64().is_some_and(|w| w < 8));
        assert_eq!(value["pad"].as_str().map(str::len), Some(8192));
    }
    assert!(
        temps_in(&tmp.path().join(".mezame")).is_empty(),
        "no temporary sibling is left behind"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn put_state_creates_a_private_directory_and_an_owner_only_file() {
    use std::os::unix::fs::PermissionsExt;
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let status = build_router(state())
        .oneshot(put(json!({ "sessions": [] })))
        .await
        .expect("router did not respond")
        .status();
    assert_eq!(status, StatusCode::NO_CONTENT);

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(&tmp.path().join(".mezame")),
        0o700,
        "the directory is owner-only"
    );
    assert_eq!(
        mode(&tmp.path().join(".mezame/state.json")),
        0o600,
        "the state file is owner-only"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_write_leaves_the_existing_file_alone_and_no_temp_behind() {
    // Requirement 14 criterion 10. An unwritable directory makes the
    // sibling's creation fail; the existing file is untouched and the
    // response is a 500. Root ignores modes, so the case returns early
    // when run as root.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    // Root ignores modes; it is detected by who owns the directory this
    // test just created, since `docker run` leaves `USER` unset.
    if std::fs::metadata(tmp.path()).is_ok_and(|m| m.uid() == 0) {
        return;
    }
    set_home(tmp.path());
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("state.json"), b"{\"kept\":true}").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let app_state = state();
    let mut ticks = app_state.state_changes.subscribe();
    let status = build_router(app_state)
        .oneshot(put(json!({ "sessions": [] })))
        .await
        .expect("router did not respond")
        .status();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        std::fs::read(dir.join("state.json")).unwrap(),
        b"{\"kept\":true}",
        "the existing file is as it was"
    );
    assert!(
        temps_in(&dir).is_empty(),
        "no temporary sibling is left behind"
    );
    assert!(ticks.try_recv().is_err(), "a failed write fires no event");
}
