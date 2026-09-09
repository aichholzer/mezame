//! Cloudflared transport: the HTTP/WS server that fronts Mezame.
//!
//! axum serves the embedded UI at `/` and accepts WS upgrades at `/ws`.
//! Public reachability is delegated to an external Cloudflare Tunnel;
//! Mezame binds loopback by default.
//!
//! Also home to the plain HTTP endpoints: `/state` (cross-device browser
//! state), `/history` (a session's transcript), and the embedded-asset
//! fallback. Every route sits behind the `Host` and `Origin` checks in
//! `guard.rs`, the two request checks that need no user identity.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;

use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use axum::{
    body::{Body, Bytes},
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, patch, post},
    Extension, Json, Router,
};

use futures_util::stream::Stream;
use futures_util::FutureExt;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch, Notify};

use crate::auth::{
    self, clear_cookie_header, cookie_value, dummy_hash, set_cookie_header, sign, verify,
    verify_password, AuthUser, Cookie, RateLimiter,
};
use crate::backend::{TRANSCRIPT_BUDGET_BYTES, TRANSCRIPT_MAX_ENTRIES};
use crate::config::Config;
use crate::conversation::Conversation;

use crate::guard::{guard_request, RequestPolicy};
use crate::hub::{warn, HubRegistry};
use crate::store::crypto::Keys;
use crate::store::{SessionList, SessionRow, Store, StoreError, UserRow, USER_NAME_MAX_CHARS};

use crate::ws::ws_upgrade;

/// Unix seconds, as the server sees them. A test installs its own.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Shared state for the axum router. Bundles the configuration with the
/// live `HubRegistry` so the WS handler can attach to existing hubs or
/// create new ones for fresh sessions, the store and the keys every
/// identity check reads, plus a broadcast channel that fires whenever a
/// user's sessions or settings change so that user's connected browsers
/// refetch `/state` without a manual reload.
pub struct AppState {
    /// Read per request; swappable so a later phase can reload it.
    pub config: ArcSwap<Config>,
    pub hubs: HubRegistry,
    pub store: Arc<dyn Store>,
    pub keys: Keys,
    pub limiter: RateLimiter,
    pub clock: Clock,
    /// Tick channel, carrying the id of the user whose state changed: a
    /// session created, renamed, titled, archived, restored or deleted,
    /// or settings written. `/state/events` forwards a tick to that
    /// user's streams alone, and each refetches `/state`. Receivers that
    /// lag are skipped ahead; the next tick brings them back in sync.
    pub state_changes: broadcast::Sender<String>,
    /// The root a user's default workspace is created at on their first
    /// session: the server's working directory when it is eligible.
    pub workspace_root: Option<PathBuf>,

    /// Process-wide shutdown signal. Fired by the SIGINT/SIGTERM
    /// handler before letting axum's graceful shutdown drain.
    /// Long-poll handlers (currently just the SSE stream) listen
    /// on this and end their futures promptly. Without it they
    /// would hold the serve loop open forever.
    pub shutdown: Arc<Notify>,
}

/// React UI bundle baked into the binary by `build.rs` + `rust-embed`.
///
/// The build script compiles the React/Vite app into
/// `$OUT_DIR/ui/dist/` and leaves the source directory untouched. That is
/// a hard crates.io requirement. `rust-embed`'s
/// `interpolate-folder-path` feature lets us reference `$OUT_DIR` in the
/// attribute below.
#[derive(RustEmbed)]
#[folder = "$OUT_DIR/ui/dist/"]
struct UiAssets;

// TODO(auth): validate the `Cf-Access-Jwt-Assertion` header on /ws before
// allowing the upgrade. The header is injected by Cloudflare Access; its
// signing keys are at
//   https://<team>.cloudflareaccess.com/cdn-cgi/access/certs
// What is enforced today lives in `guard.rs`: the `Host` allowlist and the
// `Origin` check, which stop a page in the user's own browser from reaching
// a loopback Mezame, and need no identity to do it.
//
// Not built, on purpose: an interim shared bearer token for non-loopback
// binds. The accounts phase replaces it wholesale with users and a signed
// session cookie; a static token shared by every device crosses a
// plain-HTTP LAN in the clear and, carried as a cookie, reaches every other
// service on the same host; and enabling it by default would lock every
// container deployment out on upgrade, since the container binds 0.0.0.0
// by design. If it is ever wanted before then, it belongs after the two
// checks in `guard_request`, compared in constant time, with a 401 that
// echoes nothing and a UI that stops reconnecting on it.

pub(crate) async fn run_cloudflared(
    cfg: Config,
    bind: String,
    hubs: HubRegistry,
    store: Arc<dyn Store>,
    keys: Keys,
    workspace_root: Option<PathBuf>,
) -> Result<()> {
    let (state_changes, _) = broadcast::channel(64);
    let shutdown = Arc::new(Notify::new());
    // The registry writes session rows and titles into the same store and
    // ticks on the same channel the routes do.
    let hubs = hubs.with_store(Arc::clone(&store), state_changes.clone());
    let state = Arc::new(AppState {
        config: ArcSwap::from_pointee(cfg),
        hubs,
        store,
        keys,
        limiter: RateLimiter::default(),
        clock: Arc::new(auth::now_unix),
        state_changes,
        workspace_root,
        shutdown: shutdown.clone(),
    });

    let app = build_router(state);

    let listener = TcpListener::bind(&bind).await?;
    enable_tcp_keepalive(&listener);
    eprintln!("Mezame is listening on: http://{bind}");
    serve(listener, app, shutdown_signal(shutdown)).await?;
    Ok(())
}

/// How long a connection may take to send a complete request head, and
/// how long a keep-alive connection may sit idle between requests: 30
/// seconds, hyper's own default.
///
/// `axum::serve` builds hyper with no timer, and hyper drops a timeout it
/// has no timer for, so that default was silently disabled: a connection
/// that opened and sent nothing, or finished a request and went quiet,
/// was held for as long as the peer liked, one descriptor each. hyper
/// re-arms this timer for every request head on a kept-alive connection,
/// which is what makes it the idle limit too. It runs only while a head
/// is awaited, so an upgraded WebSocket and a streaming SSE response are
/// untouched.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the accept loop waits after an accept error that is not a
/// peer's own reset before trying again. Running out of descriptors is
/// the error that matters, and it clears only as connections close.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Serve `app` on `listener` until `shutdown` resolves, then stop
/// accepting, ask every open connection to finish, and return once they
/// have.
///
/// Owns the accept loop rather than handing it to `axum::serve`, for one
/// reason: to set a timer on hyper so [`HEADER_READ_TIMEOUT`] applies.
/// The shape is axum's own (a watch channel closed by the shutdown, a
/// second one whose receivers count the open connections), on hyper's
/// HTTP/1 builder, which is the only protocol this build speaks.
/// `with_upgrades` is what lets `/ws` take the connection over.
///
/// An accept error is reported on stderr once per error kind, so a
/// descriptor limit shows up in the log instead of stalling the loop in
/// silence. Errors on a single connection (a request head that never
/// completed, a scanner sending garbage) are not logged.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    serve_with(listener, app, HEADER_READ_TIMEOUT, shutdown).await
}

/// [`serve`] with an explicit header-read timeout, so a test can drive
/// the timeout in milliseconds.
#[doc(hidden)]
pub async fn serve_with(
    listener: TcpListener,
    app: Router,
    header_read_timeout: Duration,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);

    // The shutdown drops `signal_rx`; every `signal_tx.closed()` then
    // resolves, in the accept loop and in each connection task.
    let (signal_tx, signal_rx) = watch::channel(());
    let signal_tx = Arc::new(signal_tx);
    tokio::spawn(async move {
        shutdown.await;
        drop(signal_rx);
    });
    // Each connection task holds a `close_rx`; `close_tx.closed()` resolves
    // once the last of them has dropped it.
    let (close_tx, close_rx) = watch::channel(());
    let mut noted: HashSet<io::ErrorKind> = HashSet::new();

    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => stream,
                Err(e) => {
                    // Through `warn`, never `eprintln!`: this loop is the
                    // root future of `block_on`, and a panicking write to
                    // a broken stderr here would end the process and
                    // every live session with it.
                    if noted.insert(e.kind()) {
                        warn(&format!(
                            "Could not accept a connection: {e}. If this names too many \
                             open files, raise the process's descriptor limit; see \
                             docs/service.md."
                        ));
                    }
                    if !is_connection_error(&e) {
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                    continue;
                }
            },
            _ = signal_tx.closed() => break,
        };

        let service = TowerToHyperService::new(app.clone());
        let http = http.clone();
        let signal_tx = Arc::clone(&signal_tx);
        let close_rx = close_rx.clone();
        tokio::spawn(async move {
            let conn = http
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            let mut conn = pin!(conn);
            let mut closed = pin!(signal_tx.closed().fuse());
            loop {
                tokio::select! {
                    // A finished connection, whichever way it ended.
                    _ = conn.as_mut() => break,
                    _ = &mut closed => conn.as_mut().graceful_shutdown(),
                }
            }
            drop(close_rx);
        });
    }

    drop(close_rx);
    drop(listener);
    // Every connection task holds a `close_rx`; the drain ends when the
    // last one drops. A peer that has stopped reading holds its
    // connection open until its TCP dies, and a streaming response to it
    // is never polled again, so the wait is bounded: after
    // [`DRAIN_TIMEOUT`] the process exits with whatever is still open.
    if tokio::time::timeout(DRAIN_TIMEOUT, close_tx.closed())
        .await
        .is_err()
    {
        warn(&format!(
            "Some connections did not close within {} seconds of the shutdown signal; exiting \
             with them open.",
            DRAIN_TIMEOUT.as_secs()
        ));
    }
    Ok(())
}

/// How long the shutdown waits for open connections to finish before the
/// process exits regardless. Long enough for every browser to see its
/// close frame and for a turn's last frames to go out, short enough that
/// a service manager's stop never hangs on one dead peer.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// An accept error the peer caused, which needs no pause before the next
/// accept.
fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// Enable TCP keepalive on the listening socket as a kernel-level
/// backstop to the application heartbeat in `src/ws.rs`. Accepted
/// connections inherit the listener's keepalive setting on Linux. A
/// half-open socket the kernel can detect (no ACKs for the probes) is
/// eventually torn down even with the app-level ping task wedged. The app
/// heartbeat is the primary defence against a peer that has stopped
/// sending; a peer that has stopped reading is caught by the bounded
/// outbound queue and the write timeout in `src/ws.rs`. This keeps the
/// kernel from holding a truly dead socket `ESTABLISHED` forever.
/// Best-effort: a failure here is logged and ignored, and startup
/// continues. See GitHub issue #4.
///
/// Public for the integration tests in `tests/`. Its only caller is
/// `run_cloudflared`, which serves until a signal arrives.
pub fn enable_tcp_keepalive(listener: &TcpListener) {
    use socket2::{SockRef, TcpKeepalive};

    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(20));
    let sock = SockRef::from(listener);
    if let Err(e) = sock.set_tcp_keepalive(&keepalive) {
        eprintln!("Could not enable TCP keepalive on the listener: {e}");
    }
}

/// Construct the axum router with all production routes wired in. Split
/// out from `run_cloudflared` so integration tests can drive it via
/// `tower::ServiceExt::oneshot` without binding a TCP port.
pub fn build_router(state: Arc<AppState>) -> Router {
    // Two routers. The public one holds the UI shell and its assets, the
    // login, `/me` (which answers 401 as data) and `/ws` (which completes
    // the handshake and then closes 4401, since a browser cannot read the
    // status of a refused upgrade). Everything else sits behind
    // `require_user`. Outermost, the `Host` allowlist and the `Origin` /
    // `Sec-Fetch-Site` check cover both; the policy is read from the
    // config once, here.
    let policy = Arc::new(RequestPolicy::from_config(&state.config.load()));
    let public = Router::new()
        .route("/ws", get(ws_upgrade))
        // The login body is small by construction: a name of at most 64
        // characters and a password of at most 1024 bytes. Bounding the
        // body here keeps a name that never reaches an account from
        // costing megabytes to read.
        .route(
            "/login",
            post(login).layer(DefaultBodyLimit::max(LOGIN_BODY_LIMIT)),
        )
        .route("/me", get(me))
        // SPA fallback: /, /assets/*, and any unknown path resolve against
        // the embedded UI bundle, with index.html as the fallback for
        // client-side routes.
        .fallback(get(serve_ui_asset));
    let protected = Router::new()
        .route("/logout", post(logout))
        .route("/state", get(get_state).put(put_state))
        .route("/state/events", get(state_events))
        .route("/history", get(get_history))
        .route("/sessions/:id", patch(patch_session).delete(delete_session))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_user));

    public
        .merge(protected)
        .layer(middleware::from_fn_with_state(policy, guard_request))
        .with_state(state)
}

/// The body every route behind the login answers with when there is no
/// valid cookie.
pub const LOGIN_REQUIRED: &str = "login required\n";

/// The most bytes a `/login` body may hold: room for the longest name and
/// password the rules accept, and the JSON around them, and no more.
pub const LOGIN_BODY_LIMIT: usize = 4096;

impl AppState {
    /// A state over an in-memory store and a fixed key, for the suite,
    /// with no workspace root.
    #[doc(hidden)]
    pub fn for_test(config: Config, hubs: HubRegistry, capacity: usize) -> Arc<Self> {
        Self::for_test_with(config, hubs, capacity, None, None)
    }

    /// [`AppState::for_test`] over `store` when given (else a fresh
    /// in-memory one) and with `workspace_root`. `capacity` is the tick
    /// channel's. The registry is wired to the store and the channel the
    /// way the server wires it.
    #[doc(hidden)]
    pub fn for_test_with(
        config: Config,
        hubs: HubRegistry,
        capacity: usize,
        store: Option<Arc<dyn Store>>,
        workspace_root: Option<PathBuf>,
    ) -> Arc<Self> {
        let keys = crate::store::crypto::MasterKey::from_bytes_for_test([42u8; 32]).keys();
        let store: Arc<dyn Store> = store.unwrap_or_else(|| {
            Arc::new(
                crate::store::sqlite::SqliteStore::open_in_memory(keys.clone())
                    .expect("an in-memory store opens"),
            )
        });
        let (state_changes, _) = broadcast::channel(capacity);
        let hubs = hubs.with_store(Arc::clone(&store), state_changes.clone());
        Arc::new(AppState {
            config: ArcSwap::from_pointee(config),
            hubs,
            store,
            keys,
            limiter: RateLimiter::default(),
            clock: Arc::new(auth::now_unix),
            state_changes,
            workspace_root,
            shutdown: Arc::new(Notify::new()),
        })
    }

    /// The clock in milliseconds, the unit the store's timestamps take.
    pub fn now_ms(&self) -> i64 {
        (self.clock)() * 1000
    }

    /// The session `id` when `user` owns it. `Ok(None)` for no row and for
    /// another user's row alike, so a caller answers both the same way.
    async fn owned_session(
        &self,
        user: &UserRow,
        id: &str,
    ) -> std::result::Result<Option<SessionRow>, StoreError> {
        Ok(self
            .store
            .session(id)
            .await?
            .filter(|row| row.user_id == user.id))
    }

    /// Fire a `state_changed` tick for `user_id`. A send error only means
    /// no browser is subscribed; the next one fetches `/state` on connect.
    fn tick(&self, user_id: &str) {
        let _ = self.state_changes.send(user_id.to_string());
    }

    /// Whether a cookie set on this request is marked `Secure`: the request
    /// arrived over TLS at a proxy, or the configured public URL is HTTPS.
    /// Never inferred from the bind address, so plain-HTTP LAN access keeps
    /// working.
    pub fn secure_cookies(&self, headers: &HeaderMap) -> bool {
        let forwarded_https = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|p| p.trim().eq_ignore_ascii_case("https")));
        forwarded_https
            || self
                .config
                .load()
                .public_url
                .as_deref()
                .is_some_and(|u| u.starts_with("https://"))
    }

    /// The user a request's cookie names, when the cookie verifies, the
    /// user exists and the epoch matches: `Ok(None)` for every way the
    /// cookie can fail, `Err` when the store could not answer, which the
    /// caller reports as a failure of its own rather than as a signed-out
    /// user.
    pub async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> std::result::Result<Option<(UserRow, Cookie)>, StoreError> {
        let now = (self.clock)();
        let Some(value) = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(cookie_value)
        else {
            return Ok(None);
        };
        let Some(cookie) = verify(value, &self.keys.cookie, now) else {
            return Ok(None);
        };
        let Some(user) = self.store.user_by_id(&cookie.user_id).await? else {
            return Ok(None);
        };
        if user.session_epoch != cookie.epoch {
            return Ok(None);
        }
        Ok(Some((user, cookie)))
    }

    /// Create `name` with `password` and sign a cookie for them, for the
    /// suite: the `Cookie` header value a request carries.
    #[doc(hidden)]
    pub async fn login_for_test(&self, name: &str, password: &str) -> String {
        let user = match self.store.user_by_name(name).await.expect("store") {
            Some(user) => user,
            None => {
                let hash = auth::hash_password(password).expect("a valid password");
                self.store
                    .create_user(name, &hash, crate::store::Role::User, (self.clock)() * 1000)
                    .await
                    .expect("create the test user")
            }
        };
        let cookie = Cookie::issue(&user.id, user.session_epoch, (self.clock)());
        format!("{}={}", auth::COOKIE_NAME, sign(&cookie, &self.keys.cookie))
    }
}

/// The layer every route behind the login runs under: a valid cookie
/// becomes an `AuthUser` in the request's extensions and is re-issued on
/// the response when under thirty days remain; anything else is 401, and
/// a store that could not answer is 500, never a sign-out.
async fn require_user(State(app): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let (user, cookie) = match app.current_user(req.headers()).await {
        Ok(Some(found)) => found,
        Ok(None) => return (StatusCode::UNAUTHORIZED, LOGIN_REQUIRED).into_response(),
        Err(e) => return internal(e).into_response(),
    };
    let now = (app.clock)();
    let renew = cookie.due_for_renewal(now).then(|| {
        set_cookie_header(
            &sign(
                &Cookie::issue(&user.id, user.session_epoch, now),
                &app.keys.cookie,
            ),
            app.secure_cookies(req.headers()),
        )
    });
    req.extensions_mut().insert(AuthUser(user));
    let mut res = next.run(req).await;
    // A handler that set the cookie itself (the logout, clearing it) has
    // the last word: a renewal appended after it would be the later
    // header for the same name, and the browser would keep the session
    // the handler just ended.
    if let Some(header) = renew {
        if !res.headers().contains_key(header::SET_COOKIE) {
            if let Ok(value) = HeaderValue::from_str(&header) {
                res.headers_mut().append(header::SET_COOKIE, value);
            }
        }
    }
    res
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

fn user_json(user: &UserRow) -> Value {
    json!({ "id": user.id, "name": user.name, "role": user.role.as_str() })
}

/// `POST /login`. The limiter answers first; an unknown name is verified
/// against the dummy hash so the 401 costs one argon2 run either way; the
/// body of every 401 is the same.
async fn login(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<LoginBody>, JsonRejection>,
) -> Response {
    let Json(LoginBody { username, password }) = match body {
        Ok(body) => body,
        // A body past the limit is answered as axum answers it, 413; every
        // other way the body can fail to be the login JSON is one 400.
        Err(JsonRejection::BytesRejection(too_large)) => return too_large.into_response(),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "expected a JSON body with `username` and `password`\n",
            )
                .into_response();
        }
    };
    let name = username.trim();
    // A name no account can have is refused before it reaches the
    // limiter: the limiter's cap bounds how many names it holds, not how
    // long they are, and the body limit alone would let each held name
    // run to kilobytes. The refusal costs the same verification as a
    // wrong password against an unknown name.
    if name.chars().count() > USER_NAME_MAX_CHARS {
        verify_password(&password, dummy_hash());
        return (StatusCode::UNAUTHORIZED, "wrong username or password\n").into_response();
    }
    if let Err(left) = app
        .limiter
        .check(&format!("u:{name}"), std::time::Instant::now())
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                header::RETRY_AFTER,
                HeaderValue::from(left.as_secs().max(1)),
            )],
            "too many login attempts; try again shortly\n",
        )
            .into_response();
    }
    // A store that cannot answer is a failure of the server, reported as
    // one; treating it as an unknown name would sign every browser out
    // and tell the next login its password was wrong.
    let stored = match app.store.password_hash_of(name).await {
        Ok(stored) => stored,
        Err(e) => return internal(e).into_response(),
    };
    let user = match app.store.user_by_name(name).await {
        Ok(user) => user,
        Err(e) => return internal(e).into_response(),
    };
    let hash: &str = match stored.as_deref() {
        Some(hash) => hash,
        None => dummy_hash(),
    };
    let verified = verify_password(&password, hash);
    let (Some(user), true) = (user, verified) else {
        return (StatusCode::UNAUTHORIZED, "wrong username or password\n").into_response();
    };
    let now = (app.clock)();
    let cookie = Cookie::issue(&user.id, user.session_epoch, now);
    let header = set_cookie_header(
        &sign(&cookie, &app.keys.cookie),
        app.secure_cookies(&headers),
    );
    let mut res = Json(user_json(&user)).into_response();
    if let Ok(value) = HeaderValue::from_str(&header) {
        res.headers_mut().insert(header::SET_COOKIE, value);
    }
    res
}

/// `POST /logout`: clears the cookie on this device and nothing else.
async fn logout() -> Response {
    let mut res = StatusCode::NO_CONTENT.into_response();
    if let Ok(value) = HeaderValue::from_str(&clear_cookie_header()) {
        res.headers_mut().insert(header::SET_COOKIE, value);
    }
    res
}

/// `GET /me`: who the cookie says, or 401. Public, so the browser can ask
/// without the answer being a refusal.
async fn me(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match app.current_user(&headers).await {
        Ok(Some((user, _))) => Json(user_json(&user)).into_response(),
        Ok(None) => (StatusCode::UNAUTHORIZED, LOGIN_REQUIRED).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

/// Resolve when the process receives SIGINT (Ctrl+C) or SIGTERM (systemd
/// / launchd `stop`). `serve` stops accepting new connections when the
/// returned future resolves. Mezame exits promptly when its
/// service manager asks it to.
///
/// Before returning we fire `shutdown`. Long-poll handlers in flight (the
/// SSE state-events stream) end their futures, and axum's graceful drain
/// completes. Without it the drain waits on them forever.
///
/// Live WebSocket sessions are dropped on shutdown. Each hub's Backend is
/// released with it, and its transcript goes: nothing on disk survives a
/// restart in this phase.
async fn shutdown_signal(shutdown: Arc<Notify>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                eprintln!("Failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => warn("\nReceived SIGINT, shutting down."),
        _ = terminate => warn("Received SIGTERM, shutting down."),
    }
    // Wake every long-poll handler. They release their futures before
    // axum's drain kicks in. `notify_waiters` wakes only a waiter that is
    // registered, which is why each event stream registers its one
    // notification future when it opens and keeps it; a stream opened
    // after this point is on a connection the drain is already closing.
    shutdown.notify_waiters();
}

/// Serve a single file from the embedded UI bundle.
///
/// Strips the leading `/` and falls back to `index.html` for empty paths
/// and for any unknown path, leaving the SPA to handle its own routing.
/// Sets a reasonable Cache-Control: long-lived for the hashed `/assets/*`
/// filenames Vite emits, no-cache for `index.html`.
async fn serve_ui_asset(uri: Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');
    // Resolve to an actual asset. `/` and unknown routes both fall back to
    // `index.html`, and the SPA handles its own routing from there.
    let (asset, resolved_path) = match UiAssets::get(raw_path) {
        Some(a) => (a, raw_path),
        None => match UiAssets::get("index.html") {
            Some(a) => (a, "index.html"),
            None => {
                return (StatusCode::NOT_FOUND, "UI bundle missing").into_response();
            }
        },
    };
    let is_index = resolved_path == "index.html";

    let mime = mime_for(resolved_path);
    let cache_control = if is_index || resolved_path == "sw.js" {
        // Neither `index.html` nor the service-worker script tolerates
        // aggressive caching. `index.html` is the SPA entry point, and
        // `sw.js` is how the SW updates itself. Browsers already bypass
        // the HTTP cache for SW updates in most cases; the explicit
        // no-cache keeps any intermediary from stashing it.
        "no-cache, no-store, must-revalidate"
    } else if resolved_path.starts_with("assets/") {
        // Vite emits content-hashed filenames under /assets. A year of
        // caching cannot serve stale content.
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        )
        .body(Body::from(asset.data.into_owned()))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "response build failed").into_response()
        })
}

/// Tiny mime-type lookup for the handful of extensions Vite emits. Keeps us
/// off a `mime_guess` dependency. The const table is the single source of
/// truth; matching is case-insensitive without allocating a lowercase copy
/// of the extension on every request.
const MIME_TABLE: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("js", "application/javascript; charset=utf-8"),
    ("mjs", "application/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("json", "application/json; charset=utf-8"),
    ("map", "application/json; charset=utf-8"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("txt", "text/plain; charset=utf-8"),
    ("webmanifest", "application/manifest+json"),
];

pub fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    MIME_TABLE
        .iter()
        .find(|(k, _)| ext.eq_ignore_ascii_case(k))
        .map(|(_, v)| *v)
        .unwrap_or("application/octet-stream")
}

/// The most bytes a user's settings object may take, serialised.
pub const SETTINGS_MAX_BYTES: usize = 16 * 1024;
/// The longest title a rename may set, in characters, after trimming.
pub const SESSION_TITLE_MAX_CHARS: usize = 200;

/// The body every session route answers for a row the user does not own
/// or that does not exist: one answer, so neither says which.
const NO_SUCH_SESSION: &str = "no such session\n";

/// `GET /state`: the user's open sessions by creation, their newest twenty
/// archived ones by archival, and their settings object. `title` is null
/// for an untitled session. Mezame does not interpret the settings.
async fn get_state(
    State(app): State<Arc<AppState>>,
    Extension(AuthUser(user)): Extension<AuthUser>,
) -> Response {
    match app.store.list_sessions(&user.id).await {
        Ok(list) => Json(state_json(&list, &user.settings)).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

/// The `/state` document for `list` and `settings`.
pub fn state_json(list: &SessionList, settings: &Value) -> Value {
    let sessions: Vec<Value> = list
        .active
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "title": row.title,
                "created": row.created,
                "updated": row.updated,
            })
        })
        .collect();
    let closed: Vec<Value> = list
        .archived
        .iter()
        .map(|row| json!({ "id": row.id, "title": row.title, "closedAt": row.archived_at }))
        .collect();
    json!({ "sessions": sessions, "closed": closed, "settings": settings })
}

/// `PUT /state`: replace the user's settings with the object under
/// `settings` in a body of exactly that one key, at most
/// [`SETTINGS_MAX_BYTES`] serialised. 204 and one tick for the user; any
/// other body is 400 and changes nothing.
async fn put_state(
    State(app): State<Arc<AppState>>,
    Extension(AuthUser(user)): Extension<AuthUser>,
    body: Bytes,
) -> Response {
    let Some(settings) = settings_of_body(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "the body is {{\"settings\": {{...}}}}, an object of at most {SETTINGS_MAX_BYTES} \
                 bytes\n"
            ),
        )
            .into_response();
    };
    if let Err(e) = app.store.set_settings(&user.id, &settings).await {
        return internal(e).into_response();
    }
    app.tick(&user.id);
    StatusCode::NO_CONTENT.into_response()
}

/// The settings object a `PUT /state` body carries, when it is the one
/// shape and size the route takes.
pub fn settings_of_body(body: &[u8]) -> Option<Value> {
    let document: Value = serde_json::from_slice(body).ok()?;
    let object = document.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let settings = object.get("settings")?;
    settings.as_object()?;
    let size = serde_json::to_vec(settings).ok()?.len();
    (size <= SETTINGS_MAX_BYTES).then(|| settings.clone())
}

/// What a `PATCH /sessions/{id}` body asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPatch {
    /// A rename, trimmed, 1 to [`SESSION_TITLE_MAX_CHARS`] characters.
    Title(String),
    /// Close (`true`) or restore (`false`).
    Archived(bool),
}

/// The change a `PATCH /sessions/{id}` body asks for, when it is one of
/// the two shapes the route takes and nothing else.
pub fn session_patch_of(body: &[u8]) -> Option<SessionPatch> {
    let document: Value = serde_json::from_slice(body).ok()?;
    let object = document.as_object()?;
    if object.len() != 1 {
        return None;
    }
    if let Some(title) = object.get("title") {
        let title = title.as_str()?.trim();
        let length = title.chars().count();
        return (1..=SESSION_TITLE_MAX_CHARS)
            .contains(&length)
            .then(|| SessionPatch::Title(title.to_string()));
    }
    object
        .get("archived")?
        .as_bool()
        .map(SessionPatch::Archived)
}

/// `PATCH /sessions/{id}`: rename, close or restore one of the user's
/// sessions. A close also ends the hub registered under the id, so every
/// attached socket is closed with 4404 and a later upgrade naming the id
/// is refused until the row is restored. 204 and one tick for the owner;
/// 404 for a row the user does not own or that does not exist; 400 for a
/// body of another shape.
async fn patch_session(
    State(app): State<Arc<AppState>>,
    Extension(AuthUser(user)): Extension<AuthUser>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    match app.owned_session(&user, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, NO_SUCH_SESSION).into_response(),
        Err(e) => return internal(e).into_response(),
    }
    let Some(patch) = session_patch_of(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "the body is {{\"title\": \"...\"}} (1 to {SESSION_TITLE_MAX_CHARS} characters) or \
                 {{\"archived\": true|false}}\n"
            ),
        )
            .into_response();
    };
    let now = app.now_ms();
    let outcome = match patch {
        SessionPatch::Title(title) => app.store.set_title(&id, &title, now).await,
        SessionPatch::Archived(archived) => {
            let outcome = app.store.set_archived(&id, archived, now).await;
            if outcome.is_ok() && archived {
                app.hubs.close_session(&id).await;
            }
            outcome
        }
    };
    if let Err(e) = outcome {
        return internal(e).into_response();
    }
    app.tick(&user.id);
    StatusCode::NO_CONTENT.into_response()
}

/// `DELETE /sessions/{id}`: forget one of the user's sessions and, through
/// the cascade, its messages. A hub registered under the id is closed the
/// way an archive closes it, since it has no row to serve. 204 and one
/// tick for the owner; 404 for a row the user does not own or that does
/// not exist.
async fn delete_session(
    State(app): State<Arc<AppState>>,
    Extension(AuthUser(user)): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Response {
    match app.owned_session(&user, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, NO_SUCH_SESSION).into_response(),
        Err(e) => return internal(e).into_response(),
    }
    if let Err(e) = app.store.delete_session(&id).await {
        return internal(e).into_response();
    }
    app.hubs.close_session(&id).await;
    app.tick(&user.id);
    StatusCode::NO_CONTENT.into_response()
}

/// A handler failure the client sees as a 500 with the error's text. The
/// first one per process is also written to the log, so an operator
/// reading it finds the datastore named as the cause; the rest are the
/// client's to see, since a wedged store would otherwise fill the log at
/// request rate.
fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        warn(&format!(
            "A request failed on the datastore: {e}. Later failures are answered 500 and not \
             reported here."
        ));
    }
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}\n"))
}

/// GET /state/events: Server-Sent Events stream. Emits one
/// `state_changed` event each time this user's sessions or settings
/// change, on any device; ticks for other users are not forwarded. The
/// browser reads it as a "go refetch /state" signal, and a session opened
/// in another browser shows up without a manual reload.
///
/// A periodic keep-alive comment goes out alongside. A Cloudflare Tunnel
/// or other intermediary would otherwise idle-timeout the stream during a
/// quiet period.
async fn state_events(
    State(app): State<Arc<AppState>>,
    Extension(AuthUser(user)): Extension<AuthUser>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = app.state_changes.subscribe();
    let me = user.id;
    // One notification future for the life of the stream, registered
    // now: `notify_waiters` wakes only a waiter already registered, and a
    // future made fresh on each poll of the stream would miss a shutdown
    // that fired while the stream sat between polls, which is where a
    // stream whose peer has stopped reading sits. Polled once here so its
    // registration exists before the first event is awaited; the
    // registration survives the wakes that follow.
    let shutdown = app.shutdown.clone();
    let mut notified: futures_util::future::BoxFuture<'static, ()> =
        Box::pin(async move { shutdown.notified().await });
    let _ = std::future::poll_fn(|cx| {
        let _ = notified.as_mut().poll(cx);
        std::task::Poll::Ready(())
    })
    .await;
    let stream = futures_util::stream::unfold(
        (rx, notified, me),
        |(mut rx, mut notified, me)| async move {
            loop {
                tokio::select! {
                    // Shutdown wins: end the stream and let axum's
                    // graceful drain finish. Without this the SSE handler
                    // holds a request future that never resolves, and
                    // Ctrl+C hangs.
                    () = &mut notified => return None,
                    msg = rx.recv() => match msg {
                        Ok(user_id) if user_id == me => {
                            return Some((
                                Ok(Event::default().event("state_changed").data("")),
                                (rx, notified, me),
                            ));
                        }
                        // Another user's state moved: nothing for this stream.
                        Ok(_) => continue,

                        // Lagged: skip and wait for the next message. The
                        // browser refetches on the next event delivered.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                            // All senders dropped: end the stream. In practice
                        // this only happens when the server is shutting down.
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                }
            }
        },
    );
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

/// `GET /history?session=<id>`: the transcript of the Backend behind that
/// session id, as an `entries` array holding every entry the Backend
/// retains, in recorded order, with no pagination. The shipped Backend
/// bounds what it retains at `TRANSCRIPT_BUDGET_BYTES` of entry text and
/// `TRANSCRIPT_MAX_ENTRIES` entries (`backend.rs`), the oldest turn
/// evicted first, so a long conversation comes back as its newest window.
///
/// An absent or empty `session` answers 400 with a plain-text body. A
/// session the user does not own, or that does not exist, answers 404
/// with an empty `entries` array, the same answer in both cases, so the
/// route says nothing about rows that are not theirs. An owned session
/// with no registered hub answers 200 with an empty array.
///
/// A transcript lives as long as its hub. A reload inside the grace window
/// shows the conversation so far; one after it shows an empty log.
async fn get_history(
    Query(params): Query<HashMap<String, String>>,
    State(app): State<Arc<AppState>>,
    Extension(AuthUser(user)): Extension<AuthUser>,
) -> Response {
    let sid = params.get("session").map(String::as_str).unwrap_or("");
    if sid.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing ?session=<id>").into_response();
    }
    match app.owned_session(&user, sid).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "entries": [] }))).into_response();
        }
        Err(e) => return internal(e).into_response(),
    }
    // A persisting deployment serves the store's rows through the same
    // window and the same rebuild a hub makes, so the browser shows what
    // the hub holds whether or not one is live; the echo keeps its
    // transcript in the hub and is served from there.
    let entries = if app.hubs.persists() {
        let window = match app
            .store
            .load_window(sid, TRANSCRIPT_MAX_ENTRIES, 2 * TRANSCRIPT_BUDGET_BYTES)
            .await
        {
            Ok(window) => window,
            Err(e) => return internal(e).into_response(),
        };
        let mut conversation = Conversation::new();
        conversation.restore(window);
        conversation.history()
    } else {
        app.hubs.history(sid).await.unwrap_or_default()
    };
    Json(json!({ "entries": entries })).into_response()
}
