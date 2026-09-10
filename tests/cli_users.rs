//! `mezame user add`, `mezame user list` and `mezame passwd` on the binary
//! in a temporary home, and the server's answer to a cookie signed before
//! a password change.

use std::io::{BufRead, BufReader, Read, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mezame::auth::{sign, Cookie, COOKIE_NAME};
use mezame::store::crypto::MasterKey;
use mezame::store::sqlite::SqliteStore;
use mezame::store::{Role, Store};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mezame")
}

fn run_with_home(args: &[&str], home: &Path) -> Output {
    Command::new(bin())
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("spawn mezame")
}

fn run_with_stdin(args: &[&str], home: &Path, input: &str) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mezame");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write the input");
    child.wait_with_output().expect("mezame exits")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_success(out: &Output) {
    assert!(out.status.success(), "{:?}: {}", out.status, stderr(out));
}

/// A home set up with the admin `alice` and the given bind.
fn home_with_admin(bind: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &[
            "init",
            "--bind",
            bind,
            "--admin",
            "alice",
            "--password-stdin",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    tmp
}

fn open_store(home: &Path) -> Arc<SqliteStore> {
    let keys = MasterKey::load(&home.join(".mezame/master.key"))
        .expect("the key loads")
        .keys();
    Arc::new(SqliteStore::open(&home.join(".mezame/mezame.db"), keys).expect("the store opens"))
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn users(home: &Path) -> Vec<(String, Role, u64)> {
    let store = open_store(home);
    block_on(async {
        store
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .map(|u| (u.name, u.role, u.session_epoch))
            .collect()
    })
}

#[test]
fn user_add_without_a_terminal_needs_password_stdin() {
    let tmp = home_with_admin("127.0.0.1:9510");
    let out = run_with_home(&["user", "add", "bob"], tmp.path());
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("--password-stdin"),
        "{}",
        stderr(&out)
    );
    assert_eq!(users(tmp.path()).len(), 1, "nothing was created");
}

#[test]
fn user_add_creates_users_and_list_shows_them_without_a_hash() {
    // Requirement 10 criterion 5: the flag path, the role flag, and the
    // list's shape.
    let tmp = home_with_admin("127.0.0.1:9510");
    let out = run_with_stdin(
        &["user", "add", "bob", "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    assert!(
        stdout(&out).contains("Created user `bob` (user)"),
        "{}",
        stdout(&out)
    );
    let out = run_with_stdin(
        &["user", "add", "--admin", "carol", "--password-stdin"],
        tmp.path(),
        &format!("{PASSWORD}\n"),
    );
    assert_success(&out);
    assert!(
        stdout(&out).contains("Created user `carol` (admin)"),
        "{}",
        stdout(&out)
    );

    let list = run_with_home(&["user", "list"], tmp.path());
    assert_success(&list);
    let printed = stdout(&list);
    let lines: Vec<Vec<&str>> = printed
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(lines.len(), 3, "{printed}");
    let today = mezame::prompt::today_utc().to_string();
    assert_eq!(lines[0], vec!["alice", "admin", today.as_str()]);
    assert_eq!(lines[1], vec!["bob", "user", today.as_str()]);
    assert_eq!(lines[2], vec!["carol", "admin", today.as_str()]);
    assert!(!printed.contains("$argon2"), "{printed}");
    assert!(!printed.contains(PASSWORD), "{printed}");

    // No workspace row is created for a user by `user add`: that happens
    // on their first session.
    let store = open_store(tmp.path());
    let bob = block_on(async {
        let bob = store.user_by_name("bob").await.unwrap().unwrap();
        store.default_workspace(&bob.id).await.unwrap()
    });
    assert!(bob.is_none());
}

#[test]
fn user_add_refuses_a_taken_an_empty_and_a_long_name_naming_the_rule() {
    let tmp = home_with_admin("127.0.0.1:9510");
    let out = run_with_stdin(
        &["user", "add", "alice", "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("already taken"), "{}", stderr(&out));

    let out = run_with_stdin(
        &["user", "add", "", "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("a user name is required"),
        "{}",
        stderr(&out)
    );

    let long = "x".repeat(65);
    let out = run_with_stdin(
        &["user", "add", &long, "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("at most 64 characters"),
        "{}",
        stderr(&out)
    );

    let out = run_with_home(&["user", "add"], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("needs a name"), "{}", stderr(&out));

    let out = run_with_stdin(
        &["user", "add", "dave", "--password-stdin"],
        tmp.path(),
        "short\n",
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("at least 8 characters"),
        "{}",
        stderr(&out)
    );

    assert_eq!(users(tmp.path()).len(), 1);
}

#[test]
fn user_takes_add_or_list_and_nothing_else() {
    let tmp = home_with_admin("127.0.0.1:9510");
    for args in [
        &["user"][..],
        &["user", "remove", "x"][..],
        &["user", "list", "x"][..],
    ] {
        let out = run_with_home(args, tmp.path());
        assert!(!out.status.success(), "{args:?}");
        assert!(stderr(&out).contains("mezame user"), "{}", stderr(&out));
    }
    let out = run_with_home(&["frobnicate"], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Unknown command"), "{}", stderr(&out));
}

#[test]
fn user_list_on_an_empty_datastore_says_how_the_first_is_made() {
    // An `init` that names no admin leaves a datastore with no user in it.
    let tmp = TempDir::new().unwrap();
    assert_success(&run_with_home(
        &["init", "--bind", "127.0.0.1:9510"],
        tmp.path(),
    ));
    let out = run_with_home(&["user", "list"], tmp.path());
    assert_success(&out);
    assert!(stdout(&out).contains("No users yet"), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("--admin NAME --password-stdin"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn the_user_commands_refuse_to_run_before_init_and_create_nothing() {
    // The commands print rows or refuse; none of them is a reason to make
    // a key or a datastore. Before `init`, each exits with one line naming
    // it, and `~/.mezame` gains no key and no datastore.
    for args in [
        &["user", "list"][..],
        &["user", "add", "bob", "--password-stdin"][..],
        &["passwd", "alice", "--password-stdin"][..],
    ] {
        let tmp = TempDir::new().unwrap();
        let out = run_with_home(args, tmp.path());
        assert!(!out.status.success(), "{args:?}: {}", stdout(&out));
        let err = stderr(&out);
        assert!(err.contains("No datastore yet"), "{args:?}: {err}");
        assert!(err.contains("`mezame init`"), "{args:?}: {err}");
        assert_eq!(
            err.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "{args:?}: one line: {err}"
        );
        assert!(
            !tmp.path().join(".mezame/master.key").exists(),
            "{args:?}: no key was written"
        );
        assert!(
            !tmp.path().join(".mezame/mezame.db").exists(),
            "{args:?}: no datastore was created"
        );
        assert!(
            !tmp.path().join(".mezame").exists(),
            "{args:?}: not even the directory"
        );
    }

    // A datastore whose key is gone is refused with the server's line, and
    // no key is written beside it.
    let tmp = home_with_admin("127.0.0.1:9510");
    let key_path = tmp.path().join(".mezame/master.key");
    let db_path = tmp.path().join(".mezame/mezame.db");
    std::fs::remove_file(&key_path).unwrap();
    let before = std::fs::read(&db_path).unwrap();
    for args in [
        &["user", "list"][..],
        &["user", "add", "bob", "--password-stdin"][..],
        &["passwd", "alice", "--password-stdin"][..],
    ] {
        let out = run_with_home(args, tmp.path());
        assert!(!out.status.success(), "{args:?}: {}", stdout(&out));
        let err = stderr(&out);
        assert!(err.contains("backup"), "{args:?}: {err}");
        assert!(
            err.contains(&key_path.display().to_string()),
            "{args:?}: {err}"
        );
        assert!(!key_path.exists(), "{args:?}: no key is written");
    }
    assert_eq!(
        std::fs::read(&db_path).unwrap(),
        before,
        "the datastore is untouched"
    );
}

#[test]
fn passwd_refuses_an_unknown_name_and_needs_a_name() {
    let tmp = home_with_admin("127.0.0.1:9510");
    let out = run_with_stdin(
        &["passwd", "nobody", "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no user named `nobody`"),
        "{}",
        stderr(&out)
    );
    let out = run_with_home(&["passwd"], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("needs a name"), "{}", stderr(&out));
    let out = run_with_home(&["passwd", "alice"], tmp.path());
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("--password-stdin"),
        "{}",
        stderr(&out)
    );
    assert_eq!(users(tmp.path())[0].2, 0, "the epoch is untouched");
}

// ---------- the epoch, through the server ----------

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The binary serving `home`, returned once it reports listening.
fn start_server(home: &Path) -> Child {
    let mut child = Command::new(bin())
        .env("HOME", home)
        .env_remove("AWS_PROFILE")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mezame");
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "the binary did not report startup in time"
        );
        let line = lines.next().expect("stderr stays open").expect("a line");
        if line.contains("listening on") {
            break;
        }
    }
    // Keep draining stderr so the child never blocks on a full pipe.
    std::thread::spawn(move || for _line in lines {});
    child
}

/// `GET /state` with `cookie`, answered as a status line.
fn get_state(port: u16, cookie: &str) -> String {
    let mut status = String::new();
    for _ in 0..50 {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {COOKIE_NAME}={cookie}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            status = response.lines().next().unwrap_or_default().to_string();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    status
}

#[test]
fn passwd_bumps_the_epoch_and_the_server_refuses_the_cookies_signed_before_it() {
    // Requirement 10 criterion 5 and 8: a cookie signed under the home's
    // cookie key with the user's epoch is accepted before `passwd` and
    // refused after, through `GET /state` on the started binary.
    let port = free_port();
    let tmp = home_with_admin(&format!("127.0.0.1:{port}"));
    let keys = MasterKey::load(&tmp.path().join(".mezame/master.key"))
        .unwrap()
        .keys();
    let (user_id, epoch_before) = {
        let store = open_store(tmp.path());
        block_on(async {
            let alice = store.user_by_name("alice").await.unwrap().unwrap();
            (alice.id, alice.session_epoch)
        })
    };
    let now = mezame::auth::now_unix();
    let cookie = sign(&Cookie::issue(&user_id, epoch_before, now), &keys.cookie);

    let mut server = start_server(tmp.path());
    let status = get_state(port, &cookie);
    let _ = server.kill();
    let _ = server.wait();
    assert!(status.starts_with("HTTP/1.1 200"), "before: {status}");

    let out = run_with_stdin(
        &["passwd", "alice", "--password-stdin"],
        tmp.path(),
        "a new password entirely\n",
    );
    assert_success(&out);
    assert!(
        stdout(&out).contains("Password changed for `alice`"),
        "{}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("a new password entirely"));
    assert_eq!(
        users(tmp.path())[0].2,
        epoch_before + 1,
        "the epoch moved by one"
    );

    let mut server = start_server(tmp.path());
    let status = get_state(port, &cookie);
    let _ = server.kill();
    let _ = server.wait();
    assert!(status.starts_with("HTTP/1.1 401"), "after: {status}");
}
