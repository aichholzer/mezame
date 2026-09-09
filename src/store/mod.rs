//! Persistent state: the one interface over everything that outlives a
//! process, and the keys that protect the secrets in it.
//!
//! `Store` is object-safe and async (every operation returns a
//! [`StoreFuture`]), so a second backend is a second implementation and
//! nothing above the trait changes. The rows are plain structs; a `UserRow`
//! never carries the password hash, which reaches the login handler alone
//! through `password_hash_of`. Ids of every table but `messages` are 32
//! lowercase hexadecimal characters from 16 bytes of operating-system
//! entropy, the form session ids already take, so an id never reveals a
//! count and a rename touches no path.

pub mod crypto;
pub mod sqlite;

use std::fmt;
use std::path::Path;

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::conversation::Block;
use crate::provider::Usage;

/// What every Store operation resolves to.
pub type StoreFuture<'a, T> = BoxFuture<'a, Result<T, StoreError>>;

/// Why an operation failed. The text of a variant names the operation and
/// carries SQLite's own message, never a credential payload, a password, a
/// hash or a cookie value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The store has been dropped: a guard, not a behaviour (see the
    /// design), since the store owns its only sender.
    Closed,
    /// No row matched.
    NotFound,
    /// A unique name is taken.
    Conflict(String),
    /// A rule refused the input.
    Invalid(String),
    /// A stored credential could not be opened with this master key.
    Tampered,
    /// SQLite or the store thread failed; the operation is named.
    Internal(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Closed => f.write_str("the store is closed"),
            StoreError::NotFound => f.write_str("no such row"),
            StoreError::Conflict(what) => write!(f, "{what} is already taken"),
            StoreError::Invalid(why) => f.write_str(why),
            StoreError::Tampered => {
                f.write_str("the stored credential could not be opened with this master key")
            }
            StoreError::Internal(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for StoreError {}

/// Milliseconds since the Unix epoch now, the unit every timestamp column
/// takes.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Mint an id: 16 bytes of OS entropy rendered as 32 lowercase hexadecimal
/// characters. The panic on entropy failure is deliberate: on Unix it means
/// `getrandom(2)` failed, and continuing with a predictable id would be
/// worse than stopping.
pub fn new_id() -> String {
    use std::fmt::Write as _;

    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS entropy source");
    bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        // Writing into a String cannot fail.
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A user's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }
}

/// A `users` row, without the hash.
#[derive(Debug, Clone, PartialEq)]
pub struct UserRow {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub session_epoch: u64,
    pub settings: Value,
    pub created: i64,
}

/// A `workspaces` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub root: String,
}

/// A `sessions` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub user_id: String,
    pub workspace_id: Option<String>,
    pub profile_id: Option<String>,
    pub title: Option<String>,
    pub archived_at: Option<i64>,
    pub created: i64,
    pub updated: i64,
}

/// A user's sessions: the open ones by creation, the newest twenty archived
/// ones by archival.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionList {
    pub active: Vec<SessionRow>,
    pub archived: Vec<SessionRow>,
}

/// How many archived sessions a list carries.
pub const ARCHIVED_LIST_MAX: usize = 20;

/// The side a `messages` row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        }
    }
}

/// A `messages` row, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageRow {
    pub id: i64,
    pub session_id: String,
    pub role: MessageRole,
    pub blocks: Vec<Block>,
    /// The user entry's text as the loop recorded it (the typed text alone);
    /// `None` on an assistant row.
    pub text: Option<String>,
    pub usage: Option<Usage>,
    pub rejected: bool,
    pub created: i64,
}

/// The newest rows of a session, oldest first, bounded inside the query.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MessageWindow {
    pub rows: Vec<MessageRow>,
    /// Whether the session's oldest row is among `rows`.
    pub complete: bool,
}

/// How much a session holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageStats {
    pub count: u64,
    /// `SUM(LENGTH(content))` in bytes.
    pub bytes: u64,
}

/// A `credentials` row, without its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRow {
    pub id: String,
    pub user_id: Option<String>,
    pub provider: String,
    pub label: String,
    pub created: i64,
    pub updated: i64,
}

/// A `profiles` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub id: String,
    pub user_id: Option<String>,
    pub credential_id: Option<String>,
    pub model: String,
    pub thinking: Option<String>,
    pub thinking_budget: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

/// What `upsert_global_profile` writes. The three thinking fields stay
/// `None` in this release; the columns exist for phase 4's editor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewProfile {
    pub model: String,
    pub credential_id: Option<String>,
    pub thinking: Option<String>,
    pub thinking_budget: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

/// The longest user name accepted.
pub const USER_NAME_MAX_CHARS: usize = 64;

/// Everything the crate reads or writes that outlives a process.
pub trait Store: Send + Sync {
    /// `"sqlite"` for the one implementation.
    fn backend_name(&self) -> &'static str;
    /// A round trip to the backend.
    fn health(&self) -> StoreFuture<'_, ()>;

    fn create_user(
        &self,
        name: &str,
        password_hash: &str,
        role: Role,
        now: i64,
    ) -> StoreFuture<'_, UserRow>;
    fn user_by_name(&self, name: &str) -> StoreFuture<'_, Option<UserRow>>;
    fn user_by_id(&self, id: &str) -> StoreFuture<'_, Option<UserRow>>;
    fn list_users(&self) -> StoreFuture<'_, Vec<UserRow>>;
    fn count_users(&self) -> StoreFuture<'_, u64>;
    /// The stored PHC string, for the login handler alone.
    fn password_hash_of(&self, name: &str) -> StoreFuture<'_, Option<String>>;
    /// Sets the hash and bumps the session epoch in one statement.
    fn set_password_hash(&self, id: &str, hash: &str) -> StoreFuture<'_, ()>;
    /// Bumps the session epoch; returns the new value.
    fn bump_session_epoch(&self, id: &str) -> StoreFuture<'_, u64>;
    fn settings(&self, id: &str) -> StoreFuture<'_, Value>;
    fn set_settings(&self, id: &str, settings: &Value) -> StoreFuture<'_, ()>;

    fn default_workspace(&self, user_id: &str) -> StoreFuture<'_, Option<WorkspaceRow>>;

    /// Creates the row, and the user's default workspace at `workspace_root`
    /// when they have none and a root is given, in one transaction.
    fn create_session(
        &self,
        user_id: &str,
        id: &str,
        workspace_root: Option<&Path>,
        now: i64,
    ) -> StoreFuture<'_, SessionRow>;
    fn session(&self, id: &str) -> StoreFuture<'_, Option<SessionRow>>;
    fn list_sessions(&self, user_id: &str) -> StoreFuture<'_, SessionList>;
    fn set_title(&self, id: &str, title: &str, now: i64) -> StoreFuture<'_, ()>;
    /// Sets the title only while it is null; whether a row was written.
    fn set_title_if_null(&self, id: &str, title: &str, now: i64) -> StoreFuture<'_, bool>;
    fn set_archived(&self, id: &str, archived: bool, now: i64) -> StoreFuture<'_, ()>;
    fn delete_session(&self, id: &str) -> StoreFuture<'_, ()>;

    fn append_user(
        &self,
        session_id: &str,
        blocks: &[Block],
        text: &str,
        created: i64,
    ) -> StoreFuture<'_, i64>;
    fn append_assistant(
        &self,
        session_id: &str,
        blocks: &[Block],
        usage: Option<Usage>,
        rejected: bool,
        created: i64,
    ) -> StoreFuture<'_, i64>;
    fn mark_rejected(&self, message_ids: &[i64]) -> StoreFuture<'_, ()>;
    /// The newest rows within `max_rows` and `max_bytes` of stored content,
    /// bounded inside the query, oldest first.
    fn load_window(
        &self,
        session_id: &str,
        max_rows: usize,
        max_bytes: usize,
    ) -> StoreFuture<'_, MessageWindow>;
    fn message_stats(&self, session_id: &str) -> StoreFuture<'_, MessageStats>;

    /// Creates the credential and a grant row for `creator` in one
    /// transaction; `owner` `None` means global.
    fn create_credential(
        &self,
        owner: Option<&str>,
        creator: &str,
        provider: &str,
        label: &str,
        payload: &Value,
        now: i64,
    ) -> StoreFuture<'_, CredentialRow>;
    fn credentials(
        &self,
        owner: Option<&str>,
        provider: &str,
    ) -> StoreFuture<'_, Vec<CredentialRow>>;
    /// The one way a payload leaves the store.
    fn credential_payload(&self, id: &str) -> StoreFuture<'_, Value>;
    fn delete_credential(&self, id: &str) -> StoreFuture<'_, ()>;
    /// Every credential row this store's key cannot open, with the
    /// profiles that used them, in one transaction; the sessions those
    /// profiles served keep running on none. How many credentials went.
    /// For `init` beside a datastore whose key was replaced: nothing sealed
    /// under the old key can be opened, and a row that opens is left alone,
    /// so the call is safe to make on every run.
    fn drop_unopenable_credentials(&self) -> StoreFuture<'_, u64>;

    fn global_profile(&self) -> StoreFuture<'_, Option<ProfileRow>>;
    fn upsert_global_profile(&self, profile: &NewProfile) -> StoreFuture<'_, ProfileRow>;
}
