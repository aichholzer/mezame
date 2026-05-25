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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::config::Config;

/// Handle on the ACP agent subprocess.
///
/// Thread-safety: all mutable state is behind `Mutex`/`Arc`, so the handle
/// can be cloned into spawned tasks (as `Arc<Agent>` in `handle_ws`).
pub(crate) struct Agent {
    /// Stdin to the child; serialised by a Mutex because prompt tasks may
    /// try to write concurrently.
    stdin: Mutex<ChildStdin>,
    /// Monotonic JSON-RPC id generator.
    next_id: AtomicI64,
    /// Map from in-flight request id to the oneshot waiting for its
    /// response. Shared with the reader task that populates responses.
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// Owned child. SIGKILL on drop (kill_on_drop) remains as a safety net,
    /// but `shutdown` tries a clean EOF+wait first so Kiro can release its
    /// per-session lockfile.
    child: Mutex<Child>,
    /// Process group ID (Unix only). The child is spawned in its own
    /// process group so `shutdown` can kill the entire tree (MCP servers,
    /// npm wrappers, etc.) rather than just the direct child.
    #[cfg(unix)]
    pgid: i32,
}

impl Agent {
    /// Send a JSON-RPC request and await its response.
    ///
    /// Returns the `result` value on success, or an error if the agent
    /// responded with `error`, closed before replying, or the stdin write
    /// failed. The caller is responsible for cancellation semantics — if
    /// the future is dropped mid-flight, the response will arrive at a
    /// dangling oneshot and be discarded.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        );
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        let resp = rx.await.context("Agent closed before replying")?;
        if let Some(err) = resp.get("error") {
            bail!("Agent error: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Reply to a server-initiated request (e.g. `session/request_permission`).
    pub(crate) async fn respond(&self, id: Value, result: Value) -> Result<()> {
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": id, "result": result })
        );
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Send a JSON-RPC notification (no id, no response expected). Used for
    /// one-way signals like `session/cancel`.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "method": method, "params": params })
        );
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Cooperative shutdown:
    ///   1. Best-effort `session/cancel` so any in-flight tool or turn stops.
    ///   2. Close stdin so the agent sees EOF and exits cleanly. Kiro uses
    ///      this signal to release its per-session PID lockfile; without it
    ///      you get "Session is active in another process (PID ...)" errors
    ///      on the next `session/load`.
    ///   3. Wait up to 500ms for the child to exit.
    ///   4. Kill the entire process group. This is unconditional and
    ///      idempotent: if the agent and its children already exited, the
    ///      kill is a no-op. If `kiro-cli` exited cleanly but left its MCP
    ///      server grandchildren alive (the common case), this reaps them.
    pub(crate) async fn shutdown(&self, session_id: Option<&str>) {
        if let Some(sid) = session_id {
            let _ = self
                .notify("session/cancel", json!({ "sessionId": sid }))
                .await;
        }
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.shutdown().await;
        }
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let mut child = self.child.lock().await;
            let _ = child.wait().await;
        })
        .await;

        // Always kill the group. Idempotent: a no-op if everything already
        // exited, otherwise reaps any orphaned MCP servers / npm wrappers
        // that kiro-cli did not clean up.
        self.kill_process_group();
    }

    /// Send SIGKILL to the entire process group rooted at the child.
    #[cfg(unix)]
    fn kill_process_group(&self) {
        if self.pgid > 0 {
            // kill(-pgid, SIGKILL) sends to every process in the group.
            unsafe {
                libc_kill(-self.pgid, 9);
            }
        }
    }

    #[cfg(not(unix))]
    fn kill_process_group(&self) {
        // Non-unix: fall through to kill_on_drop for the direct child.
    }
}

/// Safety net: if the Agent is dropped without a prior `shutdown()` call
/// (e.g. a panic unwind or early return), kill the entire process group
/// so grandchildren do not leak. `kill_on_drop(true)` on the Child only
/// kills the direct child; this covers the rest of the tree.
#[cfg(unix)]
impl Drop for Agent {
    fn drop(&mut self) {
        if self.pgid > 0 {
            unsafe {
                libc_kill(-self.pgid, 9);
            }
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
/// - The child is spawned in its own process group via `setsid()` so the
///   entire descendant tree (MCP servers, npm wrappers, bun/node) can be
///   killed as a unit rather than only the direct child.
/// - `kill_on_drop(true)` provides a tokio-level safety net for the direct
///   child; the `Drop` impl on `Agent` covers the rest of the group.
/// - Cooperative shutdown is preferred: `shutdown()` closes stdin and
///   waits briefly so Kiro can release its session lockfile.
///
/// Two background tasks are spawned here:
///   1. Stderr forwarder, writes each line to our stderr prefixed with
///      `[agent]`, for debugging.
///   2. Stdout reader, newline-delimited JSON decoder that routes
///      responses to their pending oneshots and everything else to the
///      returned mpsc receiver.
pub(crate) async fn spawn_agent(cfg: &Config) -> Result<(Agent, mpsc::UnboundedReceiver<Value>)> {
    let mut cmd = Command::new(&cfg.agent_cmd);
    cmd.args(&cfg.agent_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Spawn the child in its own process group so we can kill the entire
    // tree (MCP servers, npm wrappers, bun, node, etc.) on shutdown rather
    // than just the direct child. Without this, grandchildren survive as
    // orphans inside the systemd cgroup and accumulate memory.
    //
    // SAFETY: `pre_exec` runs after fork() but before exec(), in a context
    // where only async-signal-safe functions may be called. `setsid` is
    // listed as async-signal-safe by POSIX.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            // setsid() creates a new session (and process group), making
            // the child its own group leader. Bail loudly if it fails so
            // we never end up with a wrong pgid that could target the
            // parent's group on shutdown.
            if libc_setsid() == -1 {
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

    let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();

    // Optional ACP tracing. Set `MEZAME_DEBUG_ACP=1` to dump every inbound
    // line from the agent to Mezame's stderr. Helpful when wiring new
    // Kiro extensions (`_kiro.dev/*`) or debugging wire-shape mismatches.
    let debug_acp = std::env::var_os("MEZAME_DEBUG_ACP").is_some();

    // Stdout reader: route responses vs notifications.
    //
    // A response is any message carrying `result` or `error` whose `id`
    // matches a pending request we sent. Everything else — notifications
    // (no id) and server-initiated requests (id but no result/error) — is
    // pushed onto the updates channel for the WS handler to act on.
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
                    if let Some(tx) = pending_reader.lock().await.remove(&id) {
                        let _ = tx.send(msg);
                        continue;
                    }
                }
            }
            let _ = updates_tx.send(msg);
        }
    });

    Ok((
        Agent {
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending,
            child: Mutex::new(child),
            #[cfg(unix)]
            pgid,
        },
        updates_rx,
    ))
}

// Minimal FFI bindings to avoid pulling in `libc` for two calls.
#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
}
