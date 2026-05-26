//! Tests for the path-resolution and load helpers in `mezame::config`.
//! Mutates the process-global `HOME` env var, so all tests in this file
//! take a file-scoped mutex (same pattern as
//! `tests/session_steal_stale_lock.rs`).

use std::sync::OnceLock;

use mezame::config::{config_path, load_config, state_path, Config, TransportConfig};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Mutex;

fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn set_home(p: &std::path::Path) {
    std::env::set_var("HOME", p);
}

fn unset_home() {
    std::env::remove_var("HOME");
}

#[tokio::test]
async fn config_path_appends_dotmezame_config_json() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let p = config_path().expect("config_path");
    assert_eq!(p, tmp.path().join(".mezame/config.json"));
}

#[tokio::test]
async fn state_path_appends_dotmezame_state_json() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let p = state_path().expect("state_path");
    assert_eq!(p, tmp.path().join(".mezame/state.json"));
}

#[tokio::test]
async fn config_path_errors_when_home_unset() {
    let _g = home_lock().lock().await;
    unset_home();

    let err = config_path().expect_err("HOME unset should error");
    assert!(err.to_string().contains("HOME"));
}

#[tokio::test]
async fn state_path_errors_when_home_unset() {
    let _g = home_lock().lock().await;
    unset_home();

    let err = state_path().expect_err("HOME unset should error");
    assert!(err.to_string().contains("HOME"));
}

#[tokio::test]
async fn load_config_reads_a_well_formed_json_file() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    let body = json!({
        "transports": [
            { "kind": "cloudflared", "bind": "127.0.0.1:9510" }
        ],
        "agent_cmd": "kiro-cli",
        "agent_args": ["acp"]
    });
    std::fs::write(dir.join("config.json"), body.to_string()).unwrap();

    let cfg: Config = load_config().expect("load_config");
    assert_eq!(cfg.agent_cmd, "kiro-cli");
    assert_eq!(cfg.agent_args, vec!["acp"]);
    assert_eq!(cfg.transports.len(), 1);
    let TransportConfig::Cloudflared { bind } = &cfg.transports[0];
    assert_eq!(bind, "127.0.0.1:9510");
}

#[tokio::test]
async fn load_config_defaults_agent_args_when_missing() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    // `agent_args` deliberately omitted; serde should populate the
    // default empty Vec via `#[serde(default)]`.
    let body = json!({
        "transports": [
            { "kind": "cloudflared", "bind": "127.0.0.1:9510" }
        ],
        "agent_cmd": "claude"
    });
    std::fs::write(dir.join("config.json"), body.to_string()).unwrap();

    let cfg = load_config().expect("load_config");
    assert!(cfg.agent_args.is_empty());
}

#[tokio::test]
async fn load_config_errors_when_file_missing() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let err = load_config().expect_err("missing file should error");
    assert!(
        err.to_string().contains("Reading"),
        "error should mention Reading: {err}"
    );
}

#[tokio::test]
async fn load_config_errors_on_malformed_json() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), "{ this is not json").unwrap();

    let err = load_config().expect_err("malformed json should error");
    assert!(
        err.to_string().contains("Parsing"),
        "error should mention Parsing: {err}"
    );
}
