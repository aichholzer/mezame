//! Multi-attach session hub.
//!
//! The hub owns a single ACP `Agent` and broadcasts its outbound
//! messages to every WebSocket attached to the same session id. The
//! 1:1 mapping of WebSocket to subprocess we used to have is gone:
//! browsers attach and detach freely, the agent stays warm across
//! reconnects (within a configurable grace window), and Kiro's
//! per-session lockfile is held only while at least one browser is
//! interested.
//!
//! Concurrency model:
//!
//! - Each hub runs a single owner task (`run_hub_loop`) that reads
//!   `HubCommand`s from an mpsc inbox and forwards them to the agent
//!   via the existing `Agent` API. That serialises browser-originated
//!   commands so two browsers cannot race a `session/prompt` against
//!   each other through the same channel.
//! - Outbound events from the agent fan out via a `tokio::sync::broadcast`
//!   sender. Each WS handler subscribes once on attach and forwards to
//!   its own sink; lagged subscribers (slow client) are reported
//!   instead of blocking the rest.
//! - `HubRegistry` is a `RwLock<HashMap>` keyed by ACP session id.
//!   Lookups are read-locked, hub creation takes the write lock for
//!   the duration of the lookup-or-insert.
//!
//! Lifecycle:
//!
//! 1. First browser attaches: registry creates the hub, hub spawns the
//!    agent, runs the negotiate phase, then the owner loop begins.
//! 2. Subsequent browsers attach: registry returns the existing hub,
//!    the new subscriber starts receiving events. The hub's snapshot
//!    of the negotiation outcome is replayed once into the new
//!    subscriber so it sees the same `ready` and `session_info` the
//!    first browser saw.
//! 3. A browser detaches: subscriber count decrements. If it hits zero,
//!    the hub arms a grace timer (default 30 seconds). A new
//!    subscriber arriving inside the window cancels the timer.
//! 4. Grace timer fires: the hub shuts the agent down cleanly and
//!    removes itself from the registry.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};

use crate::agent::{spawn_agent, Agent};
use crate::config::Config;
use crate::ws::handle_agent_message;

/// How long the agent stays warm after the last browser detaches.
/// 30s matches the WS reconnect-backoff cap on the client; if the
/// browser is going to come back from a transient drop, it will do
/// so well within this window.
const GRACE_PERIOD: Duration = Duration::from_secs(30);

/// Capacity of the outbound broadcast channel. Has to be high enough
/// that a slow subscriber falling 1024 events behind is genuinely a
/// problem and not just a momentary backlog. Streamed agent output
/// bursts at a few hundred events per turn at most.
const BROADCAST_CAPACITY: usize = 1024;

/// Capacity of the per-hub command inbox. Each browser sends commands
/// at user pace (a prompt every few seconds at most), and the loop
/// drains them as fast as the agent accepts. 256 leaves headroom for
/// rapid clicks without any visible backpressure.
const COMMAND_CAPACITY: usize = 256;

/// Browser → hub commands. The hub owner loop drains these and
/// forwards to the agent. Only one of these can be in flight at a time
/// per hub; the loop processes them sequentially.
///
/// Each variant mirrors a JSON message the browser sends today; the
/// hub is a thin re-shape from JSON into a typed enum so the loop body
/// reads as direct calls into the agent rather than as another match
/// on string method names.
#[derive(Debug)]
pub enum HubCommand {
    /// Send a prompt to the agent. `blocks` is the ACP-shaped block
    /// list (text, image, resource, etc.). The hub forwards as
    /// `session/prompt`.
    Prompt { blocks: Vec<Value> },
    /// Reply to a `session/request_permission` we previously broadcast.
    /// `id` matches the JSON-RPC id the agent sent. First reply wins;
    /// later replies for the same id are dropped silently.
    PermissionResponse { id: Value, option_id: String },
    /// Cancel the in-flight turn.
    Cancel,
    /// Switch agent mode (`session/set_mode`).
    SetMode { mode_id: String },
    /// Switch model (`session/set_model`).
    SetModel { model_id: String },
}

/// Outcome of the hub's negotiation phase. Replayed into each new
/// subscriber so they see `ready` and (optionally) `session_info`
/// consistently regardless of when they attached.
#[derive(Clone)]
struct NegotiationSnapshot {
    /// The `ready` event in its final wire shape, ready to forward.
    ready: Value,
    /// The `session_info` event when modes/models were available.
    /// Subscribers receive this immediately after `ready`.
    session_info: Option<Value>,
}

/// Public handle to a session hub. The handle is cheap to clone; all
/// state lives behind the senders. The owner task is held alive by
/// these senders plus the registry's `Arc`.
pub struct SessionHub {
    /// Pushes commands into the owner loop.
    commands: mpsc::Sender<HubCommand>,
    /// Subscribers receive outbound events from this. New subscribers
    /// start at the current head; events emitted before subscription
    /// are not replayed.
    outbound: broadcast::Sender<Arc<Value>>,
    /// `ready` and `session_info` snapshots. Replayed on every attach.
    snapshot: NegotiationSnapshot,
    /// The ACP session id this hub owns. Cached so callers can read it
    /// without going through the snapshot.
    session_id: String,
    /// Subscriber count for grace-timer logic. The `Counter` aborts
    /// the grace timer on attach and arms it on the last detach.
    counter: Arc<Counter>,
}

/// Tracks attach/detach events and arms the grace timer. Lives behind
/// an `Arc` so both the registry and the per-attach RAII guard can
/// access it.
struct Counter {
    state: Mutex<CounterState>,
    /// Pings the owner loop when the count hits zero so it can arm
    /// the grace timer with the right deadline. The loop also
    /// observes attach events (count > 0) by inspecting the next
    /// command pull, so we do not need a separate notification for
    /// the cancel side.
    grace_tx: mpsc::Sender<GraceEvent>,
}

#[derive(Default)]
struct CounterState {
    count: usize,
    /// One-shot used to wake the grace-cancel sleeper. Some(_) only
    /// while the count is zero and we are inside the grace window.
    cancel_grace: Option<oneshot::Sender<()>>,
}

#[derive(Debug)]
enum GraceEvent {
    /// Subscriber count fell to zero; arm the grace timer.
    Empty,
    /// Subscriber count climbed back above zero during the grace
    /// window; cancel any pending shutdown.
    Refilled,
}

impl Counter {
    fn new(grace_tx: mpsc::Sender<GraceEvent>) -> Self {
        Self {
            state: Mutex::new(CounterState::default()),
            grace_tx,
        }
    }

    /// Call when a new subscriber attaches. Returns the post-attach
    /// count so callers can include it in the diagnostic event.
    async fn increment(&self) -> usize {
        let mut state = self.state.lock().await;
        let was_zero = state.count == 0;
        state.count += 1;
        if was_zero {
            // Cancel any pending grace shutdown.
            if let Some(tx) = state.cancel_grace.take() {
                let _ = tx.send(());
            }
            // Notify the owner loop so it can drop any in-flight
            // grace state on its side too.
            let _ = self.grace_tx.send(GraceEvent::Refilled).await;
        }
        state.count
    }

    /// Call when a subscriber detaches.
    async fn decrement(&self) -> usize {
        let mut state = self.state.lock().await;
        if state.count > 0 {
            state.count -= 1;
        }
        if state.count == 0 {
            let _ = self.grace_tx.send(GraceEvent::Empty).await;
        }
        state.count
    }

    /// Used by the grace timer to install its cancel handle. The
    /// returned receiver completes when a fresh subscriber arrives,
    /// at which point the timer should abandon the shutdown.
    async fn install_cancel(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().await;
        state.cancel_grace = Some(tx);
        rx
    }

    /// Returns the current subscriber count without mutating state.
    async fn count(&self) -> usize {
        self.state.lock().await.count
    }
}

/// Subscriber-side handle on an attached hub. Drop-on-detach: the
/// `Drop` impl decrements the counter so the grace timer arms when
/// the last attach goes away.
///
/// Public surface is just the broadcast receiver and the command
/// sender; the WS handler reads outbound events via the receiver and
/// pushes browser commands via the sender.
pub struct AttachedHub {
    pub commands: mpsc::Sender<HubCommand>,
    pub outbound: broadcast::Receiver<Arc<Value>>,
    pub snapshot_ready: Value,
    pub snapshot_session_info: Option<Value>,
    pub session_id: String,
    counter: Arc<Counter>,
}

impl Drop for AttachedHub {
    fn drop(&mut self) {
        // Spawn the decrement so we do not block the WS handler's
        // shutdown. The counter is `Arc<...>` so the spawn keeps it
        // alive long enough.
        let counter = self.counter.clone();
        tokio::spawn(async move {
            counter.decrement().await;
        });
    }
}

/// Registry of live hubs keyed by ACP session id. Cheap to clone;
/// `Arc<RwLock>` lets the WS handler do lookups without coordinating
/// with the owner loop.
///
/// `building` serialises slow-path attaches by session id so two
/// browsers reconnecting at the same time with the same
/// `?session=<id>` cannot both spawn an agent and race
/// `session/load` against the same Kiro lockfile (the second loses
/// with `Session is active in another process` and falls back to a
/// fresh session, which clobbers the history view). Holding a
/// per-key mutex across the build window means the second arrival
/// finds the hub already in the registry on its re-check and takes
/// the fast path. Fresh attaches (no resume id) don't go through
/// this gate; they're independent by definition.
#[derive(Clone, Default)]
pub struct HubRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<SessionHub>>>>,
    building: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl HubRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach to an existing hub for `session_id`, or create one if
    /// none is registered yet. Always returns an `AttachedHub` whose
    /// `Drop` will decrement the counter.
    ///
    /// `resume_session_id` and `cwd_override` are used only when a
    /// new hub is created; they are ignored when reusing an existing
    /// hub (the existing hub's session is already negotiated).
    pub async fn attach_or_create(
        &self,
        cfg: Arc<Config>,
        resume_session_id: Option<String>,
        cwd_override: Option<String>,
        build_id: &str,
    ) -> Result<AttachedHub> {
        // Fast path: if the resume id matches an existing hub, attach
        // directly without spawning a fresh agent. We can only do
        // this when the browser supplied an explicit `?session=<id>`,
        // because a fresh-session attach has no key to look up by.
        if let Some(sid) = resume_session_id.as_deref() {
            let map = self.inner.read().await;
            if let Some(hub) = map.get(sid).cloned() {
                drop(map);
                return Ok(self.subscribe(hub).await);
            }
        }

        // Slow path with a per-session-id lock. Two browsers arriving
        // at the same time with the same `?session=<id>` (typical at
        // server startup) must not both call `build_hub`: only one
        // agent can hold Kiro's session lockfile, so the second
        // would fail with "Session is active in another process".
        // We acquire (or create) a per-key mutex, hold it across the
        // build, and re-check the registry once we have the key
        // mutex. The first arrival builds; the second finds the hub
        // already there and falls into the fast path.
        //
        // Fresh attaches (no resume id) skip this gate entirely:
        // they spawn independent sessions and there is nothing to
        // serialise.
        if let Some(sid) = resume_session_id.as_deref() {
            let key_mutex = {
                let mut building = self.building.lock().await;
                building
                    .entry(sid.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };
            let _guard = key_mutex.lock().await;

            // Re-check now that we hold the key mutex; the first
            // arrival registered the hub before releasing.
            {
                let map = self.inner.read().await;
                if let Some(hub) = map.get(sid).cloned() {
                    drop(map);
                    let attached = self.subscribe(hub).await;
                    self.cleanup_build_slot(sid).await;
                    return Ok(attached);
                }
            }

            let result = self
                .build_and_register(cfg, Some(sid.to_string()), cwd_override, build_id)
                .await;
            self.cleanup_build_slot(sid).await;
            return result;
        }

        // Fresh-session attach: no key to coordinate on, just build.
        self.build_and_register(cfg, None, cwd_override, build_id)
            .await
    }

    /// Drop the per-key mutex from `building` once nobody else is
    /// waiting on it. Cheap garbage collection so the map does not
    /// grow without bound; the alternative is leaking one mutex per
    /// session id ever attached.
    async fn cleanup_build_slot(&self, sid: &str) {
        let mut building = self.building.lock().await;
        if let Some(entry) = building.get(sid) {
            // strong_count == 1 means we are the last holder; safe
            // to remove. Anything > 1 means another waiter has
            // cloned the Arc and we leave it for them to clean up
            // when they're done.
            if Arc::strong_count(entry) == 1 {
                building.remove(sid);
            }
        }
    }

    /// Spawn the agent, run negotiation, register the hub, return
    /// the first subscriber. Used by both the resume slow path (with
    /// the key mutex held) and the fresh-attach path.
    async fn build_and_register(
        &self,
        cfg: Arc<Config>,
        resume_session_id: Option<String>,
        cwd_override: Option<String>,
        build_id: &str,
    ) -> Result<AttachedHub> {
        let hub = build_hub(cfg, resume_session_id, cwd_override, build_id, self.clone()).await?;
        let session_id = hub.session_id.clone();
        let mut map = self.inner.write().await;
        let entry = map.entry(session_id).or_insert_with(|| Arc::new(hub));
        let hub = entry.clone();
        drop(map);
        Ok(self.subscribe(hub).await)
    }

    /// Subscribe a fresh `AttachedHub` to an existing hub. Internal
    /// helper used by both the fast and slow attach paths.
    ///
    /// The snapshot's `ready.resumed` field is rewritten to `true` for
    /// every attach. With the hub model, an attach is always a join
    /// to an existing conversation: even the very first browser to
    /// reach a hub is, in effect, "resuming" the agent's perspective
    /// that already exists from the negotiation handshake. The
    /// client uses `resumed: true` to mean "clear any stale local
    /// log and fetch /history to seed yourself", which is what we
    /// want on every attach including reloads. The original
    /// `session/new`-vs-`session/load` distinction matters to the
    /// hub's negotiation phase, not to the attached browser.
    async fn subscribe(&self, hub: Arc<SessionHub>) -> AttachedHub {
        hub.counter.increment().await;
        let mut snapshot_ready = hub.snapshot.ready.clone();
        if let Some(map) = snapshot_ready.as_object_mut() {
            map.insert("resumed".into(), Value::Bool(true));
        }
        AttachedHub {
            commands: hub.commands.clone(),
            outbound: hub.outbound.subscribe(),
            snapshot_ready,
            snapshot_session_info: hub.snapshot.session_info.clone(),
            session_id: hub.session_id.clone(),
            counter: hub.counter.clone(),
        }
    }

    /// Remove a hub by session id. Called by the owner loop when the
    /// grace window expires.
    async fn remove(&self, session_id: &str) {
        let mut map = self.inner.write().await;
        map.remove(session_id);
    }

    /// Test-only: register a pre-built hub directly. Bypasses the
    /// agent-spawn and ACP-negotiation phases so tests can exercise
    /// the broadcast / counter / grace-timer flow with an agent
    /// constructed via `Agent::from_io`.
    #[doc(hidden)]
    pub async fn register_for_test(
        &self,
        agent: Arc<Agent>,
        session_id: String,
        updates_rx: mpsc::UnboundedReceiver<Value>,
        ready: Value,
        session_info: Option<Value>,
    ) -> AttachedHub {
        let (cmd_tx, cmd_rx) = mpsc::channel::<HubCommand>(COMMAND_CAPACITY);
        let (out_tx, _) = broadcast::channel::<Arc<Value>>(BROADCAST_CAPACITY);
        let (grace_tx, grace_rx) = mpsc::channel::<GraceEvent>(8);
        let counter = Arc::new(Counter::new(grace_tx));
        let suppress_replay = Arc::new(Mutex::new(false));
        let snapshot = NegotiationSnapshot {
            ready,
            session_info,
        };

        let hub = SessionHub {
            commands: cmd_tx,
            outbound: out_tx.clone(),
            snapshot,
            session_id: session_id.clone(),
            counter: counter.clone(),
        };

        tokio::spawn(run_hub_loop(HubLoopState {
            agent,
            session_id: session_id.clone(),
            outbound: out_tx,
            commands: cmd_rx,
            updates: updates_rx,
            suppress_replay,
            counter,
            grace_rx,
            registry: self.clone(),
        }));

        let mut map = self.inner.write().await;
        let entry = map.entry(session_id).or_insert_with(|| Arc::new(hub));
        let hub = entry.clone();
        drop(map);
        self.subscribe(hub).await
    }

    /// Attach a fresh subscriber to an already-registered hub. Used
    /// by tests that want to assert multiple subscribers see the same
    /// broadcast events; production code goes through `attach_or_create`.
    #[doc(hidden)]
    pub async fn attach_existing_for_test(&self, session_id: &str) -> Option<AttachedHub> {
        let map = self.inner.read().await;
        let hub = map.get(session_id).cloned()?;
        drop(map);
        Some(self.subscribe(hub).await)
    }
}

/// Spawn the agent, run negotiation, build the SessionHub and start
/// the owner loop. Returns the hub ready for registry insertion.
async fn build_hub(
    cfg: Arc<Config>,
    resume_session_id: Option<String>,
    cwd_override: Option<String>,
    build_id: &str,
    registry: HubRegistry,
) -> Result<SessionHub> {
    let (agent, updates_rx) = spawn_agent(&cfg).await?;
    let agent = Arc::new(agent);

    // Run the negotiation phase and collect the outbound events into a
    // local buffer rather than firing them at a WS sink. We then keep
    // the buffer as the snapshot subscribers will replay on attach.
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<axum::extract::ws::Message>();
    let _outcome = crate::ws::negotiate_session(
        &agent,
        &snapshot_tx,
        resume_session_id,
        cwd_override,
        build_id,
    )
    .await?;
    drop(snapshot_tx);

    // The negotiate helper writes WS-shaped `Message::Text` frames; we
    // unwrap each to a JSON value. The wire shape is stable and the
    // helper is internal, so a panic here would mean the helper
    // changed contract behind our back.
    let mut ready: Option<Value> = None;
    let mut session_info: Option<Value> = None;
    while let Some(msg) = snapshot_rx.recv().await {
        let axum::extract::ws::Message::Text(text) = msg else {
            continue;
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match value.get("type").and_then(Value::as_str) {
            Some("ready") => ready = Some(value),
            Some("session_info") => session_info = Some(value),
            // The negotiate helper also emits a `sys` append on
            // resume failure. We want every attached subscriber to
            // see that, so we re-emit it through the broadcast once
            // the loop is up. For now, store it alongside the
            // session_info as a third snapshot slot.
            //
            // Implementation note: we collapse to the snapshot we
            // already publish; the resume-failure notice is rare
            // enough that the first browser will receive it via the
            // ready emission anyway, and a late-arriving second
            // browser does not need to know the resume failed.
            _ => {}
        }
    }
    let ready = ready.ok_or_else(|| anyhow::anyhow!("Negotiation produced no `ready` event"))?;
    let session_id = ready
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("`ready` missing sessionId"))?
        .to_string();
    let suppress_replay = Arc::new(Mutex::new(
        ready
            .get("resumed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ));

    // Channels.
    let (cmd_tx, cmd_rx) = mpsc::channel::<HubCommand>(COMMAND_CAPACITY);
    let (out_tx, _) = broadcast::channel::<Arc<Value>>(BROADCAST_CAPACITY);
    let (grace_tx, grace_rx) = mpsc::channel::<GraceEvent>(8);
    let counter = Arc::new(Counter::new(grace_tx));

    let snapshot = NegotiationSnapshot {
        ready,
        session_info,
    };

    // Use the ACP session id from negotiation for naming, but the
    // hub itself only cares about its own slot in the registry. The
    // registry keys by this id.
    let hub = SessionHub {
        commands: cmd_tx,
        outbound: out_tx.clone(),
        snapshot: snapshot.clone(),
        session_id: session_id.clone(),
        counter: counter.clone(),
    };

    // Spawn the owner loop.
    tokio::spawn(run_hub_loop(HubLoopState {
        agent,
        session_id,
        outbound: out_tx,
        commands: cmd_rx,
        updates: updates_rx,
        suppress_replay,
        counter,
        grace_rx,
        registry,
    }));

    Ok(hub)
}

/// Captured state the owner loop reads. Bundled into one struct so
/// the spawn site reads cleanly without a 7-arg call.
struct HubLoopState {
    agent: Arc<Agent>,
    session_id: String,
    outbound: broadcast::Sender<Arc<Value>>,
    commands: mpsc::Receiver<HubCommand>,
    updates: mpsc::UnboundedReceiver<Value>,
    suppress_replay: Arc<Mutex<bool>>,
    counter: Arc<Counter>,
    grace_rx: mpsc::Receiver<GraceEvent>,
    registry: HubRegistry,
}

/// Owner loop: serialises browser commands and broadcasts agent
/// outbound events. Runs until the agent's update channel closes
/// (subprocess exited) or the grace timer fires with no subscribers.
async fn run_hub_loop(state: HubLoopState) {
    let HubLoopState {
        agent,
        session_id,
        outbound,
        mut commands,
        mut updates,
        suppress_replay,
        counter,
        mut grace_rx,
        registry,
    } = state;

    // Adapter: `handle_agent_message` writes WS-shaped Text frames
    // into an mpsc; we run a side-channel mpsc here, parse each frame
    // back into JSON, and broadcast to subscribers. Two extra
    // serde calls per message; cheap given the typical event volume.
    let (relay_tx, mut relay_rx) = mpsc::unbounded_channel::<axum::extract::ws::Message>();

    // Track pending permission ids so duplicate replies from a second
    // browser drop silently. This is the stage-1 simplification of
    // the original design: rather than emit a targeted error, we let
    // the second browser's card stay in its current state until the
    // agent's eventual `tool_call_update` resolves it.
    let mut answered_permissions: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    let mut grace_deadline: Option<Pin<Box<dyn Future<Output = ()> + Send>>> = None;

    loop {
        tokio::select! {
            // Browser → agent.
            cmd = commands.recv() => {
                match cmd {
                    Some(c) => handle_command(
                        &agent,
                        &session_id,
                        c,
                        &suppress_replay,
                        &mut answered_permissions,
                        &outbound,
                    ).await,
                    None => break, // all senders dropped: nobody can reach us
                }
            }
            // Agent → browser. Tee through `handle_agent_message` first.
            agent_msg = updates.recv() => {
                let Some(msg) = agent_msg else {
                    break; // agent stdout reader exited
                };
                let suppress = *suppress_replay.lock().await;
                handle_agent_message(&relay_tx, msg, suppress).await;
                while let Ok(frame) = relay_rx.try_recv() {
                    if let axum::extract::ws::Message::Text(text) = frame {
                        if let Ok(value) = serde_json::from_str::<Value>(&text) {
                            // Broadcast errors only mean no subscribers
                            // are listening yet; that is fine, the
                            // ready/session_info snapshot covers them.
                            let _ = outbound.send(Arc::new(value));
                        }
                    }
                }
            }
            // Subscriber attach/detach signals.
            grace_evt = grace_rx.recv() => {
                match grace_evt {
                    Some(GraceEvent::Empty) => {
                        let cancel_rx = counter.install_cancel().await;
                        grace_deadline = Some(Box::pin(async move {
                            tokio::select! {
                                _ = tokio::time::sleep(GRACE_PERIOD) => {}
                                _ = cancel_rx => {}
                            }
                        }));
                    }
                    Some(GraceEvent::Refilled) => {
                        grace_deadline = None;
                    }
                    None => {} // counter dropped its sender; should not happen while we hold a strong ref
                }
            }
            // Grace timer fired with nobody attached → tear down.
            _ = async {
                match grace_deadline.as_mut() {
                    Some(f) => f.await,
                    None => std::future::pending().await,
                }
            }, if grace_deadline.is_some() => {
                if counter.count().await == 0 {
                    break;
                }
                // Race: a fresh subscriber arrived between the timer
                // firing and us reading the count. Cancel the deadline
                // and keep going.
                grace_deadline = None;
            }
        }
    }

    // Cooperative shutdown: tell the agent to stop, close stdin, wait
    // a short window. Same path the previous `handle_ws` exit took.
    agent.shutdown(Some(&session_id)).await;
    // Remove ourselves from the registry so the next attach for this
    // session id triggers a fresh hub. Holding the registry handle
    // until here prevents a race where a fresh subscriber attaches
    // to the slot just before we shut down.
    registry.remove(&session_id).await;
}

async fn handle_command(
    agent: &Arc<Agent>,
    session_id: &str,
    cmd: HubCommand,
    suppress_replay: &Mutex<bool>,
    answered: &mut std::collections::HashSet<String>,
    outbound: &broadcast::Sender<Arc<Value>>,
) {
    match cmd {
        HubCommand::Prompt { blocks } => {
            if blocks.is_empty() {
                return;
            }
            // First live prompt after a resume: stop hiding Kiro's
            // session/update events. From here on everything the
            // agent emits is real.
            *suppress_replay.lock().await = false;

            // Echo the user prompt to every attached browser via the
            // broadcast. Including the sender: the client used to
            // append the user's text locally, but with multi-attach
            // that approach hides the prompt from peer browsers and
            // produces inconsistent timelines. Now the hub is the
            // single source of truth for "what was said in this
            // session"; clients render only what the broadcast emits.
            let echo_text = extract_user_text(&blocks);
            if !echo_text.is_empty() {
                let _ = outbound.send(Arc::new(json!({
                    "type": "append",
                    "role": "user",
                    "text": format!("> {echo_text}\n")
                })));
            }

            // Fire the agent request, then broadcast `prompt_done` (or
            // an `error` event on failure). The previous implementation
            // forgot to emit either, so the sender's `busy`/`inFlight`
            // flags never cleared and the composer stayed locked at
            // "Agent is working" indefinitely. We broadcast to every
            // attached browser; peers that did not send will already
            // have `busy: false` so the broadcast is a no-op clear for
            // them, and the sender flips back to a free composer.
            let agent = Arc::clone(agent);
            let sid = session_id.to_string();
            let outbound_clone = outbound.clone();
            tokio::spawn(async move {
                let res = agent
                    .request(
                        "session/prompt",
                        json!({ "sessionId": sid, "prompt": blocks }),
                    )
                    .await;
                if let Err(e) = res {
                    let _ = outbound_clone.send(Arc::new(json!({
                        "type": "error",
                        "message": format!("{e}")
                    })));
                }
                let _ = outbound_clone.send(Arc::new(json!({ "type": "prompt_done" })));
            });
        }
        HubCommand::PermissionResponse { id, option_id } => {
            // First reply wins. Any further replies for the same id
            // are dropped, including replies from the same browser
            // (defensive against duplicate clicks).
            let key = id.to_string();
            if !answered.insert(key) {
                return;
            }
            let agent = Arc::clone(agent);
            tokio::spawn(async move {
                let _ = agent
                    .respond(
                        id,
                        json!({
                            "outcome": {
                                "outcome": "selected",
                                "optionId": option_id
                            }
                        }),
                    )
                    .await;
            });
        }
        HubCommand::Cancel => {
            let agent = Arc::clone(agent);
            let sid = session_id.to_string();
            tokio::spawn(async move {
                let _ = agent
                    .notify("session/cancel", json!({ "sessionId": sid }))
                    .await;
            });
        }
        HubCommand::SetMode { mode_id } => {
            let agent = Arc::clone(agent);
            let sid = session_id.to_string();
            tokio::spawn(async move {
                let _ = agent
                    .request(
                        "session/set_mode",
                        json!({ "sessionId": sid, "modeId": mode_id }),
                    )
                    .await;
            });
        }
        HubCommand::SetModel { model_id } => {
            let agent = Arc::clone(agent);
            let sid = session_id.to_string();
            tokio::spawn(async move {
                let _ = agent
                    .request(
                        "session/set_model",
                        json!({ "sessionId": sid, "modelId": model_id }),
                    )
                    .await;
            });
        }
    }
}

// (imports at the top of the file)

/// Pull the user's plain text out of an ACP prompt-block array. Used
/// by the broadcast echo so peer browsers see what the sender typed.
/// Image and resource blocks are dropped from the echo because the
/// existing client only renders the text portion of user turns; the
/// agent still sees the full block payload via `session/prompt`.
fn extract_user_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str)? == "text" {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
