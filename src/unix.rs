//! Tiny Unix FFI helpers for child-process teardown.
//!
//! Three libc symbols are all this crate needs: `kill`, `setsid` and
//! `getsid`. The `libc` crate is not a dependency, and this module is the
//! one place the bindings live.
//!
//! The 0.14 harness spawns no process of its own yet, so nothing in this
//! release calls these from production code. They stay for what comes
//! next on this line: the exec tool and MCP servers, both children Mezame
//! starts as their own session leaders and must reap whole on teardown.
//! `send_signal` and `reap_session` keep their integration tests under
//! `tests/`, so the teardown they will do is pinned before it has a
//! caller.
//!
//! The module is only compiled on Unix targets.

#![cfg(unix)]

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn setsid() -> i32;
    fn getsid(pid: i32) -> i32;
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
/// Meant for a `Command::pre_exec` hook, to make a child Mezame spawns its
/// own session and process-group leader, so that one `kill(-pid)` on
/// teardown reaches the child and everything it started. POSIX lists
/// `setsid` as async-signal-safe, which is the requirement a `pre_exec`
/// hook imposes.
///
/// # Safety
///
/// Must only be called between fork() and exec(); calling it in the
/// parent process would detach the parent from its controlling terminal.
/// `Command::pre_exec` enforces that constraint by contract.
pub unsafe fn new_session() -> i32 {
    setsid()
}

/// Session id of the current process (`getsid(0)`), or -1 on error.
///
/// `reap_session` uses this as a guard against sweeping our own session.
/// That sweep would SIGKILL Mezame itself.
///
/// SAFETY: `getsid` is a thin syscall wrapper that reads kernel state and
/// mutates nothing in the caller's address space.
fn own_session() -> i32 {
    unsafe { getsid(0) }
}

/// Parse the session id (field 6 of `/proc/<pid>/stat`) for `pid`.
///
/// Returns `None` if the entry is gone or unparseable. The `comm` field
/// (field 2) is wrapped in parentheses and may itself contain spaces and
/// parentheses. The split is on the LAST `)`, and the count starts there:
/// the tokens following it are `state ppid pgrp session ...`, putting the
/// session id 4th.
#[cfg(target_os = "linux")]
fn session_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(3)?.parse().ok()
}

/// SIGKILL every process belonging to session `sid`.
///
/// A child spawned as its own session leader, via [`new_session`] in a
/// `pre_exec` hook, has a session id equal to its pid, and a `kill(-pid)`
/// on its process group reaps it and the processes that stayed in that
/// group. Processes that put themselves in a process group of their own,
/// as an MCP server launched through `npx`/`npm` does, escape the group
/// kill and inherit only the leader's SESSION. Walking `/proc` for
/// processes whose session id matches and SIGKILLing them reaps those
/// escapees before they orphan to PID 1 and accumulate inside the service
/// cgroup.
///
/// Best-effort and defensive:
///   - A `sid` of 0 or 1, or one equal to our own session, is a no-op. A
///     test harness or a stray misparse can then never sweep unrelated
///     processes (or Mezame itself).
///   - pid <= 1 and our own pid are always skipped (in `sweep_session`).
///   - Unreadable or vanished `/proc` entries are silently ignored.
///
/// The walk is Linux-specific (procfs). On other Unix targets it degrades
/// to a no-op via `sweep_session`, and the process-group kill through
/// [`send_signal`] with a negative pid remains the primary teardown.
pub fn reap_session(sid: i32) {
    // Never sweep the kernel/init sessions or our own: the first two do
    // nothing useful and the third takes Mezame down with it.
    if sid <= 1 || sid == own_session() {
        return;
    }
    sweep_session(sid);
}

/// Walk `/proc` and SIGKILL every process whose session id equals `sid`.
/// Skips pid <= 1 and our own pid; unreadable or vanished entries are
/// ignored. The caller (`reap_session`) has already applied the sid
/// guards. Linux-only (procfs).
#[cfg(target_os = "linux")]
fn sweep_session(sid: i32) {
    let me = std::process::id() as i32;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        // Only numeric `/proc` entries are pids; skip `self`, `cpuinfo`, etc.
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 1 || pid == me {
            continue;
        }
        if session_of(pid) == Some(sid) {
            send_signal(pid, 9);
        }
    }
}

/// Non-Linux Unix targets have no `/proc` to walk and the sweep is a
/// no-op. The process-group kill through [`send_signal`] with a negative
/// pid remains the teardown there.
#[cfg(not(target_os = "linux"))]
fn sweep_session(_sid: i32) {}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn fork() -> i32;
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
        fn _exit(status: i32) -> !;
    }

    /// Exercise `new_session` in a forked child, the only context where
    /// calling `setsid` is safe: a freshly forked child is never a
    /// process-group leader. `setsid` succeeds there and the new session
    /// stays confined to the throwaway child. Calling it directly in the
    /// test process would detach the test runner from its controlling
    /// terminal, and the real production path (inside `Command::pre_exec`)
    /// runs between fork and exec where llvm-cov cannot observe it.
    ///
    /// The child leaves through `_exit`, never `std::process::exit`. The
    /// test binary is multithreaded: libtest runs each test on a thread
    /// of its own, and a thread starting or finishing at the instant of
    /// the fork may hold one of the runtime's locks. The child inherits
    /// that lock held by a thread it does not have, and `exit` runs the
    /// runtime's cleanup, which takes the lock and waits forever; that
    /// hung a `cargo test` run once. `_exit` skips the cleanup. The cost
    /// is the child's coverage profile, which is never written, so
    /// `new_session`'s body reads as uncovered; the production path,
    /// inside `Command::pre_exec`, was never observable there either.
    #[test]
    fn new_session_succeeds_in_a_forked_child() {
        unsafe {
            let pid = fork();
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                // Child: setsid must return a valid (non-negative) session
                // id. Map success to exit code 0, failure to 1.
                let sid = new_session();
                _exit(if sid >= 0 { 0 } else { 1 });
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
