//! Integration tests for `mezame::unix::send_signal`. Unix-only.

#![cfg(unix)]

use mezame::unix::send_signal;

#[test]
fn signal_zero_to_self_succeeds() {
    // `kill(self, 0)` queries existence without delivering a signal;
    // the process always exists, so the syscall must return 0. Doubles
    // as a smoke test for the FFI symbol resolution.
    let pid = std::process::id() as i32;
    assert_eq!(send_signal(pid, 0), 0);
}

#[test]
fn signal_zero_to_known_dead_pid_fails() {
    // Highest 32-bit positive value is virtually guaranteed to be
    // out of the kernel's PID range. `kill` must return -1 with
    // ESRCH when the target does not exist.
    assert_eq!(send_signal(i32::MAX, 0), -1);
}
