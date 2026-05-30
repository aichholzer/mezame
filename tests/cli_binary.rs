//! End-to-end tests for the CLI entry points in `src/main.rs` and
//! `mezame::run()` (`src/lib.rs`). Those two files are pure process glue:
//! argument dispatch, help/version output, config discovery, and the
//! transport-selection `match`. None of it is reachable from in-process
//! unit tests without standing up a tokio runtime and a real server, so
//! we drive the compiled binary as a subprocess instead.
//!
//! Cargo exposes the built binary to integration tests via
//! `CARGO_BIN_EXE_mezame`. Running it as a child process still counts
//! toward coverage under `cargo llvm-cov`: the child inherits the
//! profile-file pattern and writes its own `.profraw`.
//!
//! Each test points the child at its own temp `HOME` via `Command::env`,
//! so nothing here mutates the parent process environment and the tests
//! need no shared lock.

use std::io::Write;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// Absolute path to the freshly built `mezame` binary, injected by Cargo
/// for integration tests.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mezame")
}

/// Run the binary with `args` and an explicit `HOME`, with stdin closed so
/// any accidental interactive prompt fails fast rather than hanging.
fn run_with_home(args: &[&str], home: &std::path::Path) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("spawn mezame")
}

/// Write a `config.json` under `<home>/.mezame/` and return the temp dir
/// that owns it. Keep the `TempDir` alive for the duration of the test.
fn home_with_config(body: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("config.json")).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    tmp
}

#[test]
fn version_flag_prints_version_and_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["--version"], tmp.path());

    assert!(out.status.success(), "--version should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("mezame {}", env!("CARGO_PKG_VERSION"))),
        "unexpected --version output: {stdout}"
    );
}

#[test]
fn short_version_flag_matches_long_form() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["-V"], tmp.path());

    assert!(out.status.success(), "-V should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "unexpected -V output: {stdout}"
    );
}

#[test]
fn help_flag_prints_usage_and_subcommands() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["--help"], tmp.path());

    assert!(out.status.success(), "--help should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE"), "help missing USAGE: {stdout}");
    assert!(
        stdout.contains("SUBCOMMANDS"),
        "help missing SUBCOMMANDS: {stdout}"
    );
    assert!(
        stdout.contains("init"),
        "help should mention the init subcommand: {stdout}"
    );
}

#[test]
fn short_help_flag_matches_long_form() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["-h"], tmp.path());

    assert!(out.status.success(), "-h should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE"), "help missing USAGE: {stdout}");
}

#[test]
fn empty_transports_config_bails() {
    // A well-formed config with no transports must fail loudly rather than
    // silently doing nothing. This exercises the `[]` arm of run()'s
    // transport match, plus the full config-discovery and runtime-build
    // path that precedes it.
    let tmp = home_with_config(r#"{ "transports": [], "agent_cmd": "cat" }"#);
    let out = run_with_home(&[], tmp.path());

    assert!(
        !out.status.success(),
        "empty transports should be a non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("No transports configured"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn multiple_transports_config_bails() {
    // Multi-transport is parsed but not yet runnable; run() bails on the
    // `_` arm rather than silently serving only the first entry.
    let body = r#"{
        "transports": [
            { "kind": "cloudflared", "bind": "127.0.0.1:9510" },
            { "kind": "cloudflared", "bind": "127.0.0.1:9511" }
        ],
        "agent_cmd": "cat"
    }"#;
    let tmp = home_with_config(body);
    let out = run_with_home(&[], tmp.path());

    assert!(
        !out.status.success(),
        "multiple transports should be a non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("more than one transport"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn missing_config_reports_and_attempts_setup() {
    // No config on disk: run() announces the missing file and drops into
    // interactive setup. With stdin closed the prompt cannot succeed, so
    // the process exits non-zero, but it must first print where it looked.
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&[], tmp.path());

    assert!(
        !out.status.success(),
        "missing config with no stdin should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("No config at"),
        "should report the missing config path: {stderr}"
    );
}
