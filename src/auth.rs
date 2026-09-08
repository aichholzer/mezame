//! Identity: password hashes, the session cookie, and the login limiter.
//!
//! Passwords are hashed with argon2id at the `argon2` crate's defaults into
//! a PHC string and verified by parsing that string, so a later parameter
//! change verifies old hashes unchanged. An unknown user is verified
//! against a fixed dummy hash, so a wrong name and a wrong password cost
//! the same.
//!
//! The session cookie is `<user_id>.<expiry>.<epoch>.<mac>`: the MAC is
//! HMAC-SHA256 under the cookie sub-key of the first three parts, rendered
//! base64url without padding and decoded strictly, so exactly one string
//! verifies for a given key and payload. It is `HttpOnly`, `SameSite=Lax`,
//! `Path=/`, lives 90 days, and is re-issued by the auth layer when under
//! 30 days remain. The epoch is compared by the layer, which has the user
//! row; a password change bumps it and every earlier cookie is refused.
//!
//! The limiter is a fixed 60-second window of ten attempts per username,
//! in memory, capped at 4,096 keys. There is no per-peer window: behind a
//! tunnel every request shares one address, and a forwarded-for header is
//! forgeable on any bind that is not loopback.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::store::crypto::KEY_LEN;

type HmacSha256 = Hmac<Sha256>;

/// The cookie's name.
pub const COOKIE_NAME: &str = "mezame_session";
/// How long a cookie lives from issue.
pub const COOKIE_LIFETIME: Duration = Duration::from_secs(90 * 24 * 60 * 60);
/// A cookie with less than this left is re-issued on the response.
pub const COOKIE_RENEW_WITHIN: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// The shortest password accepted, in characters.
pub const MIN_PASSWORD_CHARS: usize = 8;
/// The longest password accepted, in bytes.
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// Login attempts allowed per username in one window.
pub const LOGIN_LIMIT: u32 = 10;
/// The login window.
pub const LOGIN_WINDOW: Duration = Duration::from_secs(60);
/// The most limiter keys held at once.
pub const LIMITER_CAP: usize = 4096;

/// Why a password was refused or a hash could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    TooShort,
    TooLong,
    Hash(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::TooShort => {
                write!(f, "a password is at least {MIN_PASSWORD_CHARS} characters")
            }
            AuthError::TooLong => write!(f, "a password is at most {MAX_PASSWORD_BYTES} bytes"),
            AuthError::Hash(why) => write!(f, "could not hash the password: {why}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Refuse a password outside the length rules.
pub fn check_password(password: &str) -> Result<(), AuthError> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(AuthError::TooShort);
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(AuthError::TooLong);
    }
    Ok(())
}

/// Hash a password to a PHC string with a fresh salt.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    check_password(password)?;
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

thread_local! {
    static VERIFY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether `password` is the one `phc` was made from. A string that is not
/// a PHC hash verifies nothing. Every call is counted on the calling thread,
/// so a test can assert that a login runs exactly one verification whatever
/// the outcome while other tests run beside it.
pub fn verify_password(password: &str, phc: &str) -> bool {
    VERIFY_CALLS.with(|calls| calls.set(calls.get() + 1));
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// How many verifications have run on this thread.
#[doc(hidden)]
pub fn verify_calls_for_test() -> usize {
    VERIFY_CALLS.with(|calls| calls.get())
}

/// A real hash of a random password nobody holds, verified against when the
/// username is unknown so the 401 costs one argon2 run either way. Made
/// once per process, on first use.
pub fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("OS entropy source");
        let password = URL_SAFE_NO_PAD.encode(bytes);
        hash_password(&password).expect("a 43-character password hashes")
    })
}

/// What a session cookie says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub user_id: String,
    /// Unix seconds.
    pub expiry: i64,
    pub epoch: u64,
}

impl Cookie {
    /// A cookie for `user_id` issued at `now`.
    pub fn issue(user_id: &str, epoch: u64, now: i64) -> Self {
        Self {
            user_id: user_id.to_string(),
            expiry: now + COOKIE_LIFETIME.as_secs() as i64,
            epoch,
        }
    }

    /// Whether the auth layer re-issues it on a response at `now`.
    pub fn due_for_renewal(&self, now: i64) -> bool {
        self.expiry - now < COOKIE_RENEW_WITHIN.as_secs() as i64
    }

    fn payload(&self) -> String {
        format!("{}.{}.{}", self.user_id, self.expiry, self.epoch)
    }
}

fn mac_of(payload: &str, key: &[u8; KEY_LEN]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any size");
    mac.update(payload.as_bytes());
    mac
}

/// The cookie's value: the payload and its MAC.
pub fn sign(cookie: &Cookie, key: &[u8; KEY_LEN]) -> String {
    let payload = cookie.payload();
    let mac = mac_of(&payload, key).finalize().into_bytes();
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(mac))
}

/// The cookie a value carries, when its shape, its MAC and its expiry all
/// hold at `now`. The epoch is the caller's to compare.
pub fn verify(value: &str, key: &[u8; KEY_LEN], now: i64) -> Option<Cookie> {
    let parts: Vec<&str> = value.split('.').collect();
    let [user_id, expiry, epoch, mac] = parts.as_slice() else {
        return None;
    };
    if !crate::ws::is_session_id(user_id) {
        return None;
    }
    let expiry: i64 = expiry.parse().ok()?;
    let epoch: u64 = epoch.parse().ok()?;
    // The strict decoder is what makes one string alone verify: padding, a
    // byte outside the alphabet and a final character with trailing bits
    // set are each refused.
    let tag = URL_SAFE_NO_PAD.decode(mac).ok()?;
    let payload = format!("{user_id}.{expiry}.{epoch}");
    mac_of(&payload, key).verify_slice(&tag).ok()?;
    if expiry <= now {
        return None;
    }
    Some(Cookie {
        user_id: user_id.to_string(),
        expiry,
        epoch,
    })
}

/// The `Set-Cookie` header that installs `value`.
pub fn set_cookie_header(value: &str, secure: bool) -> String {
    let mut header = format!(
        "{COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        COOKIE_LIFETIME.as_secs()
    );
    if secure {
        header.push_str("; Secure");
    }
    header
}

/// The `Set-Cookie` header that clears the cookie.
pub fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// The session cookie's value in a `Cookie` request header, if present.
pub fn cookie_value(header: &str) -> Option<&str> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name.trim() == COOKIE_NAME).then(|| value.trim())
    })
}

/// Unix seconds now.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    count: u32,
}

/// A fixed-window counter over login attempts.
#[derive(Debug)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, Window>>,
    limit: u32,
    window: Duration,
    cap: usize,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(LOGIN_LIMIT, LOGIN_WINDOW, LIMITER_CAP)
    }
}

impl RateLimiter {
    pub fn new(limit: u32, window: Duration, cap: usize) -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
            limit,
            window,
            cap,
        }
    }

    /// Count an attempt under `key` at `now`: `Ok` while the window holds
    /// fewer than the limit, else the time left in it.
    pub fn check(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(window) = windows.get_mut(key) {
            if now.duration_since(window.start) >= self.window {
                *window = Window {
                    start: now,
                    count: 0,
                };
            }
            if window.count >= self.limit {
                let left = self.window.saturating_sub(now.duration_since(window.start));
                return Err(left.max(Duration::from_secs(1)));
            }
            window.count += 1;
            return Ok(());
        }
        if windows.len() >= self.cap {
            // Evict the oldest window so a scan of names cannot grow the map.
            if let Some(oldest) = windows
                .iter()
                .min_by_key(|(_, w)| w.start)
                .map(|(k, _)| k.clone())
            {
                windows.remove(&oldest);
            }
        }
        windows.insert(
            key.to_string(),
            Window {
                start: now,
                count: 1,
            },
        );
        Ok(())
    }

    /// How many keys are held.
    pub fn len(&self) -> usize {
        self.windows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The authenticated user a request carries once the layer has run.
#[derive(Debug, Clone)]
pub struct AuthUser(pub crate::store::UserRow);
