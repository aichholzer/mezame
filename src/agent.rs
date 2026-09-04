//! ACP agent subprocess wrapper and JSON-RPC framing.
//!
//! One `Agent` wraps one spawned child process and its JSON-RPC stdio.
//! The stdout reader task splits incoming traffic into two streams:
//!   - Responses (messages with `result` or `error` and a known `id`)
//!     go to the matching oneshot sender registered by `request`.
//!   - Notifications and server-initiated requests go out through the
//!     `updates_rx` mpsc channel, which the WS handler drains.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::config::Config;

/// Type-erased writer the JSON-RPC framing helpers send into. In
/// production this wraps `ChildStdin`; tests pass in a `tokio::io::duplex`
/// half so the loop can be exercised without a real subprocess.
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// In-flight request table, shared between the handle and its stdout
/// reader task.
///
/// `None` means the agent's stdout has reached EOF and no further response
/// can arrive. The reader takes the map on EOF, which drops every sender
/// still in it and wakes each waiter with a `RecvError`, and a request
/// arriving afterwards finds `None` and fails on the spot.
///
/// The closed marker lives inside the same lock as the map on purpose.
/// Holding them apart would let a request read "open", lose the race to
/// the reader's cleanup, and then insert into a table nobody will ever
/// drain again. See issue #9.
type Pending = Arc<Mutex<Option<HashMap<i64, oneshot::Sender<Value>>>>>;

/// Ceiling on a single wait for the child to exit during `shutdown`.
/// Applied twice: once for the cooperative stdin-EOF exit, and once after
/// the kills to collect the pid.
const CHILD_WAIT: Duration = Duration::from_millis(500);

/// Handle on the ACP agent.
///
/// In production the handle drives a spawned subprocess and holds a
/// process-group id (`pgid > 0`). The `Child` itself belongs to the reaper
/// task; see `exited`. Tests build the same shape from in-memory streams
/// via `Agent::from_io`, where `exited` is `None` and `pgid` is 0, and
/// `shutdown` is then a stdin EOF with no process-management side effects.
///
/// Thread-safety: all mutable state sits behind `Mutex`/`Arc`. The handle
/// clones into spawned tasks as `Arc<Agent>`; see `handle_ws`.
pub struct Agent {
    /// Stdin to the child; serialised by a Mutex because prompt tasks may
    /// try to write concurrently.
    stdin: Mutex<Writer>,
    /// Monotonic JSON-RPC id generator.
    next_id: AtomicI64,
    /// Map from in-flight request id to the oneshot waiting for its
    /// response. Shared with the stdout reader and the reaper task, either
    /// of which empties it. See `Pending`.
    pending: Pending,
    /// Flips to `true` once the reaper task has collected the child.
    /// `None` for test-built agents, which own no process.
    ///
    /// The `Child` lives in that task, which parks on `child.wait()` for
    /// the life of the agent. The handle keeps none of its own. A dead
    /// process is then noticed and collected the moment it exits, with no
    /// dependence on its stdout reaching EOF. `shutdown` observes exit
    /// through this channel.
    exited: Option<watch::Receiver<bool>>,
    /// Dropped with the handle, which tells the reaper task to kill the
    /// child and collect it. The channel is never sent on; the drop is the
    /// whole signal, and it keeps teardown deterministic on platforms with
    /// no process-group kill to fall back on.
    _kill_on_drop: Option<oneshot::Sender<()>>,
    /// Process group ID (Unix only). The child is spawned in its own
    /// process group. `shutdown` kills the whole tree through it: MCP
    /// servers, npm wrappers, everything below the direct child. 0 when
    /// there is no real subprocess (tests).
    #[cfg(unix)]
    pgid: i32,
    /// Session ID (Unix only). Equal to `pgid` at spawn: `setsid` makes
    /// the child both a session and a process-group leader. MCP
    /// servers spawned via `npx`/`npm` inherit the session and fork into
    /// their own process groups. The session sweep in `shutdown` uses this
    /// to reap the escapees a `kill(-pgid)` cannot reach. 0 when there is
    /// no real subprocess (tests).
    #[cfg(unix)]
    sid: i32,
    /// Set to true once `shutdown()` has finished. Tests read this to
    /// confirm cooperative shutdown ran. Not serialised; relaxed
    /// ordering is fine for the read-after-write that tests perform.
    shutdown_done: Arc<AtomicBool>,
}

impl Agent {
    /// Write a single JSON-RPC message to the agent's stdin, terminated by
    /// newline and flushed. The agent reads newline-delimited JSON. The
    /// trailing `\n` is part of the wire framing.
    async fn write_message(&self, msg: Value) -> Result<()> {
        let line = format!("{msg}\n");
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Send a JSON-RPC request and await its response.
    ///
    /// Returns the `result` value on success, or an error if the agent
    /// responded with `error`, closed before replying, or the stdin write
    /// failed. Cancellation semantics are the caller's: drop the future
    /// mid-flight and the response arrives at a dangling oneshot and is
    /// discarded.
    ///
    /// An agent that dies with this request outstanding resolves it as an
    /// error, from the reader task emptying `pending` on EOF. This call
    /// terminates for every outcome the agent can produce, including
    /// dying without answering. Issue #9 is what happens when it does
    /// not: the future is held forever, and with it the `Arc<Agent>` and
    /// an unreaped child process.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            let Some(outstanding) = pending.as_mut() else {
                bail!("Agent closed before replying");
            };
            outstanding.insert(id, tx);
        }

        self.write_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        let resp = rx.await.context("Agent closed before replying")?;
        if let Some(err) = resp.get("error") {
            bail!("Agent error: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Reply to a server-initiated request (e.g. `session/request_permission`).
    pub async fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.write_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .await
    }

    /// Send a JSON-RPC notification (no id, no response expected). Used for
    /// one-way signals like `session/cancel`.
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    /// Cooperative shutdown:
    ///   1. Best-effort `session/cancel` to stop any in-flight tool or turn.
    ///   2. Close stdin. The agent sees EOF and exits cleanly. Kiro uses
    ///      this signal to release its per-session PID lockfile; without it
    ///      you get "Session is active in another process (PID ...)" errors
    ///      on the next `session/load`.
    ///   3. Wait up to 500ms for the child to exit.
    ///   4. Kill the entire process group. This is unconditional and
    ///      idempotent: if the agent and its children already exited, the
    ///      kill is a no-op. If `kiro-cli` exited cleanly but left its MCP
    ///      server grandchildren alive (the common case), this reaps them.
    ///   5. Sweep the agent's whole session. MCP servers launched through
    ///      `npx`/`npm` put themselves in their own process groups and the
    ///      group kill in step 4 never reaches them; they only inherit the
    ///      agent's session. Without this they orphan to PID 1 and pile up
    ///      until the service cgroup is throttled. See `unix::reap_session`.
    ///   6. Wait again for the reaper to confirm the pid is collected.
    ///      Steps 4 and 5 are what ended the child whenever the wait in
    ///      step 3 timed out. The reaper collects it either way, and this
    ///      makes `shutdown` return with that already done. See issue #9.
    pub async fn shutdown(&self, session_id: Option<&str>) {
        if let Some(sid) = session_id {
            let _ = self
                .notify("session/cancel", json!({ "sessionId": sid }))
                .await;
        }
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.shutdown().await;
        }
        self.wait_briefly().await;

        // Always kill the group. Idempotent: a no-op if everything already
        // exited, otherwise reaps any orphaned MCP servers / npm wrappers
        // that kiro-cli did not clean up.
        self.kill_process_group();
        // The group kill misses MCP servers that forked into their own
        // process groups. The session sweep catches those escapees.
        self.reap_session();
        // Collect the direct child now that it has certainly been killed.
        self.wait_briefly().await;
        self.shutdown_done.store(true, Ordering::Relaxed);
    }

    /// Wait up to `CHILD_WAIT` for the reaper task to report the child
    /// collected. A child that ignores stdin EOF cannot stall teardown past
    /// the bound. No-op for test-built agents, which own no process.
    async fn wait_briefly(&self) {
        let Some(exited) = self.exited.as_ref() else {
            return;
        };
        let mut rx = exited.clone();
        let _ = tokio::time::timeout(CHILD_WAIT, async move {
            loop {
                // The borrow is released before the await below.
                let collected = *rx.borrow_and_update();
                if collected {
                    return;
                }
                // Err means the reaper task is gone without reporting.
                // No amount of further waiting changes that.
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
    }

    /// True once `shutdown()` has run to completion. Test-only signal so
    /// integration suites can assert cooperative shutdown happened
    /// without polling for process state.
    #[doc(hidden)]
    pub fn shutdown_complete(&self) -> bool {
        self.shutdown_done.load(Ordering::Relaxed)
    }

    /// Send SIGKILL to the entire process group rooted at the child.
    #[cfg(unix)]
    fn kill_process_group(&self) {
        if self.pgid > 0 {
            // kill(-pgid, SIGKILL) sends to every process in the group.
            crate::unix::send_signal(-self.pgid, 9);
        }
    }

    #[cfg(not(unix))]
    fn kill_process_group(&self) {
        // Non-unix: fall through to kill_on_drop for the direct child.
    }

    /// SIGKILL every process still in the agent's session. Catches MCP
    /// servers that forked into their own process groups and escaped the
    /// `kill(-pgid)` group kill. No-op for test-built agents (`sid == 0`)
    /// and guarded against sweeping our own session.
    #[cfg(unix)]
    fn reap_session(&self) {
        if self.sid > 0 {
            crate::unix::reap_session(self.sid);
        }
    }

    #[cfg(not(unix))]
    fn reap_session(&self) {
        // Non-unix: no procfs to walk; the group kill is the only teardown.
    }
}

/// Safety net: if the Agent is dropped without a prior `shutdown()` call
/// (a panic unwind or an early return), the entire process group is killed
/// and no grandchildren leak. Dropping the handle also closes the reaper's
/// kill channel, which deals with the direct child; this covers the rest of
/// the tree. No-op for test-built agents, where `pgid == 0`.
///
/// When `shutdown()` already ran, the group kill and session sweep are
/// done. This repeats only the cheap group kill and skips the heavier
/// `/proc` walk. On the early-return or panic path `shutdown()` never ran,
/// and the session sweep runs here to catch MCP servers that escaped the
/// group via their own process groups.
#[cfg(unix)]
impl Drop for Agent {
    fn drop(&mut self) {
        if self.pgid > 0 {
            crate::unix::send_signal(-self.pgid, 9);
        }
        if !self.shutdown_done.load(Ordering::Relaxed) {
            self.reap_session();
        }
    }
}

/// Spawn the configured agent and wire its stdio into the `Agent` handle.
///
/// Returns the handle plus the receiver end of the agent-updates channel.
/// The receiver is owned by the caller (the WS select loop) for the life
/// of the session.
///
/// Process lifecycle:
/// - The child is spawned in its own process group via `setsid()`. The
///   entire descendant tree (MCP servers, npm wrappers, bun/node) is then
///   killable as a unit, down from the direct child.
/// - The reaper task owns the `Child` and collects it the moment it exits.
///   Dropping the `Agent` closes that task's kill channel, which kills the
///   direct child on any platform. `kill_on_drop(true)` stays as a last
///   resort for a runtime teardown that drops the task itself, and the
///   `Drop` impl on `Agent` covers the rest of the group.
/// - Cooperative shutdown is preferred: `shutdown()` closes stdin and
///   waits briefly, leaving Kiro room to release its session lockfile.
///
/// Three background tasks are spawned here:
///   1. Reaper, parks on `child.wait()` and empties the pending request
///      table once the process is gone.
///   2. Stderr forwarder, writes each line to our stderr prefixed with
///      `[agent]`, for debugging.
///   3. Stdout reader, newline-delimited JSON decoder that routes
///      responses to their pending oneshots and everything else to the
///      returned mpsc receiver.
pub async fn spawn_agent(cfg: &Config) -> Result<(Agent, mpsc::UnboundedReceiver<Value>)> {
    let mut cmd = Command::new(&cfg.agent_cmd);
    cmd.args(&cfg.agent_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Spawn the child in its own process group. Shutdown then kills the
    // entire tree through it: MCP servers, npm wrappers, bun, node.
    // Without this, grandchildren survive as orphans inside the systemd
    // cgroup and accumulate memory.
    //
    // SAFETY: `pre_exec` runs after fork() but before exec(), in a context
    // where only async-signal-safe functions may be called. `setsid` is
    // listed as async-signal-safe by POSIX.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            // setsid() creates a new session and process group, making
            // the child its own group leader. A failure bails loudly. A
            // silent one would leave a wrong pgid behind, and shutdown
            // would aim its group kill at the parent's group.
            if crate::unix::new_session() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn `{}`", cfg.agent_cmd))?;

    #[cfg(unix)]
    let pgid = child.id().map(|id| id as i32).unwrap_or(0);
    // `setsid` in pre_exec makes the child a session leader as well, and
    // the session id equals the child pid (== pgid). Tracked separately:
    // MCP servers inherit the session, and the group is theirs alone.
    #[cfg(unix)]
    let sid = pgid;

    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    // Stderr forwarder.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[agent] {line}");
        }
    });

    let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();

    // Reaper. Owns the `Child` and parks on `wait()` for the life of the
    // agent. That call collects the pid the instant the process exits.
    //
    // This is the trigger the stdout reader cannot be: a grandchild that
    // inherited the agent's stdout holds that pipe open after the agent
    // itself is gone, and the reader then sits on a stream that never
    // reaches EOF. MCP servers launched through `npx`/`npm` are exactly
    // that shape. Process exit is the fact worth keying on, and this is
    // where it is observed.
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let (exited_tx, exited_rx) = watch::channel(false);
    let pending_reaper = pending.clone();
    tokio::spawn(async move {
        // Two ways out: the child exits on its own, or the `Agent` is
        // dropped and takes `kill_tx` with it. `kill_rx` resolves either
        // way, and the drop is the signal; nothing is ever sent.
        let handle_dropped = {
            let wait = child.wait();
            tokio::pin!(wait);
            tokio::select! {
                _ = &mut wait => false,
                _ = kill_rx => true,
            }
        };
        if handle_dropped {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        // The process is gone and collected. Nothing outstanding can be
        // answered now, whatever state its stdout is in. `take` on an
        // already-empty table is a no-op. Racing the stdout reader here is
        // harmless. See issue #9.
        let outstanding = pending_reaper.lock().await.take();
        drop(outstanding);
        let _ = exited_tx.send(true);
    });

    // Optional ACP tracing. Set `MEZAME_DEBUG_ACP=1` to dump every inbound
    // line from the agent to Mezame's stderr. Helpful when wiring new
    // Kiro extensions (`_kiro.dev/*`) or debugging wire-shape mismatches.
    let debug_acp = std::env::var_os("MEZAME_DEBUG_ACP").is_some();

    // Stdout reader: route responses vs notifications.
    //
    // A response is any message holding `result` or `error` whose `id`
    // matches a pending request we sent. Everything else is pushed onto
    // the updates channel for the WS handler to act on: notifications with
    // no id, and server-initiated requests with an id but no
    // result or error.
    let pending_reader = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if debug_acp {
                eprintln!("[acp<-] {line}");
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue, // malformed line; skip silently
            };
            let is_response = msg.get("result").is_some() || msg.get("error").is_some();
            if is_response {
                if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                    let waiting = pending_reader
                        .lock()
                        .await
                        .as_mut()
                        .and_then(|outstanding| outstanding.remove(&id));
                    if let Some(tx) = waiting {
                        let _ = tx.send(msg);
                        continue;
                    }
                }
            }
            let _ = updates_tx.send(msg);
        }

        // Stdout is closed: the agent exited or its stdio broke, and no
        // response can arrive from here on. Take the map, which marks the
        // handle closed and drops every sender still waiting. Each caller
        // blocked in `request` wakes with a `RecvError`.
        //
        // Without this a request outstanding at the moment the agent died
        // waits for a reply that cannot come. That future holds the
        // `Arc<Agent>`, the `Agent` holds the `Child`, and tokio reaps a
        // `Child` only on drop. The pid then sits `<defunct>` for the life
        // of the process. See issue #9.
        let outstanding = pending_reader.lock().await.take();
        // Dropped outside the lock: waking a waiter takes no part in it.
        drop(outstanding);
    });

    Ok((
        Agent {
            stdin: Mutex::new(Box::new(stdin)),
            next_id: AtomicI64::new(1),
            pending,
            exited: Some(exited_rx),
            _kill_on_drop: Some(kill_tx),
            #[cfg(unix)]
            pgid,
            #[cfg(unix)]
            sid,
            shutdown_done: Arc::new(AtomicBool::new(false)),
        },
        updates_rx,
    ))
}

/// Build an `Agent` from in-memory streams. Test-only escape hatch:
/// production code should always go through `spawn_agent`. The returned
/// agent has no child process. `shutdown` then closes stdin and flips the
/// `shutdown_complete()` flag, and does nothing else.
///
/// The `stdout` reader runs the same routing logic as the production path,
/// including emptying the pending table when the stream ends. Response
/// correlation and closed-agent behaviour both work here as they do in
/// production.
#[doc(hidden)]
pub fn from_io(
    stdin: impl AsyncWrite + Send + Unpin + 'static,
    stdout: impl AsyncRead + Send + Unpin + 'static,
) -> (Agent, mpsc::UnboundedReceiver<Value>) {
    let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();

    let pending_reader = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let is_response = msg.get("result").is_some() || msg.get("error").is_some();
            if is_response {
                if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                    let waiting = pending_reader
                        .lock()
                        .await
                        .as_mut()
                        .and_then(|outstanding| outstanding.remove(&id));
                    if let Some(tx) = waiting {
                        let _ = tx.send(msg);
                        continue;
                    }
                }
            }
            let _ = updates_tx.send(msg);
        }

        // Matches the production reader: end of stream closes the handle
        // and fails everything outstanding. See issue #9.
        let outstanding = pending_reader.lock().await.take();
        drop(outstanding);
    });

    (
        Agent {
            stdin: Mutex::new(Box::new(stdin)),
            next_id: AtomicI64::new(1),
            pending,
            exited: None,
            _kill_on_drop: None,
            #[cfg(unix)]
            pgid: 0,
            #[cfg(unix)]
            sid: 0,
            shutdown_done: Arc::new(AtomicBool::new(false)),
        },
        updates_rx,
    )
}
