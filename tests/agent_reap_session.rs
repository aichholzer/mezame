//! Regression test for `mezame::unix::reap_session`. Linux-only.
//!
//! Reproduces the production leak that the process-group kill alone could
//! not fix: an MCP server launched by the agent through `npx`/`npm` forks
//! into its OWN process group while keeping the agent's SESSION id. A
//! `kill(-pgid)` on the agent's process group therefore never reaches it,
//! so on the agent's death it reparents to PID 1 and survives, piling up
//! until the service cgroup is throttled.
//!
//! We model that exactly: a session leader (its own direct child) spawns a
//! grandchild that `setpgid`s into a fresh process group but stays in the
//! leader's session — the "escapee". We then show a group kill misses the
//! escapee, and that `reap_session` (called by `Agent::shutdown` after the
//! group kill) does reap it.
//!
//! The whole reproduction is Linux-specific (procfs + the session walk),
//! so the file compiles to nothing on other targets, where `reap_session`
//! is a documented no-op.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use mezame::unix::{reap_session, send_signal};

extern "C" {
    fn fork() -> i32;
    fn setsid() -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn execvp(file: *const u8, argv: *const *const u8) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn getsid(pid: i32) -> i32;
}

/// Replace the current process with a long-lived `sleep`. Called only
/// between `fork` and `exec`, so it touches nothing but async-signal-safe
/// functions and stack-local byte literals (no allocation).
fn exec_sleep() -> ! {
    let prog = b"sleep\0";
    let arg0 = b"sleep\0";
    let arg1 = b"86400\0";
    let argv = [arg0.as_ptr(), arg1.as_ptr(), std::ptr::null()];
    unsafe {
        execvp(prog.as_ptr(), argv.as_ptr());
        // execvp only returns on failure; bail with a distinctive code.
        std::process::exit(127);
    }
}

/// First token after the last `)` of `/proc/<pid>/stat` is the process
/// state. Returns `None` if the entry is gone.
fn proc_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().next()?.chars().next()
}

/// True while `pid` exists and is not a zombie. A SIGKILLed process is at
/// worst a zombie (it can never run again), so treating zombies as dead
/// makes the assertions robust against subreaper environments where an
/// orphan's zombie may linger before being reaped — `kill(pid, 0)` would
/// still report such a zombie as "alive", procfs state does not.
fn running(pid: i32) -> bool {
    matches!(proc_state(pid), Some(state) if state != 'Z')
}

/// Poll until `pid` stops running (gone or zombie), or the deadline lapses.
fn wait_until_not_running(pid: i32, within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if !running(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    !running(pid)
}

/// Spawn a session leader and an "escapee" grandchild that shares the
/// leader's session but lives in its own process group. Returns
/// `(session_id, escapee_pid)`. Both processes are `exec`'d `sleep`s.
///
/// A fork inherits the parent's process group, so the escapee starts in
/// the leader's group and only moves to its own when it calls `setpgid`.
/// To make the reproduction deterministic (rather than racing the later
/// group kill), the leader does not report the escapee's pid until the
/// escapee has signalled — over a sync pipe — that its `setpgid` is done.
fn spawn_session_with_escapee() -> (i32, i32) {
    // report pipe: leader -> test, carries the escapee pid.
    let mut report = [0i32; 2];
    assert_eq!(unsafe { pipe(report.as_mut_ptr()) }, 0, "report pipe");
    let (report_rd, report_wr) = (report[0], report[1]);

    let leader = unsafe { fork() };
    assert!(leader >= 0, "fork leader");
    if leader == 0 {
        // Leader: become a session (and process-group) leader, so our pid
        // is the session id. Then fork the escapee.
        unsafe { close(report_rd) };
        if unsafe { setsid() } == -1 {
            std::process::exit(11);
        }
        // sync pipe: escapee -> leader, signals "setpgid done".
        let mut sync = [0i32; 2];
        if unsafe { pipe(sync.as_mut_ptr()) } != 0 {
            std::process::exit(14);
        }
        let (sync_rd, sync_wr) = (sync[0], sync[1]);
        let escapee = unsafe { fork() };
        if escapee < 0 {
            std::process::exit(12);
        }
        if escapee == 0 {
            // Escapee: move into a fresh process group (its own pid) while
            // staying in the leader's session — exactly what npm/node do.
            unsafe {
                close(report_wr);
                close(sync_rd);
            }
            if unsafe { setpgid(0, 0) } == -1 {
                std::process::exit(13);
            }
            // Signal the leader that setpgid is done, then exec. Once
            // setpgid has returned, the escapee is in its own group for
            // good, so the leader can safely report us now.
            let ready = [1u8];
            unsafe {
                write(sync_wr, ready.as_ptr(), 1);
                close(sync_wr);
            }
            exec_sleep();
        }
        // Leader: wait for the escapee's "setpgid done" signal before
        // reporting it, so the test never races the group kill.
        unsafe { close(sync_wr) };
        let mut ready = [0u8; 1];
        let n = unsafe { read(sync_rd, ready.as_mut_ptr(), 1) };
        unsafe { close(sync_rd) };
        if n != 1 {
            std::process::exit(15);
        }
        // Report the escapee pid to the parent as 4 raw bytes (no alloc),
        // then become a long-lived process ourselves.
        let bytes = escapee.to_ne_bytes();
        unsafe {
            write(report_wr, bytes.as_ptr(), bytes.len());
            close(report_wr);
        }
        exec_sleep();
    }

    // Parent (the test): the fork return value is the leader pid, which is
    // also the session id (setsid set sid == leader pid).
    unsafe { close(report_wr) };
    let mut buf = [0u8; 4];
    let n = unsafe { read(report_rd, buf.as_mut_ptr(), buf.len()) };
    unsafe { close(report_rd) };
    assert_eq!(n, 4, "read escapee pid");
    let escapee = i32::from_ne_bytes(buf);
    (leader, escapee)
}

#[test]
fn reap_session_kills_escapee_that_group_kill_misses() {
    let (sid, escapee) = spawn_session_with_escapee();
    assert!(running(sid), "session leader should be running");
    assert!(running(escapee), "escapee should be running");

    // The pre-existing teardown is `kill(-pgid, SIGKILL)` on the agent's
    // process group. The escapee forked into its own group, so the group
    // kill cannot reach it. Prove it: kill the leader's group, reap the
    // leader, then confirm the escapee is still running — the bug.
    assert_eq!(send_signal(-sid, 9), 0, "group kill of the leader's group");
    let mut status = 0i32;
    unsafe { waitpid(sid, &mut status as *mut i32, 0) };
    assert!(
        wait_until_not_running(sid, Duration::from_secs(2)),
        "leader should be gone after the group kill"
    );
    assert!(
        running(escapee),
        "regression guard: the group kill alone must miss the escapee"
    );

    // The fix: sweep the whole session, which `Agent::shutdown` now does
    // right after the group kill.
    reap_session(sid);

    assert!(
        wait_until_not_running(escapee, Duration::from_secs(5)),
        "reap_session must SIGKILL the escapee sharing the dead leader's session"
    );
}

#[test]
fn reap_session_spares_processes_in_other_sessions() {
    let (sid, escapee) = spawn_session_with_escapee();

    // A control process in the TEST's own session (a plain child that does
    // not call setsid), so it shares our session id, not `sid`.
    let control = unsafe { fork() };
    assert!(control >= 0, "fork control");
    if control == 0 {
        exec_sleep();
    }
    assert!(running(control), "control should be running");

    // Sweep the leader's session; the control is in a different session
    // and must be left untouched.
    reap_session(sid);
    assert!(
        running(control),
        "reap_session must not touch a process in another session"
    );

    // Cleanup: the control is our direct child, so we can reap it. The
    // leader/escapee were already swept by reap_session above.
    send_signal(control, 9);
    let mut status = 0i32;
    unsafe { waitpid(control, &mut status as *mut i32, 0) };
    send_signal(-sid, 9);
    unsafe { waitpid(sid, &mut status as *mut i32, 0) };
    let _ = wait_until_not_running(escapee, Duration::from_secs(5));
}

#[test]
fn reap_session_is_a_noop_for_guarded_session_ids() {
    // A live control child in the test's own session.
    let control = unsafe { fork() };
    assert!(control >= 0, "fork control");
    if control == 0 {
        exec_sleep();
    }
    assert!(running(control), "control should be running");

    // The kernel/init session ids are guarded and never swept.
    reap_session(0);
    reap_session(1);
    // Our own session is guarded too — sweeping it would SIGKILL the test
    // runner itself; surviving past this line proves the guard holds.
    let own = unsafe { getsid(0) };
    reap_session(own);

    assert!(
        running(control),
        "a guarded sweep must not kill live processes"
    );

    send_signal(control, 9);
    let mut status = 0i32;
    unsafe { waitpid(control, &mut status as *mut i32, 0) };
}
