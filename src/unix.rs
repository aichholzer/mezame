//! Tiny Unix FFI helpers shared across modules.
//!
//! We only need two libc symbols (`kill`, `setsid`), so depending on the
//! `libc` crate would be overkill. This module hosts the single source of
//! truth for both bindings; previously they were duplicated across
//! `agent.rs` and `session.rs`.
//!
//! All entry points are gated on `#[cfg(unix)]` at the call sites; the
//! module itself is only compiled on Unix targets.

#![cfg(unix)]

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn setsid() -> i32;
}

/// Send `sig` to `pid`. Returns the libc return value: 0 on success, -1
/// on error (with `errno` set).
///
/// Pass `0` for `sig` to query process existence without delivering a
/// signal. Pass a negative `pid` to target the entire process group of
/// `-pid` (the standard kill(2) idiom).
///
/// SAFETY: `kill` is a thin syscall wrapper. No state is mutated in the
/// caller's address space.
pub fn send_signal(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}

/// Create a new session and process group. Returns the new session id on
/// success, -1 on error.
///
/// Used inside `Command::pre_exec` so the spawned child becomes its own
/// process-group leader. `setsid` is listed as async-signal-safe by
/// POSIX, which is what makes it valid in a `pre_exec` hook.
///
/// SAFETY: must only be called between fork() and exec(); calling it in
/// the parent process would detach the parent from its controlling
/// terminal. `Command::pre_exec` enforces that constraint by contract.
pub(crate) unsafe fn new_session() -> i32 {
    setsid()
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn fork() -> i32;
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    }

    /// Exercise `new_session` in a forked child, the only context where
    /// calling `setsid` is safe: a freshly forked child is never a
    /// process-group leader, so `setsid` succeeds and the new session is
    /// confined to the throwaway child. Calling it directly in the test
    /// process would detach the test runner from its controlling
    /// terminal, and the real production path (inside `Command::pre_exec`)
    /// runs between fork and exec where llvm-cov cannot observe it.
    ///
    /// The child exits via `std::process::exit` (not `_exit`) so the
    /// coverage profile flushes before the child goes; the lib unit-test
    /// binary has no other tests, so the fork happens with effectively
    /// one active thread and the usual fork-without-exec hazards do not
    /// bite.
    #[test]
    fn new_session_succeeds_in_a_forked_child() {
        unsafe {
            let pid = fork();
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                // Child: setsid must return a valid (non-negative) session
                // id. Map success to exit code 0, failure to 1.
                let sid = new_session();
                std::process::exit(if sid >= 0 { 0 } else { 1 });
            }
            // Parent: reap the child and assert it exited cleanly.
            let mut status: i32 = 0;
            let reaped = waitpid(pid, &mut status as *mut i32, 0);
            assert_eq!(reaped, pid, "waitpid did not reap our child");
            let exited_normally = (status & 0x7f) == 0;
            let exit_code = (status >> 8) & 0xff;
            assert!(
                exited_normally && exit_code == 0,
                "child setsid failed: raw status {status}"
            );
        }
    }
}
