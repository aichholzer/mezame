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
