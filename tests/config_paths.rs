//! Tests for the path-resolution and load helpers in `mezame::config`.
//! Mutates the process-global `HOME` env var. Every test in this file
//! takes a file-scoped mutex, the same pattern as
//! `tests/session_steal_stale_lock.rs`.

use std::sync::OnceLock;

use std::path::Path;

use mezame::config::{
    config_path, eligible_workspace_root, load_config, mezame_dir, resolve_workspace_root, Config,
    TransportConfig, WorkspaceIneligible,
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

#[tokio::test]
async fn an_empty_home_is_refused_like_an_unset_one() {
    // Joined as given, an empty HOME would name `.mezame` relative to the
    // working directory, and `init` would write the key and the datastore
    // there; it is refused with the line an unset HOME gets.
    let _g = home_lock().lock().await;
    unset_home();
    let unset = config_path()
        .expect_err("HOME unset should error")
        .to_string();
    std::env::set_var("HOME", "");
    let empty = config_path()
        .expect_err("an empty HOME should error")
        .to_string();
    assert_eq!(empty, unset, "the same line for both");
    assert!(empty.contains("HOME"), "{empty}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_home_reached_through_a_symlink_is_still_refused_as_a_workspace_root() {
    // `getcwd` resolves symlinks and `HOME` is taken as given, so the two
    // spellings of one home directory never compare equal on the raw paths.
    // The resolving check canonicalizes both, through the nearest existing
    // ancestor for `~/.mezame`, which need not exist yet.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    let real = tmp.path().join("realhome");
    std::fs::create_dir_all(real.join("project")).unwrap();
    let link = tmp.path().join("linkhome");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    set_home(&link);
    let dir = mezame_dir().unwrap();
    assert_eq!(dir, link.join(".mezame"), "built from HOME as given");
    assert!(!dir.exists(), "the directory does not exist yet");
    let resolved = real.canonicalize().unwrap();
    assert!(
        eligible_workspace_root(&resolved, &dir).is_ok(),
        "the raw comparison lets the home directory through"
    );

    assert_eq!(
        resolve_workspace_root(&resolved, &dir),
        Err(WorkspaceIneligible::Home),
        "the home directory is refused however it is spelled"
    );
    assert_eq!(
        resolve_workspace_root(&resolved.join("project"), &dir),
        Ok(resolved.join("project")),
        "a directory under it is accepted"
    );
    assert_eq!(
        resolve_workspace_root(&link.join("project"), &dir),
        Ok(resolved.join("project")),
        "and named by its canonical path"
    );
    std::fs::create_dir_all(real.join(".mezame/inner")).unwrap();
    assert_eq!(
        resolve_workspace_root(&resolved.join(".mezame/inner"), &dir),
        Err(WorkspaceIneligible::MezameDir),
        "once the directory exists, a directory inside it is refused too"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_mezame_dir_that_is_a_symlink_still_refuses_the_home_and_refuses_its_target() {
    // A datastore kept on another volume: `~/.mezame` is itself a symlink
    // to a directory elsewhere, so the directory canonicalized whole no
    // longer has the home as its parent, and a check made on that spelling
    // alone would let the home directory through. The home is refused on
    // the spelling built from the canonical home; the link's target, a
    // directory holding it and one inside it are refused on the canonical
    // directory, since the key and the datastore sit there in fact.
    let _g = home_lock().lock().await;
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let target = base.join("data/mz");
    std::fs::create_dir_all(home.join("project")).unwrap();
    std::fs::create_dir_all(target.join("inner")).unwrap();
    std::os::unix::fs::symlink(&target, home.join(".mezame")).unwrap();
    set_home(&home);
    let dir = mezame_dir().unwrap();
    assert_eq!(dir, home.join(".mezame"), "built from HOME as given");
    assert_eq!(
        dir.canonicalize().unwrap(),
        target,
        "and resolving to the other directory"
    );

    assert_eq!(
        resolve_workspace_root(&home, &dir),
        Err(WorkspaceIneligible::Home),
        "the home directory is refused although the canonical directory sits elsewhere"
    );
    assert_eq!(
        resolve_workspace_root(&target, &dir),
        Err(WorkspaceIneligible::MezameDir),
        "the link's target is ~/.mezame"
    );
    assert_eq!(
        resolve_workspace_root(&target.join("inner"), &dir),
        Err(WorkspaceIneligible::MezameDir),
        "and so is a directory inside it"
    );
    assert_eq!(
        resolve_workspace_root(&base.join("data"), &dir),
        Err(WorkspaceIneligible::MezameDir),
        "a directory holding the target holds the key and the datastore"
    );
    assert_eq!(
        resolve_workspace_root(&home.join("project"), &dir),
        Ok(home.join("project")),
        "a directory under the home is accepted"
    );
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
