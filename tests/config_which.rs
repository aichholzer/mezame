//! Tests for `mezame::config::which`, the `$PATH` lookup behind the
//! agent menu in `mezame init`.
//!
//! Every test swaps the process-global `PATH`. A file-scoped mutex
//! serialises them, and a guard restores the original value on the way
//! out, including on a panicking test. Same shape as the `HOME` handling
//! in `tests/config_paths.rs`.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard, OnceLock};

use mezame::config::which;
use tempfile::TempDir;

fn path_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Holds the lock for the duration of a test and puts `PATH` back as it
/// was when dropped.
struct PathGuard {
    _lock: MutexGuard<'static, ()>,
    original: Option<OsString>,
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// Take the lock and snapshot `PATH`. A poisoned mutex is recovered: a
/// panic in one test would otherwise fail every later one for an
/// unrelated reason.
fn lock_path() -> PathGuard {
    let lock = path_lock().lock().unwrap_or_else(|e| e.into_inner());
    PathGuard {
        _lock: lock,
        original: std::env::var_os("PATH"),
    }
}

#[test]
fn resolves_a_name_against_path() {
    let _g = lock_path();
    let dir = TempDir::new().unwrap();
    let bin = dir.path().join("mezame-fake-agent");
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
    std::env::set_var("PATH", dir.path());

    assert_eq!(which("mezame-fake-agent"), Some(bin));
}

#[test]
fn searches_path_entries_in_order() {
    // Two directories both holding the name. The first entry on `PATH`
    // wins, which is what a shell would do, and the saved config then
    // names the binary the user would have run.
    let _g = lock_path();
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let wanted = first.path().join("mezame-dupe");
    std::fs::write(&wanted, b"first\n").unwrap();
    std::fs::write(second.path().join("mezame-dupe"), b"second\n").unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:{}", first.path().display(), second.path().display()),
    );

    assert_eq!(which("mezame-dupe"), Some(wanted));
}

#[test]
fn skips_a_path_entry_that_does_not_hold_the_name() {
    let _g = lock_path();
    let empty = TempDir::new().unwrap();
    let holder = TempDir::new().unwrap();
    let bin = holder.path().join("mezame-later");
    std::fs::write(&bin, b"x\n").unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:{}", empty.path().display(), holder.path().display()),
    );

    assert_eq!(which("mezame-later"), Some(bin));
}

#[test]
fn returns_none_when_the_name_is_absent() {
    let _g = lock_path();
    let dir = TempDir::new().unwrap();
    std::env::set_var("PATH", dir.path());

    assert_eq!(which("mezame-definitely-not-installed"), None);
}

#[test]
fn ignores_a_directory_bearing_the_name() {
    // The lookup demands a file. A directory called `kiro-cli` on `PATH`
    // would otherwise be written into the config as the agent command and
    // fail at spawn time.
    let _g = lock_path();
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("kiro-cli")).unwrap();
    std::env::set_var("PATH", dir.path());

    assert_eq!(which("kiro-cli"), None);
}

#[test]
fn returns_none_when_path_is_unset() {
    let _g = lock_path();
    std::env::remove_var("PATH");

    assert_eq!(which("sh"), None);
}
