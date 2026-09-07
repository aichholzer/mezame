//! Multi-attach session hub.
//!
//! A hub owns one [`Backend`] and broadcasts what
//! it produces to every WebSocket attached to the same session id.
//! Browsers attach and detach freely, the session stays warm across
//! reconnects within a grace window, and a laptop and a phone on one
//! session see the same conversation.
//!
//! Concurrency model:
//!
//! - Each hub runs a single owner task (`run_hub_loop`) that reads
//!   `HubCommand`s from an mpsc inbox. That serialises browser-originated
//!   commands: two browsers cannot race a prompt against each other
//!   through the same channel.
//! - A turn runs in its own spawned task, so the owner loop keeps
//!   draining commands while the Backend works. That task streams the
//!   turn's events and reports the outcome back to the loop; it never
//!   touches the in-flight count and never sends a terminal frame. The
//!   loop is the single writer of both, which is what keeps a second
//!   turn's echo from landing between the first turn's release and its
//!   `prompt_done`.
//! - The loop awaits nothing a Backend implements. `set_model`, `cancel`
//!   and `permission_response` each run on a spawned task, and the one
//!   with a reply reports it through a channel the loop selects on. A
//!   Backend that stalls on I/O therefore never stalls the inbox, the
//!   grace timer or a peer's cancel.
//! - The subscriber count is an atomic with a `Notify` beside it. An
//!   attach or a detach bumps the count and wakes the loop; the loop
//!   reads the count when it wakes. Nothing on that path takes a lock or
//!   waits on channel capacity, so nothing on it can wait on the loop.
//! - Outbound events fan out through a `tokio::sync::broadcast` sender.
//!   Each WS handler subscribes once on attach and forwards to its own
//!   sink. A lagged subscriber is skipped and the rest keep moving.
//! - `HubRegistry` is a `RwLock<HashMap>` keyed by session id. Lookups
//!   are read-locked; a build takes a per-id gate so two browsers
//!   arriving together with one id cannot both build a hub.
//!
//! Lifecycle:
//!
//! 1. First browser attaches: the registry builds the hub with its own
//!    Backend and starts the owner loop.
//! 2. Later browsers attach: the registry returns the existing hub and
//!    replays its `ready` snapshot, and its `session_info` snapshot when
//!    there is one, so every browser sees the same session.
//! 3. A browser detaches: the subscriber count decrements and the loop
//!    wakes. At zero the loop arms a grace timer. A fresh subscriber
//!    inside the window wakes the loop again, which drops the timer.
//! 4. The grace timer fires with nothing attached and no turn running:
//!    the hub removes itself from the registry, then shuts its Backend
//!    down. A turn still in flight holds teardown off, up to a cap. The
//!    same two steps run if the loop dies with a panic, so a slot in the
//!    registry never outlives the loop that drives it.
//!
//! The registry holds at most `MAX_LIVE_HUBS` hubs. An upgrade naming an
//! id with no live hub is answered 503 while it is full; a live session
//! is always joinable.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::FutureExt;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex, Notify, RwLock};
use tokio::time::{Instant, Sleep};

use crate::backend::{
    user_echo_event, user_text_len, Backend, EchoBackend, HistoryEntry, TurnOutcome,
};

/// How long a session stays warm after the last browser detaches. 30s
/// matches the WS reconnect-backoff cap on the client. A browser coming
/// back from a transient drop lands well inside this window.
pub const GRACE_PERIOD: Duration = Duration::from_secs(30);

/// How many hubs the registry holds at once, counting each session from
/// its first attach to the end of its grace window.
///
/// One idle hub costs about 40 KB (its broadcast ring, its inbox, its
/// task and an empty transcript) and the loop that drives it, and a hub
/// outlives its last socket by `GRACE_PERIOD`. With no cap, live hubs
/// equal the handshake rate times 30 seconds, and every unseen id costs
/// one. A person keeps tens of tabs across a few devices, not hundreds.
/// The worst case one peer can hold is this many transcripts at their
/// byte budget, 2 GiB of prompt text at 16 MiB each, all of it
/// bandwidth-bound and released 30 seconds after the peer stops. While
/// the cap is held, every new session is refused with a 503 and every
/// existing one stays joinable: a peer sustaining about four handshakes
/// a second can deny new sessions to the user, and cannot touch live
/// ones or the process.
pub const MAX_LIVE_HUBS: usize = 128;

/// How many grace periods a detached hub may hold a running turn before
/// teardown proceeds anyway. 60 periods is 30 minutes against the
/// production `GRACE_PERIOD`, and the multiple keeps the cap in
/// proportion for tests that drive a short period.
const MAX_INFLIGHT_HOLD_PERIODS: u32 = 60;

/// Ceiling on a single detached-but-busy hold. A turn that never resolves
/// would otherwise keep its hub and its Backend alive for the life of the
/// process.
fn max_inflight_hold(grace_period: Duration) -> Duration {
    grace_period * MAX_INFLIGHT_HOLD_PERIODS
}

/// Holds the single turn slot; releases it on drop.
///
/// The slot has to be released even if the turn task dies with the guard
/// still inside it. A count stuck above zero makes the hub immortal:
/// every grace fire re-arms for the capped window, and past the cap the
/// session is reclaimed mid-turn although it was never busy.
struct InflightGuard(Arc<AtomicUsize>);

impl InflightGuard {
    /// Claim the single turn slot. `None` when a turn is already in
    /// flight, which is what makes a second prompt mid-turn a no-op.
    fn claim(counter: &Arc<AtomicUsize>) -> Option<Self> {
        counter
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(Arc::clone(counter)))
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.store(0, Ordering::SeqCst);
    }
}

/// Process-static attach id counter. Bumped on every successful
/// `subscribe`. It wraps eventually, and attach lifetimes never overlap
/// a u64 worth of attaches, so equality checks against a turn's prompter
/// stay sound.
static NEXT_ATTACH_ID: AtomicU64 = AtomicU64::new(1);

/// Capacity of the outbound broadcast channel. High enough that a
/// subscriber falling 1024 events behind is a genuine problem and no
/// longer a momentary backlog. A streamed turn bursts at a few hundred
/// events at most.
const BROADCAST_CAPACITY: usize = 1024;

/// Ceiling on the text of one prompt, 1 MiB.
///
/// The text is what a turn puts into memory beyond the turn itself: the
/// echo in the broadcast ring, where the last `BROADCAST_CAPACITY` events
/// stay until overwritten, and the transcript. Attachments are not text;
/// the message ceiling in `ws` bounds them, and they are held for the
/// turn alone. A prompt past this ceiling is refused to its sender with
/// an `error` frame and nothing else happens: no echo, no turn slot, no
/// Backend call.
pub const MAX_PROMPT_TEXT_BYTES: usize = 1024 * 1024;

/// Capacity of the per-hub command inbox. Each browser sends commands at
/// user pace, and the loop drains them as fast as the Backend accepts.
/// 256 leaves headroom for rapid clicks with no visible backpressure.
const COMMAND_CAPACITY: usize = 256;

/// Browser to hub commands. The owner loop drains these and calls into
/// the Backend. The loop processes them one at a time, so two browsers
/// cannot interleave halfway through one command.
#[derive(Debug)]
pub enum HubCommand {
    /// Run a turn for `blocks`. `attach_id` identifies the attach that
    /// sent it, and the loop stamps that id on a permission request
    /// raised during the turn so peer browsers never see a card they
    /// were not asked to answer.
    Prompt { blocks: Vec<Value>, attach_id: u64 },
    /// Answer a permission request the hub broadcast earlier. `id` is
    /// the one the Backend minted. First answer wins; later answers for
    /// the same id are dropped in silence.
    PermissionResponse { id: Value, option_id: String },
    /// Cancel the turn in flight.
    Cancel,
    /// Change the selected model. One change runs at a time. A request
    /// arriving while one runs waits as the single pending request, and a
    /// later request replaces it, so the last selection a browser made is
    /// the one that runs next.
    SetModel { model_id: String },
}

/// What every attach is sent on arrival. Replayed so a browser joining
/// late sees the same session as the one that opened it.
#[derive(Clone)]
struct SessionSnapshot {
    /// The `ready` event in its final wire shape, bar the per-attach
    /// `resumed` and `busy` fields and the `buildId` the WS handler
    /// stamps.
    ready: Value,
    /// The most recent `session_info` frame, when the Backend has
    /// supplied one. Sent immediately after `ready`.
    session_info: Option<Value>,
}

/// Public handle to a session hub. Cheap to clone; all state lives behind
/// the senders. The owner task is held alive by these senders plus the
/// registry's `Arc`.
pub struct SessionHub {
    /// Pushes commands into the owner loop.
    commands: mpsc::Sender<HubCommand>,
    /// Subscribers receive outbound events from this. A new subscriber
    /// starts at the current head; earlier events are not replayed.
    outbound: broadcast::Sender<Arc<Value>>,
    /// The `ready` and `session_info` snapshots, replayed on every
    /// attach. Behind a `Mutex` because a successful model change
    /// replaces the `session_info` half.
    snapshot: Arc<Mutex<SessionSnapshot>>,
    /// The session id this hub is registered under.
    session_id: String,
    /// Subscriber count for the grace timer.
    counter: Arc<Counter>,
    /// Turns currently in flight, 0 or 1. Read by `subscribe` for the
    /// per-attach `ready.busy` field, and claimed by the loop's prompt
    /// arm.
    inflight: Arc<AtomicUsize>,
    /// Read by [`HubRegistry::history`] for `GET /history`.
    backend: Arc<dyn Backend>,
}

/// Tracks attaches and detaches for the grace timer. Lives behind an
/// `Arc` so both the registry and the per-attach guard reach it.
///
/// Two fields and no lock. The count is an atomic and the wake is a
/// `Notify`, so neither `increment` nor `decrement` waits on anything:
/// not on the owner loop, not on channel capacity, not on each other. An
/// earlier shape held a mutex across a bounded-channel send the loop had
/// to drain under that same mutex, and a Backend call awaited inline on
/// the loop plus one browser reconnecting a few times was enough to
/// wedge the session id until restart.
///
/// The loop reads the count when it wakes rather than trusting the change
/// that woke it, so two changes that coalesce into one wake, or two wakes
/// that land out of order, come to the same decision the count does.
struct Counter {
    count: AtomicUsize,
    /// Wakes the owner loop on every change to `count`.
    changed: Notify,
}

impl Counter {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            changed: Notify::new(),
        }
    }

    /// Call when a new subscriber attaches. Returns the post-attach
    /// count.
    fn increment(&self) -> usize {
        let count = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        self.changed.notify_one();
        count
    }

    /// Call when a subscriber detaches. Saturates at zero: a count that
    /// dipped below would read as "attached" forever and make the hub
    /// immortal. Returns the post-detach count.
    ///
    /// A compare-exchange loop rather than `fetch_update`, which Rust
    /// 1.99 deprecates in favour of a `try_update` the 1.86 floor does
    /// not have.
    fn decrement(&self) -> usize {
        let mut current = self.count.load(Ordering::SeqCst);
        loop {
            let next = current.saturating_sub(1);
            match self.count.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    self.changed.notify_one();
                    return next;
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// The current subscriber count.
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

/// Subscriber-side handle on an attached hub. Drop-on-detach: the `Drop`
/// impl decrements the counter, so the grace timer arms when the last
/// attach goes away.
pub struct AttachedHub {
    pub commands: mpsc::Sender<HubCommand>,
    pub outbound: broadcast::Receiver<Arc<Value>>,
    pub snapshot_ready: Value,
    pub snapshot_session_info: Option<Value>,
    pub session_id: String,
    /// Process-unique id for this attach. The WS handler filters
    /// targeted broadcasts on it, so a peer never renders a permission
    /// card it was not asked to answer.
    pub attach_id: u64,
    counter: Arc<Counter>,
}

impl AttachedHub {
    /// The receiver `subscribe` took, with everything it has buffered.
    ///
    /// Leaves a fresh, never-read receiver behind so `Drop` still has a
    /// field to drop. The attach loop has to read *this* receiver:
    /// `resubscribe` would start at the channel's tail and drop whatever
    /// arrived between the subscribe and the hand-off, which is the
    /// window a `prompt_done` lands in for an attach that read `busy` as
    /// true.
    pub fn take_outbound(&mut self) -> broadcast::Receiver<Arc<Value>> {
        let fresh = self.outbound.resubscribe();
        std::mem::replace(&mut self.outbound, fresh)
    }
}

impl Drop for AttachedHub {
    fn drop(&mut self) {
        // Synchronous and lock-free, so it runs to completion inside the
        // drop whatever the runtime is doing.
        self.counter.decrement();
    }
}

/// The registry is full: it holds `capacity` hubs and none is registered
/// under the id that asked. Answered 503 before the handshake by
/// `ws_upgrade`, and by an `error` frame from `handle_ws` if the cap is
/// reached between the pre-check and the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryFull {
    pub capacity: usize,
}

impl std::fmt::Display for RegistryFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this Mezame is serving its maximum of {} sessions; try again in {} seconds",
            self.capacity,
            GRACE_PERIOD.as_secs()
        )
    }
}

impl std::error::Error for RegistryFull {}

/// Registry of live hubs keyed by session id. Cheap to clone;
/// `Arc<RwLock>` lets the WS handler look hubs up without coordinating
/// with any owner loop.
///
/// `building` serialises hub construction by session id. Two browsers
/// reconnecting at the same moment with the same id must not both build
/// a hub: the second would replace the first in the registry, and the
/// two halves of one conversation would run against two Backends.
/// Holding a per-key mutex across the build window means the second
/// arrival finds the hub on its re-check and subscribes to it instead.
///
/// `capacity` bounds the map. It is read under the write lock at the one
/// point a hub is inserted, so the bound holds whatever the pre-check in
/// `ws_upgrade` saw.
#[derive(Clone)]
pub struct HubRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<SessionHub>>>>,
    building: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    capacity: usize,
}

impl Default for HubRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HubRegistry {
    /// A registry holding at most [`MAX_LIVE_HUBS`] hubs.
    pub fn new() -> Self {
        Self::with_capacity(MAX_LIVE_HUBS)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::default(),
            building: Arc::default(),
            capacity,
        }
    }

    /// Test-only: a registry with a small capacity, so a test reaches the
    /// cap with a handful of hubs.
    #[doc(hidden)]
    pub fn with_capacity_for_test(capacity: usize) -> Self {
        Self::with_capacity(capacity)
    }

    /// Whether an upgrade naming `session_id` can be served now: a live
    /// session is always joinable, and a new one needs a free slot. An
    /// advisory read for `ws_upgrade`, so a full registry is answered
    /// before the handshake; `build_and_register` decides under the lock.
    pub async fn admits(&self, session_id: &str) -> bool {
        let map = self.inner.read().await;
        map.contains_key(session_id) || map.len() < self.capacity
    }

    /// Attach to the hub registered under `session_id`, building one if
    /// none is. Always returns an `AttachedHub` whose `Drop` decrements
    /// the counter.
    pub async fn attach_or_create(&self, session_id: &str) -> Result<AttachedHub> {
        self.attach_or_create_parked(session_id, std::future::ready(()))
            .await
    }

    /// Test-only: `attach_or_create` with `park` awaited after the per-id
    /// gate is taken and before the hub is built. A case can hold the
    /// first arrival inside the build window while a second arrival
    /// reaches the gate, which is the interleaving Requirement 6 criterion
    /// 4 describes and one a single-threaded scheduler never produces on
    /// its own: the first arrival builds, registers and subscribes in one
    /// poll, and the second always finds the hub on the fast path.
    #[doc(hidden)]
    pub async fn attach_or_create_parked_for_test(
        &self,
        session_id: &str,
        park: impl std::future::Future<Output = ()>,
    ) -> Result<AttachedHub> {
        self.attach_or_create_parked(session_id, park).await
    }

    async fn attach_or_create_parked(
        &self,
        session_id: &str,
        park: impl std::future::Future<Output = ()>,
    ) -> Result<AttachedHub> {
        // Fast path: a hub is already registered under this id.
        if let Some(hub) = self.lookup(session_id).await {
            return Ok(self.subscribe(hub).await);
        }

        // Slow path, behind the per-id gate. The gate and the local
        // handle on it are both released before the cleanup below, so
        // the cleanup can recognise itself as the last holder and the
        // map stays bounded by the number of concurrent builds rather
        // than by the number of session ids this process has seen.
        let result = {
            let key_mutex = {
                let mut building = self.building.lock().await;
                building
                    .entry(session_id.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };
            let _guard = key_mutex.lock().await;
            park.await;

            // Re-check now that the gate is held: the first arrival
            // registered its hub before releasing.
            match self.lookup(session_id).await {
                Some(hub) => Ok(self.subscribe(hub).await),
                None => self.build_and_register(session_id).await,
            }
        };
        self.cleanup_build_slot(session_id).await;
        result
    }

    /// The hub registered under `session_id`, if there is one.
    async fn lookup(&self, session_id: &str) -> Option<Arc<SessionHub>> {
        self.inner.read().await.get(session_id).cloned()
    }

    /// Drop the per-key mutex once nobody else holds it. Without this the
    /// map grows by one entry per session id the process has seen.
    async fn cleanup_build_slot(&self, sid: &str) {
        let mut building = self.building.lock().await;
        if let Some(entry) = building.get(sid) {
            // A strong count of 1 means the map holds the only handle
            // and the removal is safe. Anything above 1 means another
            // arrival has cloned it, and the cleanup falls to whoever
            // finishes last.
            if Arc::strong_count(entry) == 1 {
                building.remove(sid);
            }
        }
    }

    /// Build the hub, register it, and return its first subscriber.
    /// Called with the per-id gate held.
    ///
    /// The cap is checked and the hub built under the write lock, so no
    /// two builds can both see a free slot. `build_hub` awaits nothing,
    /// so the lock is held for microseconds. Building before the check
    /// would spawn a loop whose teardown removes the id from the map.
    async fn build_and_register(&self, session_id: &str) -> Result<AttachedHub> {
        let mut map = self.inner.write().await;
        if map.len() >= self.capacity {
            return Err(RegistryFull {
                capacity: self.capacity,
            }
            .into());
        }
        let hub = build_hub(session_id, self.clone())?;
        // The gate above is what makes the occupied case unreachable
        // here; `or_insert_with` keeps the insert atomic against
        // `register_for_test` all the same.
        let entry = map
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(hub));
        let hub = entry.clone();
        // Released before `subscribe`, which awaits the snapshot lock.
        drop(map);
        Ok(self.subscribe(hub).await)
    }

    /// Subscribe a fresh `AttachedHub` to an existing hub.
    ///
    /// The order matters. The broadcast receiver is taken before the
    /// in-flight count is read, so a `busy` of `true` means the count was
    /// above zero at a moment later than the subscribe. The loop
    /// decrements before it broadcasts `prompt_done`, so that
    /// `prompt_done` lands inside this receiver's window and the
    /// composer this attach locked is unlocked again.
    ///
    /// `ready.resumed` is `true` on every attach. Under the hub model an
    /// attach is always a join to a conversation that already exists,
    /// and the client reads `resumed` as "clear any stale local log and
    /// seed yourself from the history endpoint".
    async fn subscribe(&self, hub: Arc<SessionHub>) -> AttachedHub {
        hub.counter.increment();
        let outbound = hub.outbound.subscribe();
        let busy = hub.inflight.load(Ordering::SeqCst) > 0;
        let snapshot = hub.snapshot.lock().await;
        let mut snapshot_ready = snapshot.ready.clone();
        if let Some(map) = snapshot_ready.as_object_mut() {
            map.insert("resumed".into(), Value::Bool(true));
            map.insert("busy".into(), Value::Bool(busy));
        }
        let snapshot_session_info = snapshot.session_info.clone();
        drop(snapshot);
        AttachedHub {
            commands: hub.commands.clone(),
            outbound,
            snapshot_ready,
            snapshot_session_info,
            session_id: hub.session_id.clone(),
            attach_id: NEXT_ATTACH_ID.fetch_add(1, Ordering::Relaxed),
            counter: hub.counter.clone(),
        }
    }

    /// The transcript of the Backend behind `session_id`, or `None` when
    /// no hub is registered under it. Creates nothing.
    ///
    /// The `Arc` on the Backend is cloned under the read lock and the
    /// lock is released before the transcript is awaited. Holding it
    /// across that await would let a slow Backend block every attach.
    pub async fn history(&self, session_id: &str) -> Option<Vec<HistoryEntry>> {
        let backend = {
            let map = self.inner.read().await;
            map.get(session_id).map(|hub| Arc::clone(&hub.backend))
        }?;
        Some(backend.history().await)
    }

    /// Remove a hub by session id. Called by the owner loop on exit.
    async fn remove(&self, session_id: &str) {
        let mut map = self.inner.write().await;
        map.remove(session_id);
    }

    /// Test-only: report whether a hub is registered for `session_id`
    /// without attaching. Attaching would increment the subscriber count
    /// and disarm the grace timer, so this is how a test observes
    /// teardown.
    #[doc(hidden)]
    pub async fn is_registered_for_test(&self, session_id: &str) -> bool {
        self.inner.read().await.contains_key(session_id)
    }

    /// Test-only: register a hub around a caller-supplied Backend, with
    /// a caller-supplied `ready` and `session_info`. Bypasses
    /// `build_hub` so a test drives the broadcast, the counter and the
    /// grace timer against a scripted Backend.
    #[doc(hidden)]
    pub async fn register_for_test(
        &self,
        backend: Arc<dyn Backend>,
        session_id: String,
        ready: Value,
        session_info: Option<Value>,
    ) -> AttachedHub {
        self.register_for_test_with_grace(GRACE_PERIOD, backend, session_id, ready, session_info)
            .await
    }

    /// Test-only variant of `register_for_test` with an explicit grace
    /// period, so a test drives the timer with a short value instead of
    /// waiting out the production one.
    #[doc(hidden)]
    pub async fn register_for_test_with_grace(
        &self,
        grace_period: Duration,
        backend: Arc<dyn Backend>,
        session_id: String,
        ready: Value,
        session_info: Option<Value>,
    ) -> AttachedHub {
        let (cmd_tx, cmd_rx) = mpsc::channel::<HubCommand>(COMMAND_CAPACITY);
        let (out_tx, _) = broadcast::channel::<Arc<Value>>(BROADCAST_CAPACITY);
        let counter = Arc::new(Counter::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        let snapshot = Arc::new(Mutex::new(SessionSnapshot {
            ready,
            session_info,
        }));

        let hub = SessionHub {
            commands: cmd_tx,
            outbound: out_tx.clone(),
            snapshot: snapshot.clone(),
            session_id: session_id.clone(),
            counter: counter.clone(),
            inflight: inflight.clone(),
            backend: Arc::clone(&backend),
        };

        tokio::spawn(run_hub_loop(HubLoopState {
            backend,
            session_id: session_id.clone(),
            outbound: out_tx,
            commands: cmd_rx,
            counter,
            registry: self.clone(),
            snapshot,
            grace_period,
            inflight,
        }));

        let mut map = self.inner.write().await;
        let entry = map.entry(session_id).or_insert_with(|| Arc::new(hub));
        let hub = entry.clone();
        drop(map);
        self.subscribe(hub).await
    }

    /// Test-only: attach a fresh subscriber to an already-registered
    /// hub. Production code goes through `attach_or_create`.
    #[doc(hidden)]
    pub async fn attach_existing_for_test(&self, session_id: &str) -> Option<AttachedHub> {
        let hub = self.lookup(session_id).await?;
        Some(self.subscribe(hub).await)
    }
}

/// Build the hub for `session_id` with its own Backend and start the
/// owner loop. Returns the hub ready for registry insertion.
///
/// Fallible for one call: the working directory the `ready` template
/// reports. The OS answers with an absolute path, and a browser cannot
/// choose another one. Synchronous, so the caller may hold the registry
/// lock across it.
fn build_hub(session_id: &str, registry: HubRegistry) -> Result<SessionHub> {
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let ready = json!({
        "type": "ready",
        "sessionId": session_id,
        "resumed": true,
        "cwd": cwd,
        "promptCapabilities": {
            "image": true,
            "audio": false,
            "embeddedContext": true
        }
    });

    // One Backend per hub. This is the line a later phase changes.
    let backend: Arc<dyn Backend> = Arc::new(EchoBackend::new());

    let (cmd_tx, cmd_rx) = mpsc::channel::<HubCommand>(COMMAND_CAPACITY);
    let (out_tx, _) = broadcast::channel::<Arc<Value>>(BROADCAST_CAPACITY);
    let counter = Arc::new(Counter::new());
    let inflight = Arc::new(AtomicUsize::new(0));
    let snapshot = Arc::new(Mutex::new(SessionSnapshot {
        ready,
        session_info: None,
    }));

    let hub = SessionHub {
        commands: cmd_tx,
        outbound: out_tx.clone(),
        snapshot: snapshot.clone(),
        session_id: session_id.to_string(),
        counter: counter.clone(),
        inflight: inflight.clone(),
        backend: Arc::clone(&backend),
    };

    tokio::spawn(run_hub_loop(HubLoopState {
        backend,
        session_id: session_id.to_string(),
        outbound: out_tx,
        commands: cmd_rx,
        counter,
        registry,
        snapshot,
        grace_period: GRACE_PERIOD,
        inflight,
    }));

    Ok(hub)
}

/// What a finished turn hands back to the loop.
struct TurnDone {
    /// The turn's own result, wrapped in the panic outcome
    /// `catch_unwind` produces.
    result: std::result::Result<Result<TurnOutcome>, Box<dyn Any + Send>>,
    /// Still claimed. The loop drops it, then sends the terminal frames.
    guard: InflightGuard,
}

/// Captured state the owner loop reads. Bundled into one struct so the
/// spawn site reads cleanly.
struct HubLoopState {
    backend: Arc<dyn Backend>,
    session_id: String,
    outbound: broadcast::Sender<Arc<Value>>,
    commands: mpsc::Receiver<HubCommand>,
    counter: Arc<Counter>,
    registry: HubRegistry,
    /// Shared with `SessionHub::snapshot`. The loop replaces the
    /// `session_info` half on a successful model change, and every later
    /// attach replays the current selection.
    snapshot: Arc<Mutex<SessionSnapshot>>,
    /// How long the session stays warm after the last browser detaches.
    /// Held on the loop state so a test drives the timer with a short
    /// period.
    grace_period: Duration,
    /// Shared with `SessionHub::inflight`, so `subscribe` reads the same
    /// count the loop claims.
    inflight: Arc<AtomicUsize>,
}

/// Owner loop: serialises browser commands, sends the frames that end a
/// turn, and tears the hub down when nothing needs it.
///
/// The two teardown steps at the bottom run however the loop ends.
/// `drive` holds the loop proper under `catch_unwind`, so a panic on the
/// owner task still frees the registry slot and shuts the Backend down.
/// Without that the slot would hold a hub nobody drives, every later
/// attach for the id would join it and hang, and only a restart would
/// clear it.
async fn run_hub_loop(state: HubLoopState) {
    let backend = Arc::clone(&state.backend);
    let registry = state.registry.clone();
    let session_id = state.session_id.clone();

    if let Err(panic) = AssertUnwindSafe(drive(state)).catch_unwind().await {
        warn(&format!(
            "Session {session_id}: the hub loop panicked: {}. Releasing the session.",
            panic_message(panic)
        ));
    }

    // Free the registry slot first, then release the Backend. The loop
    // has returned, so its inbox is closed: an attach that found this
    // hub in the registry now would send into a dead inbox and be
    // dropped, and the browser would reconnect into a fresh session. With
    // the slot gone before the shutdown runs, an attach arriving during
    // a slow shutdown builds a fresh hub instead. The window that is
    // left is the write lock below, not the shutdown's latency, and an
    // attach caught in it self-heals the same way.
    registry.remove(&session_id).await;
    backend.shutdown().await;
}

/// Permission ids the Backend has raised in the turn in flight and no
/// browser has answered yet.
///
/// `forward` adds an id when it broadcasts the request. The first answer
/// removes it and reaches the Backend; every other answer, for that id or
/// for one never raised, is dropped. The set is cleared when the turn
/// ends. It is bounded by what the Backend asks, not by what a browser
/// sends: an earlier shape remembered every id a browser ever answered
/// with, which let one attached peer grow the hub's memory without bound.
///
/// A `std::sync::Mutex`, held for one insert, remove or clear and never
/// across an await. The turn task and the loop both reach it.
type Outstanding = Arc<std::sync::Mutex<HashSet<String>>>;

/// Lock the outstanding set, recovering from poison. A holder panicking
/// mid-operation is a bug elsewhere; it must not also wedge every later
/// permission on the session.
fn lock_outstanding(outstanding: &Outstanding) -> std::sync::MutexGuard<'_, HashSet<String>> {
    outstanding
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Where the hub is with model changes.
///
/// One runs at a time. A request arriving mid-change waits as the single
/// pending one, and a later request replaces it: the last selection a
/// browser made is the one that runs next, and a burst of clicks spawns
/// one task per completed change rather than one per click.
#[derive(Default)]
struct ModelChange {
    in_flight: bool,
    pending: Option<String>,
}

/// The loop proper. Returns when every command sender is gone or when
/// the grace timer decides the hub is done; `run_hub_loop` tears down.
async fn drive(state: HubLoopState) {
    let HubLoopState {
        backend,
        session_id,
        outbound,
        mut commands,
        counter,
        registry: _,
        snapshot,
        grace_period,
        inflight,
    } = state;

    let outstanding: Outstanding = Arc::default();

    // Turn outcomes come back here. The sender is cloned into every turn
    // task and the loop holds this one, so the receiver never closes
    // while the loop runs.
    let (turn_done_tx, mut turn_done_rx) = mpsc::unbounded_channel::<TurnDone>();

    // Model-change replies come back here. At most one change is in
    // flight, so the channel never holds more than one message.
    let (model_done_tx, mut model_done_rx) = mpsc::unbounded_channel::<Result<Value>>();
    let mut model_change = ModelChange::default();

    // When the current detached-but-busy hold began. `None` whenever a
    // browser is attached or no turn is running.
    let mut inflight_hold_since: Option<Instant> = None;

    // `Some` while nobody is attached and the grace window is open.
    let mut grace_deadline: Option<Pin<Box<Sleep>>> = None;

    loop {
        tokio::select! {
            // Browser to Backend.
            cmd = commands.recv() => {
                match cmd {
                    Some(c) => handle_command(
                        CommandContext {
                            backend: &backend,
                            session_id: &session_id,
                            outbound: &outbound,
                            inflight: &inflight,
                            turn_done_tx: &turn_done_tx,
                            outstanding: &outstanding,
                            model_change: &mut model_change,
                            model_done_tx: &model_done_tx,
                        },
                        c,
                    ),
                    None => break, // every sender dropped: nobody can reach us
                }
            }
            // A turn finished. This arm is synchronous from its first
            // line to its last: the slot is released and `prompt_done`
            // goes out with no await between them, so no second turn's
            // echo can land in the gap and unlock a composer early.
            Some(TurnDone { result, guard }) = turn_done_rx.recv() => {
                drop(guard);
                // A permission the turn left unanswered has no turn to
                // answer into now.
                lock_outstanding(&outstanding).clear();
                match result {
                    Ok(Ok(_outcome)) => {}
                    Ok(Err(e)) => broadcast_error(&outbound, format!("{e}")),
                    Err(panic) => broadcast_error(
                        &outbound,
                        format!("The turn panicked: {}", panic_message(panic)),
                    ),
                }
                let _ = outbound.send(Arc::new(json!({ "type": "prompt_done" })));
            }
            // A model change resolved. Apply it, then run the pending one
            // if a browser asked for another in the meantime.
            Some(result) = model_done_rx.recv() => {
                apply_model_change(result, &snapshot, &outbound).await;
                match model_change.pending.take() {
                    Some(model_id) => spawn_set_model(&backend, model_id, &model_done_tx),
                    None => model_change.in_flight = false,
                }
            }
            // A subscriber attached or detached. Read the count rather
            // than trust the wake: two changes can coalesce into one.
            _ = counter.changed.notified() => {
                if counter.count() == 0 {
                    // Arm once. A second detach-to-zero inside an open
                    // window (two attaches gone within milliseconds)
                    // must not push the deadline out.
                    if grace_deadline.is_none() {
                        grace_deadline = Some(Box::pin(tokio::time::sleep(grace_period)));
                    }
                } else {
                    // A browser is back. Drop the window, and any later
                    // hold starts its cap afresh.
                    grace_deadline = None;
                    inflight_hold_since = None;
                }
            }
            // The grace timer fired with nobody attached.
            _ = async {
                match grace_deadline.as_mut() {
                    Some(deadline) => deadline.await,
                    None => std::future::pending().await,
                }
            }, if grace_deadline.is_some() => {
                if counter.count() != 0 {
                    // A fresh subscriber arrived between the timer
                    // firing and the count read, and its wake is still
                    // queued. Cancel and keep going.
                    grace_deadline = None;
                    inflight_hold_since = None;
                } else if inflight.load(Ordering::SeqCst) == 0 {
                    break;
                } else {
                    // Nobody is attached and a turn is still running.
                    // Tearing down here would cancel the user's turn,
                    // which is what losing focus on a phone mid-turn
                    // used to do. Hold and re-arm; a later fire reclaims
                    // the session once the turn resolves.
                    //
                    // The hold is capped. A turn that never resolves
                    // would otherwise keep this hub and its Backend
                    // alive for the life of the process.
                    let held_since = *inflight_hold_since.get_or_insert_with(Instant::now);
                    let held_for = held_since.elapsed();
                    if held_for >= max_inflight_hold(grace_period) {
                        warn(&format!(
                            "Session {session_id}: a turn has been in flight for {held_for:?} \
                             with no browser attached. Releasing the session."
                        ));
                        break;
                    }
                    grace_deadline = Some(Box::pin(tokio::time::sleep(grace_period)));
                }
            }
        }
    }
}

/// Write one line to stderr, ignoring a failure to. `eprintln!` panics
/// when stderr is a closed pipe, and a panic on the owner task costs the
/// session; a log line is not worth that.
fn warn(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

/// Broadcast one `error` frame. A send failure only means no subscriber
/// is listening.
fn broadcast_error(outbound: &broadcast::Sender<Arc<Value>>, message: String) {
    let _ = outbound.send(Arc::new(json!({
        "type": "error",
        "message": message
    })));
}

/// The text a panic payload carries, or a stand-in when it carries none.
fn panic_message(panic: Box<dyn Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => match panic.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "no message".to_string(),
        },
    }
}

/// Broadcast one streamed event, with its fields unchanged and in order.
///
/// A `permission_request` is stamped with the prompter's `attach_id`, and
/// nothing else is. The attach loop drops a stamped frame on every other
/// attach, so a peer never renders a card it was not asked to answer. The
/// request's id also joins the outstanding set, which is what lets an
/// answer for it through later.
fn forward(
    outbound: &broadcast::Sender<Arc<Value>>,
    mut event: Value,
    attach_id: u64,
    outstanding: &Outstanding,
) {
    if event.get("type").and_then(Value::as_str) == Some("permission_request") {
        // Keyed on the id's JSON rendering, the same key the answer is
        // matched on, so a numeric 1 and a string "1" stay distinct.
        if let Some(id) = event.get("id") {
            lock_outstanding(outstanding).insert(id.to_string());
        }
        if let Some(map) = event.as_object_mut() {
            map.insert("_target".into(), Value::Number(attach_id.into()));
        }
    }
    let _ = outbound.send(Arc::new(event));
}

/// Run one turn: stream its events, drain what is buffered, report the
/// outcome to the loop.
///
/// This task never touches the in-flight count and never sends a terminal
/// frame. The guard travels back inside the message, so a task that dies
/// before sending still releases the count through the guard's `Drop`.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    backend: Arc<dyn Backend>,
    blocks: Vec<Value>,
    events_tx: mpsc::UnboundedSender<Value>,
    mut events_rx: mpsc::UnboundedReceiver<Value>,
    outbound: broadcast::Sender<Arc<Value>>,
    attach_id: u64,
    outstanding: Outstanding,
    guard: InflightGuard,
    done: mpsc::UnboundedSender<TurnDone>,
) {
    // `catch_unwind` turns a panicking turn into an outcome this task can
    // still report. Without it the task dies and every composer on the
    // session stays locked. It covers the polled future and nothing the
    // Backend spawns, which is what the trait's obligations say.
    let mut turn = Box::pin(AssertUnwindSafe(backend.prompt(blocks, events_tx)).catch_unwind());
    let mut drained = false;
    let result = loop {
        tokio::select! {
            event = events_rx.recv(), if !drained => match event {
                Some(event) => forward(&outbound, event, attach_id, &outstanding),
                // The Backend dropped its sender ahead of resolving.
                // The guard keeps this arm from spinning on a closed
                // channel for the rest of the turn.
                None => drained = true,
            },
            outcome = &mut turn => break outcome,
        }
    };

    // The Hub, not the Backend, decides when the channel is over.
    // `close` rejects every later send and `recv` then returns `None`
    // once the buffer is empty, whether or not a clone of the sender is
    // still alive somewhere inside the Backend.
    events_rx.close();
    while let Some(event) = events_rx.recv().await {
        forward(&outbound, event, attach_id, &outstanding);
    }

    // A loop that has already exited dropped the receiver. The send then
    // fails, the guard is released on the drop, and the Backend's
    // shutdown obligation is what ended the turn.
    let _ = done.send(TurnDone { result, guard });
}

/// Run `set_model` on its own task and report the reply to the loop.
///
/// A panic inside the Backend's future is reported as an `Err`, so the
/// loop always hears back and the in-flight flag is always cleared. A
/// change that never reported would leave every later change pending
/// forever.
fn spawn_set_model(
    backend: &Arc<dyn Backend>,
    model_id: String,
    done: &mpsc::UnboundedSender<Result<Value>>,
) {
    let backend = Arc::clone(backend);
    let done = done.clone();
    tokio::spawn(async move {
        let result = match AssertUnwindSafe(backend.set_model(model_id))
            .catch_unwind()
            .await
        {
            Ok(result) => result,
            Err(panic) => Err(anyhow::anyhow!(
                "the model change panicked: {}",
                panic_message(panic)
            )),
        };
        // A closed receiver means the loop is gone, and so is the hub.
        let _ = done.send(result);
    });
}

/// Apply a resolved model change: store and broadcast the new
/// `session_info`, or tell every browser the change did not take.
async fn apply_model_change(
    result: Result<Value>,
    snapshot: &Arc<Mutex<SessionSnapshot>>,
    outbound: &broadcast::Sender<Arc<Value>>,
) {
    match result {
        Ok(info) => {
            let frame = json!({ "type": "session_info", "info": info });
            // Store and broadcast under one lock. An attach that reads
            // the snapshot before the store sees the old value and
            // receives the new frame on its receiver; one that reads
            // after sees the new value and receives a harmless
            // duplicate.
            let mut snap = snapshot.lock().await;
            snap.session_info = Some(frame.clone());
            let _ = outbound.send(Arc::new(frame));
            drop(snap);
        }
        Err(e) => {
            // The sender already shows the new selection from its
            // optimistic update. The notice tells everyone it did not
            // take, and no `session_info` goes out.
            let _ = outbound.send(Arc::new(json!({
                "type": "append",
                "role": "sys",
                "text": format!("\n[The model change failed: {e}]\n")
            })));
        }
    }
}

/// What `handle_command` reads and writes, borrowed from the loop for
/// the length of one command.
struct CommandContext<'a> {
    backend: &'a Arc<dyn Backend>,
    session_id: &'a str,
    outbound: &'a broadcast::Sender<Arc<Value>>,
    inflight: &'a Arc<AtomicUsize>,
    turn_done_tx: &'a mpsc::UnboundedSender<TurnDone>,
    outstanding: &'a Outstanding,
    model_change: &'a mut ModelChange,
    model_done_tx: &'a mpsc::UnboundedSender<Result<Value>>,
}

/// Act on one browser command. Synchronous: nothing here waits on the
/// Backend, so no Backend can hold the inbox.
fn handle_command(ctx: CommandContext<'_>, cmd: HubCommand) {
    match cmd {
        HubCommand::Prompt { blocks, attach_id } => {
            if blocks.is_empty() {
                return;
            }
            let text_len = user_text_len(&blocks);
            if text_len > MAX_PROMPT_TEXT_BYTES {
                // Refused ahead of the claim and the echo, so no composer
                // locks and no peer sees a turn start. Stamped for the
                // sender: that browser renders the error and unlocks its
                // composer, and every other attach drops the frame.
                let _ = ctx.outbound.send(Arc::new(json!({
                    "type": "error",
                    "message": format!(
                        "The prompt holds {text_len} bytes of text; the limit is \
                         {MAX_PROMPT_TEXT_BYTES} bytes."
                    ),
                    "_target": attach_id
                })));
                return;
            }
            // The claim, the echo and the spawn hold no await between
            // them, so no other command interleaves and the count is
            // above zero from before the echo until the loop releases
            // it.
            let Some(guard) = InflightGuard::claim(ctx.inflight) else {
                warn(&format!(
                    "Session {}: prompt discarded, a turn is already in flight",
                    ctx.session_id
                ));
                return;
            };
            let _ = ctx.outbound.send(Arc::new(user_echo_event(&blocks)));
            let (events_tx, events_rx) = mpsc::unbounded_channel::<Value>();
            tokio::spawn(run_turn(
                Arc::clone(ctx.backend),
                blocks,
                events_tx,
                events_rx,
                ctx.outbound.clone(),
                attach_id,
                Arc::clone(ctx.outstanding),
                guard,
                ctx.turn_done_tx.clone(),
            ));
        }
        HubCommand::PermissionResponse { id, option_id } => {
            // First answer wins: the id leaves the outstanding set with
            // the answer that takes it. A later answer for the same id,
            // a duplicate click from one browser included, finds nothing
            // and is dropped. So is an answer for an id the Backend never
            // raised, which is the only way the set stays bounded by the
            // Backend's requests rather than by a browser's frames.
            if !lock_outstanding(ctx.outstanding).remove(&id.to_string()) {
                warn(&format!(
                    "Session {}: dropped an answer for a permission that is not outstanding",
                    ctx.session_id
                ));
                return;
            }
            let backend = Arc::clone(ctx.backend);
            tokio::spawn(async move {
                backend.permission_response(id, option_id).await;
            });
        }
        HubCommand::Cancel => {
            // Spawned, so a slow Backend cannot stall the command inbox
            // and the grace arm behind one call.
            let backend = Arc::clone(ctx.backend);
            tokio::spawn(async move {
                backend.cancel().await;
            });
        }
        HubCommand::SetModel { model_id } => {
            // Never awaited here. The reply is applied by the loop when it
            // arrives, one change at a time; a change that stalls on I/O
            // stalls nothing else on the session.
            if ctx.model_change.in_flight {
                ctx.model_change.pending = Some(model_id);
            } else {
                ctx.model_change.in_flight = true;
                spawn_set_model(ctx.backend, model_id, ctx.model_done_tx);
            }
        }
    }
}
