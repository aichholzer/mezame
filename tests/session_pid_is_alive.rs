//! Integration tests for `mezame::session::pid_is_alive`. Unix-only;
//! the non-Unix stub is conservative by design (always returns true)
//! and not worth a dedicated test.

#![cfg(unix)]

use mezame::session::pid_is_alive;

#[test]
fn self_pid_is_alive() {
    let pid = std::process::id() as i32;
    assert!(pid_is_alive(pid));
}

#[test]
fn known_dead_pid_is_not_alive() {
    // Highest 32-bit positive value is virtually guaranteed to be out
    // of the kernel's PID range.
    assert!(!pid_is_alive(i32::MAX));
}
