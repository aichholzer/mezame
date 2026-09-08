//! Tests for the path-resolution and load helpers in `mezame::config`.
//! Mutates the process-global `HOME` env var. Every test in this file
//! takes a file-scoped mutex, the same pattern as
//! `tests/session_steal_stale_lock.rs`.

use std::sync::OnceLock;

use std::path::Path;

use mezame::config::{
    config_path, eligible_workspace_root, load_config, Config, TransportConfig, WorkspaceIneligible,
};
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
async fn config_path_errors_when_home_unset() {
    let _g = home_lock().lock().await;
    unset_home();

    let err = config_path().expect_err("HOME unset should error");
    assert!(err.to_string().contains("HOME"));
}

#[test]
fn the_working_directory_is_a_workspace_root_unless_it_is_the_root_the_home_or_holds_mezame() {
    // Requirement 3 criterion 5: a pure rule over two paths, no `HOME`
    // read, so it needs no lock.
    let mezame = Path::new("/home/alice/.mezame");
    assert_eq!(
        eligible_workspace_root(Path::new("/home/alice/project"), mezame),
        Ok(std::path::PathBuf::from("/home/alice/project"))
    );
    assert_eq!(
        eligible_workspace_root(Path::new("/srv/mezame"), mezame),
        Ok(std::path::PathBuf::from("/srv/mezame"))
    );
    assert_eq!(
        eligible_workspace_root(Path::new("/"), mezame),
        Err(WorkspaceIneligible::Root)
    );
    assert_eq!(
        eligible_workspace_root(Path::new("/home/alice"), mezame),
        Err(WorkspaceIneligible::Home)
    );
    for holds_or_is in ["/home", "/home/alice/.mezame", "/home/alice/.mezame/inner"] {
        assert_eq!(
            eligible_workspace_root(Path::new(holds_or_is), mezame),
            Err(WorkspaceIneligible::MezameDir),
            "{holds_or_is}"
        );
    }
    // A sibling that merely shares a prefix is a directory of its own.
    assert!(eligible_workspace_root(Path::new("/home/alice/.mezame-work"), mezame).is_ok());
    assert!(eligible_workspace_root(Path::new("/home/alice2"), mezame).is_ok());
    // Each reason reads as a sentence for the startup line.
    for why in [
        WorkspaceIneligible::Root,
        WorkspaceIneligible::Home,
        WorkspaceIneligible::MezameDir,
    ] {
        assert!(
            why.to_string().starts_with("the working directory"),
            "{why}"
        );
    }
}

#[tokio::test]
async fn load_config_reads_a_well_formed_json_file() {
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    set_home(tmp.path());

    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    let body = json!({
        "version": 2,
        "transports": [
            { "kind": "cloudflared", "bind": "127.0.0.1:9510" }
        ],
        "agent_cmd": "kiro-cli",
        "agent_args": ["acp"]
    });
    std::fs::write(dir.join("config.json"), body.to_string()).unwrap();

    let cfg: Config = load_config().expect("load_config");
    assert_eq!(cfg.transports.len(), 1);
    let TransportConfig::Cloudflared { bind, hosts } = &cfg.transports[0];
    assert_eq!(bind, "127.0.0.1:9510");
    assert!(
        hosts.is_empty(),
        "a file without the key reads as no extra hosts"
    );
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
