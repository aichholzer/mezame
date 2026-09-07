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

/// Run the binary with `args` and an explicit `HOME`, with stdin closed.
/// An accidental interactive prompt then fails fast; open stdin would
/// leave the test hanging.
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
    // A well-formed config with no transports must fail loudly. Doing
    // nothing in silence is the failure mode this guards. It exercises
    // the `[]` arm of run()'s transport match, plus the full
    // config-discovery and runtime-build path that precedes it.
    let tmp = home_with_config(r#"{ "transports": [] }"#);
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
    // Multi-transport is parsed and not yet runnable. run() bails on the
    // `_` arm. Serving only the first entry in silence is the failure
    // mode this guards.
    let body = r#"{
        "transports": [
            { "kind": "cloudflared", "bind": "127.0.0.1:9510" },
            { "kind": "cloudflared", "bind": "127.0.0.1:9511" }
        ]
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
    // A prompt that cannot be answered writes nothing. A half-written
    // config would be served on the next start.
    assert!(
        !tmp.path().join(".mezame/config.json").exists(),
        "no config.json is written when standard input cannot be read"
    );
}

// ---------- phase 1: startup with a Bedrock section ----------

/// Spawn the binary on `body` with no AWS credentials in its environment
/// and a region set, read stderr until `Backend:` and the listening line
/// appear, fetch `/`, and return the backend line, the response status
/// line, and the child (killed on drop).
fn start_and_fetch_root(body: &str, port: u16) -> (String, String) {
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    let tmp = home_with_config(body);
    let mut child = Command::new(bin())
        .env("HOME", tmp.path())
        // No credentials: startup must not need any. A region, so the
        // SDK's chain never probes the instance metadata service.
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_BEARER_TOKEN_BEDROCK")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mezame");
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let mut backend_line = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "the binary did not report startup in time"
        );
        let line = lines.next().expect("stderr stays open").expect("a line");
        if line.starts_with("Backend:") {
            backend_line = line.clone();
        }
        if line.contains("listening on") {
            break;
        }
    }
    let mut status = String::new();
    for _ in 0..50 {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            status = response.lines().next().unwrap_or_default().to_string();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    (backend_line, status)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn a_bedrock_configuration_starts_with_no_credentials_and_names_its_backend() {
    let port = free_port();
    let body = format!(
        r#"{{"transports":[{{"kind":"cloudflared","bind":"127.0.0.1:{port}"}}],"bedrock":{{"model":"anthropic.claude-sonnet-5","region":"us-east-1"}}}}"#
    );
    let (backend, status) = start_and_fetch_root(&body, port);
    assert!(
        backend.starts_with("Backend: Bedrock anthropic.claude-sonnet-5"),
        "{backend}"
    );
    assert!(backend.contains("region: us-east-1"), "{backend}");
    assert!(backend.contains("profile: default chain"), "{backend}");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}

#[test]
fn a_configuration_without_a_bedrock_section_names_the_echo() {
    let port = free_port();
    let body = format!(r#"{{"transports":[{{"kind":"cloudflared","bind":"127.0.0.1:{port}"}}]}}"#);
    let (backend, status) = start_and_fetch_root(&body, port);
    assert!(backend.starts_with("Backend: echo"), "{backend}");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}
