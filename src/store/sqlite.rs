//! The `Store` over SQLite.
//!
//! One operating-system thread owns the `rusqlite::Connection` and runs
//! every operation in order; the async side sends it boxed jobs over a
//! bounded channel and awaits a one-shot reply. No tokio worker ever holds
//! the connection, and SQLite's one-writer rule is kept by the one thread.
//! Each job runs under `catch_unwind`, so a panicking statement answers its
//! caller with an error and the thread survives. Dropping the store closes
//! the queue and then joins the thread, so the loop runs what is queued
//! and the connection closes before the drop returns; a job whose caller
//! stopped waiting for its answer still runs.
//!
//! Migrations are numbered SQL files embedded with `include_str!` and
//! applied forward-only against `PRAGMA user_version`, each inside one
//! transaction. Nothing outside this module names `rusqlite`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::BoxFuture;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::crypto::{open, seal, Keys};
use super::{
    new_id, CredentialRow, MessageRole, MessageRow, MessageStats, MessageWindow, NewProfile,
    ProfileRow, Role, SessionList, SessionRow, Store, StoreError, StoreFuture, UserRow,
    WorkspaceRow, ARCHIVED_LIST_MAX, USER_NAME_MAX_CHARS,
};
use crate::conversation::Block;
use crate::provider::Usage;

/// The migrations, in order. Adding one is a new file and a new entry;
/// an existing file is never edited once it has landed on the branch.
pub const MIGRATIONS: &[(u32, &str)] = &[(1, include_str!("migrations/0001_initial.sql"))];

/// How many operations may wait for the thread before a caller waits too.
pub const QUEUE_CAPACITY: usize = 1024;

/// How long a statement waits for a lock held by another connection.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// The store over one SQLite database, on its own thread.
pub struct SqliteStore {
    /// The queue's one sender. `None` only while `Drop` runs, which takes
    /// it so the thread sees the queue close before the join waits for it.
    jobs: Option<mpsc::Sender<Job>>,
    /// The thread, joined by `Drop`.
    thread: Option<std::thread::JoinHandle<()>>,
    keys: Keys,
    /// The file, or `None` for an in-memory database.
    path: Option<PathBuf>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// `path` for messages.
fn shown(path: Option<&Path>) -> String {
    path.map_or_else(
        || "the in-memory datastore".to_string(),
        |p| p.display().to_string(),
    )
}

impl SqliteStore {
    /// Open or create the datastore at `path`: the parent `0700`, the file
    /// `0600`, WAL, `foreign_keys` on, a five-second busy timeout, and every
    /// missing migration applied.
    pub fn open(path: &Path, keys: Keys) -> Result<Self, StoreError> {
        let at = shown(Some(path));
        if let Some(parent) = path.parent() {
            crate::config::ensure_private_dir(parent)
                .map_err(|e| StoreError::Internal(format!("creating {}: {e}", parent.display())))?;
        }
        let existed = path.exists();
        let conn = Connection::open(path)
            .map_err(|e| StoreError::Internal(format!("opening {at}: {e}")))?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| StoreError::Internal(format!("setting the mode of {at}: {e}")))?;
        }
        conn.pragma_update(None, "journal_mode", "WAL")
            .and_then(|()| conn.pragma_update(None, "synchronous", "NORMAL"))
            .map_err(|e| StoreError::Internal(format!("configuring {at}: {e}")))?;
        Self::finish_open(conn, keys, Some(path.to_path_buf()))
    }

    /// The same store over an in-memory database, for tests.
    pub fn open_in_memory(keys: Keys) -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| StoreError::Internal(format!("opening the in-memory datastore: {e}")))?;
        Self::finish_open(conn, keys, None)
    }

    fn finish_open(
        conn: Connection,
        keys: Keys,
        path: Option<PathBuf>,
    ) -> Result<Self, StoreError> {
        let at = shown(path.as_deref());
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| StoreError::Internal(format!("configuring {at}: {e}")))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| StoreError::Internal(format!("configuring {at}: {e}")))?;
        migrate_with(&conn, MIGRATIONS, &at)?;
        let (jobs, mut rx) = mpsc::channel::<Job>(QUEUE_CAPACITY);
        let thread = std::thread::Builder::new()
            .name("mezame-store".to_string())
            .spawn(move || {
                let mut conn = conn;
                while let Some(job) = rx.blocking_recv() {
                    // A panicking job drops its reply sender; the caller reads
                    // that as `Internal` and the thread carries on.
                    let _ = catch_unwind(AssertUnwindSafe(|| job(&mut conn)));
                }
            })
            .map_err(|e| StoreError::Internal(format!("spawning the store thread: {e}")))?;
        Ok(Self {
            jobs: Some(jobs),
            thread: Some(thread),
            keys,
            path,
        })
    }

    /// Run `f` on the store thread and await its answer.
    async fn run<T, F>(&self, name: &'static str, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = reply_tx.send(f(conn));
        });
        let jobs = self.jobs.as_ref().ok_or(StoreError::Closed)?;
        jobs.send(job).await.map_err(|_| StoreError::Closed)?;
        reply_rx
            .await
            .map_err(|_| StoreError::Internal(format!("{name}: the store thread panicked")))?
    }

    /// Run an arbitrary job on the store thread. For tests that need a
    /// panicking job, since no public operation can be made to panic.
    #[doc(hidden)]
    pub fn run_for_test<F>(&self, job: F) -> StoreFuture<'_, ()>
    where
        F: FnOnce(&mut Connection) + Send + 'static,
    {
        Box::pin(self.run("run_for_test", move |conn| {
            job(conn);
            Ok(())
        }))
    }
}

/// Close the queue, then wait for the thread to run what is queued and
/// close the connection. The store holds the only sender, so once it is
/// gone `blocking_recv` hands out the remaining jobs and then `None`, and
/// the loop ends. A drop running on the store thread itself would wait for
/// its own exit, so that one only closes the queue.
impl Drop for SqliteStore {
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take() {
            if thread.thread().id() != std::thread::current().id() {
                // The loop catches every job's panic, so this only fails
                // when the thread is already gone.
                let _ = thread.join();
            }
        }
    }
}

/// Apply every migration above the database's `user_version`, refusing a
/// database from the future.
pub fn migrate(conn: &Connection, at: &str) -> Result<(), StoreError> {
    migrate_with(conn, MIGRATIONS, at)
}

/// [`migrate`] over an explicit list, so a test can feed a failing one.
#[doc(hidden)]
pub fn migrate_with(
    conn: &Connection,
    migrations: &[(u32, &str)],
    at: &str,
) -> Result<(), StoreError> {
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| StoreError::Internal(format!("reading the schema version of {at}: {e}")))?;
    let latest = migrations.last().map_or(0, |(n, _)| *n);
    if version > latest {
        return Err(StoreError::Invalid(format!(
            "{at} is at schema version {version} and this release ({}) knows version {latest}: \
             it was written by a newer Mezame",
            env!("CARGO_PKG_VERSION")
        )));
    }
    for (number, sql) in migrations {
        if *number <= version {
            continue;
        }
        let script = format!("BEGIN;\n{sql}\nPRAGMA user_version = {number};\nCOMMIT;");
        if let Err(e) = conn.execute_batch(&script) {
            // The failed statement leaves the transaction open.
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(StoreError::Internal(format!(
                "applying migration {number:04} to {at}: {e}"
            )));
        }
    }
    Ok(())
}

/// Map a SQLite error to the store's vocabulary, naming the operation.
fn sql(name: &'static str) -> impl Fn(rusqlite::Error) -> StoreError {
    move |e| match &e {
        rusqlite::Error::SqliteFailure(failure, message)
            if failure.code == ErrorCode::ConstraintViolation =>
        {
            let text = message.clone().unwrap_or_default();
            if text.contains("FOREIGN KEY") {
                StoreError::NotFound
            } else {
                StoreError::Conflict(format!("{name}: {text}"))
            }
        }
        _ => StoreError::Internal(format!("{name}: {e}")),
    }
}

fn user_row(row: &Row<'_>) -> rusqlite::Result<UserRow> {
    let role: String = row.get("role")?;
    let settings: String = row.get("settings")?;
    Ok(UserRow {
        id: row.get("id")?,
        name: row.get("name")?,
        role: Role::parse(&role).unwrap_or(Role::User),
        session_epoch: row.get::<_, i64>("session_epoch")? as u64,
        settings: serde_json::from_str(&settings)
            .unwrap_or_else(|_| Value::Object(Default::default())),
        created: row.get("created")?,
    })
}

const USER_COLUMNS: &str = "id, name, role, session_epoch, settings, created";

fn session_row(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get("id")?,
        user_id: row.get("user_id")?,
        workspace_id: row.get("workspace_id")?,
        profile_id: row.get("profile_id")?,
        title: row.get("title")?,
        archived_at: row.get("archived_at")?,
        created: row.get("created")?,
        updated: row.get("updated")?,
    })
}

const SESSION_COLUMNS: &str =
    "id, user_id, workspace_id, profile_id, title, archived_at, created, updated";

fn credential_row(row: &Row<'_>) -> rusqlite::Result<CredentialRow> {
    Ok(CredentialRow {
        id: row.get("id")?,
        user_id: row.get("user_id")?,
        provider: row.get("provider")?,
        label: row.get("label")?,
        created: row.get("created")?,
        updated: row.get("updated")?,
    })
}

fn profile_row(row: &Row<'_>) -> rusqlite::Result<ProfileRow> {
    Ok(ProfileRow {
        id: row.get("id")?,
        user_id: row.get("user_id")?,
        credential_id: row.get("credential_id")?,
        model: row.get("model")?,
        thinking: row.get("thinking")?,
        thinking_budget: row
            .get::<_, Option<i64>>("thinking_budget")?
            .map(|n| n as u32),
        max_output_tokens: row
            .get::<_, Option<i64>>("max_output_tokens")?
            .map(|n| n as u32),
    })
}

/// One row of `messages`, decoded. A `content` that does not parse is an
/// internal error naming the row, never a panic.
fn message_row(row: &Row<'_>) -> Result<MessageRow, StoreError> {
    let read = |e: rusqlite::Error| StoreError::Internal(format!("load_window: {e}"));
    let id: i64 = row.get("id").map_err(read)?;
    let role: String = row.get("role").map_err(read)?;
    let content: String = row.get("content").map_err(read)?;
    let blocks: Vec<Block> = serde_json::from_str(&content).map_err(|e| {
        StoreError::Internal(format!("load_window: message {id} does not decode: {e}"))
    })?;
    let input: Option<i64> = row.get("input_tokens").map_err(read)?;
    let output: Option<i64> = row.get("output_tokens").map_err(read)?;
    let cache_read: Option<i64> = row.get("cache_read_tokens").map_err(read)?;
    let cache_write: Option<i64> = row.get("cache_write_tokens").map_err(read)?;
    let usage = match (input, output) {
        (Some(input), Some(output)) => Some(Usage {
            input: input.max(0) as u32,
            output: output.max(0) as u32,
            cache_read: cache_read.unwrap_or(0).max(0) as u32,
            cache_write: cache_write.unwrap_or(0).max(0) as u32,
        }),
        _ => None,
    };
    Ok(MessageRow {
        id,
        session_id: row.get("session_id").map_err(read)?,
        role: if role == "assistant" {
            MessageRole::Assistant
        } else {
            MessageRole::User
        },
        blocks,
        text: row.get("text").map_err(read)?,
        usage,
        rejected: row.get::<_, i64>("rejected").map_err(read)? != 0,
        created: row.get("created").map_err(read)?,
    })
}

fn user_exists(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    conn.query_row("SELECT 1 FROM users WHERE id = ?1", params![id], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
        .map_err(sql("user lookup"))
}

fn workspace_of(conn: &Connection, user_id: &str) -> Result<Option<WorkspaceRow>, StoreError> {
    conn.query_row(
        "SELECT id, user_id, name, root FROM workspaces WHERE user_id = ?1 ORDER BY rowid LIMIT 1",
        params![user_id],
        |row| {
            Ok(WorkspaceRow {
                id: row.get("id")?,
                user_id: row.get("user_id")?,
                name: row.get("name")?,
                root: row.get("root")?,
            })
        },
    )
    .optional()
    .map_err(sql("default_workspace"))
}

fn session_by_id(conn: &Connection, id: &str) -> Result<Option<SessionRow>, StoreError> {
    conn.query_row(
        &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
        params![id],
        session_row,
    )
    .optional()
    .map_err(sql("session"))
}

fn global_profile_of(conn: &Connection) -> Result<Option<ProfileRow>, StoreError> {
    conn.query_row(
        "SELECT id, user_id, credential_id, model, thinking, thinking_budget, max_output_tokens \
         FROM profiles WHERE user_id IS NULL ORDER BY rowid LIMIT 1",
        [],
        profile_row,
    )
    .optional()
    .map_err(sql("global_profile"))
}

fn changed_or_not_found(changed: usize) -> Result<(), StoreError> {
    if changed == 0 {
        Err(StoreError::NotFound)
    } else {
        Ok(())
    }
}

fn encode_blocks(blocks: &[Block], name: &'static str) -> Result<String, StoreError> {
    serde_json::to_string(blocks).map_err(|e| StoreError::Internal(format!("{name}: {e}")))
}

impl Store for SqliteStore {
    fn backend_name(&self) -> &'static str {
        "sqlite"
    }

    fn health(&self) -> StoreFuture<'_, ()> {
        Box::pin(self.run("health", |conn| {
            conn.query_row("SELECT 1", [], |_| Ok(()))
                .map_err(sql("health"))
        }))
    }

    fn create_user(
        &self,
        name: &str,
        password_hash: &str,
        role: Role,
        now: i64,
    ) -> StoreFuture<'_, UserRow> {
        let name = name.trim().to_string();
        let hash = password_hash.to_string();
        Box::pin(self.run("create_user", move |conn| {
            if name.is_empty() {
                return Err(StoreError::Invalid("a user name is required".to_string()));
            }
            if name.chars().count() > USER_NAME_MAX_CHARS {
                return Err(StoreError::Invalid(format!(
                    "a user name is at most {USER_NAME_MAX_CHARS} characters"
                )));
            }
            let id = new_id();
            conn.execute(
                "INSERT INTO users (id, name, password_hash, role, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, name, hash, role.as_str(), now],
            )
            .map_err(|e| match sql("create_user")(e) {
                StoreError::Conflict(_) => StoreError::Conflict(format!("the user name `{name}`")),
                other => other,
            })?;
            Ok(UserRow {
                id,
                name,
                role,
                session_epoch: 0,
                settings: Value::Object(Default::default()),
                created: now,
            })
        }))
    }

    fn user_by_name(&self, name: &str) -> StoreFuture<'_, Option<UserRow>> {
        let name = name.to_string();
        Box::pin(self.run("user_by_name", move |conn| {
            conn.query_row(
                &format!("SELECT {USER_COLUMNS} FROM users WHERE name = ?1"),
                params![name],
                user_row,
            )
            .optional()
            .map_err(sql("user_by_name"))
        }))
    }

    fn user_by_id(&self, id: &str) -> StoreFuture<'_, Option<UserRow>> {
        let id = id.to_string();
        Box::pin(self.run("user_by_id", move |conn| {
            conn.query_row(
                &format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"),
                params![id],
                user_row,
            )
            .optional()
            .map_err(sql("user_by_id"))
        }))
    }

    fn list_users(&self) -> StoreFuture<'_, Vec<UserRow>> {
        Box::pin(self.run("list_users", |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {USER_COLUMNS} FROM users ORDER BY created, name"
                ))
                .map_err(sql("list_users"))?;
            let rows = stmt
                .query_map([], user_row)
                .map_err(sql("list_users"))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql("list_users"))?;
            Ok(rows)
        }))
    }

    fn count_users(&self) -> StoreFuture<'_, u64> {
        Box::pin(self.run("count_users", |conn| {
            conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get::<_, i64>(0))
                .map(|n| n.max(0) as u64)
                .map_err(sql("count_users"))
        }))
    }

    fn password_hash_of(&self, name: &str) -> StoreFuture<'_, Option<String>> {
        let name = name.to_string();
        Box::pin(self.run("password_hash_of", move |conn| {
            conn.query_row(
                "SELECT password_hash FROM users WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql("password_hash_of"))
        }))
    }

    fn login_user(&self, name: &str) -> StoreFuture<'_, Option<(UserRow, String)>> {
        let name = name.to_string();
        Box::pin(self.run("login_user", move |conn| {
            conn.query_row(
                &format!("SELECT {USER_COLUMNS}, password_hash FROM users WHERE name = ?1"),
                params![name],
                |row| Ok((user_row(row)?, row.get("password_hash")?)),
            )
            .optional()
            .map_err(sql("login_user"))
        }))
    }

    fn set_password_hash(&self, id: &str, hash: &str) -> StoreFuture<'_, ()> {
        let id = id.to_string();
        let hash = hash.to_string();
        Box::pin(self.run("set_password_hash", move |conn| {
            let changed = conn
                .execute(
                    "UPDATE users SET password_hash = ?1, session_epoch = session_epoch + 1 \
                     WHERE id = ?2",
                    params![hash, id],
                )
                .map_err(sql("set_password_hash"))?;
            changed_or_not_found(changed)
        }))
    }

    fn bump_session_epoch(&self, id: &str) -> StoreFuture<'_, u64> {
        let id = id.to_string();
        Box::pin(self.run("bump_session_epoch", move |conn| {
            let changed = conn
                .execute(
                    "UPDATE users SET session_epoch = session_epoch + 1 WHERE id = ?1",
                    params![id],
                )
                .map_err(sql("bump_session_epoch"))?;
            changed_or_not_found(changed)?;
            conn.query_row(
                "SELECT session_epoch FROM users WHERE id = ?1",
                params![id],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u64)
            .map_err(sql("bump_session_epoch"))
        }))
    }

    fn settings(&self, id: &str) -> StoreFuture<'_, Value> {
        let id = id.to_string();
        Box::pin(self.run("settings", move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT settings FROM users WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql("settings"))?;
            let raw = raw.ok_or(StoreError::NotFound)?;
            Ok(serde_json::from_str(&raw).unwrap_or_else(|_| Value::Object(Default::default())))
        }))
    }

    fn set_settings(&self, id: &str, settings: &Value) -> StoreFuture<'_, ()> {
        let id = id.to_string();
        let text = settings.to_string();
        Box::pin(self.run("set_settings", move |conn| {
            let changed = conn
                .execute(
                    "UPDATE users SET settings = ?1 WHERE id = ?2",
                    params![text, id],
                )
                .map_err(sql("set_settings"))?;
            changed_or_not_found(changed)
        }))
    }

    fn default_workspace(&self, user_id: &str) -> StoreFuture<'_, Option<WorkspaceRow>> {
        let user_id = user_id.to_string();
        Box::pin(self.run("default_workspace", move |conn| {
            workspace_of(conn, &user_id)
        }))
    }

    fn create_session(
        &self,
        user_id: &str,
        id: &str,
        workspace_root: Option<&Path>,
        now: i64,
    ) -> StoreFuture<'_, SessionRow> {
        let user_id = user_id.to_string();
        let id = id.to_string();
        let root = workspace_root.map(|p| p.to_string_lossy().into_owned());
        Box::pin(self.run("create_session", move |conn| {
            if !user_exists(conn, &user_id)? {
                return Err(StoreError::NotFound);
            }
            let tx = conn.transaction().map_err(sql("create_session"))?;
            let workspace_id = match (workspace_of(&tx, &user_id)?, root) {
                (Some(existing), _) => Some(existing.id),
                (None, Some(root)) => {
                    let workspace_id = new_id();
                    tx.execute(
                        "INSERT INTO workspaces (id, user_id, name, root) VALUES (?1, ?2, 'default', ?3)",
                        params![workspace_id, user_id, root],
                    )
                    .map_err(sql("create_session"))?;
                    Some(workspace_id)
                }
                (None, None) => None,
            };
            let profile_id = global_profile_of(&tx)?.map(|p| p.id);
            tx.execute(
                "INSERT INTO sessions (id, user_id, workspace_id, profile_id, created, updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![id, user_id, workspace_id, profile_id, now],
            )
            .map_err(|e| match sql("create_session")(e) {
                StoreError::Conflict(_) => StoreError::Conflict(format!("the session id `{id}`")),
                other => other,
            })?;
            tx.commit().map_err(sql("create_session"))?;
            Ok(SessionRow {
                id,
                user_id,
                workspace_id,
                profile_id,
                title: None,
                archived_at: None,
                created: now,
                updated: now,
            })
        }))
    }

    fn session(&self, id: &str) -> StoreFuture<'_, Option<SessionRow>> {
        let id = id.to_string();
        Box::pin(self.run("session", move |conn| session_by_id(conn, &id)))
    }

    fn list_sessions(&self, user_id: &str) -> StoreFuture<'_, SessionList> {
        let user_id = user_id.to_string();
        Box::pin(self.run("list_sessions", move |conn| {
            let query = |sql_text: &str| -> Result<Vec<SessionRow>, StoreError> {
                let mut stmt = conn.prepare(sql_text).map_err(sql("list_sessions"))?;
                let rows = stmt
                    .query_map(params![user_id, ARCHIVED_LIST_MAX as i64], session_row)
                    .map_err(sql("list_sessions"))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sql("list_sessions"))?;
                Ok(rows)
            };
            let active = query(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions WHERE user_id = ?1 AND archived_at IS NULL \
                 ORDER BY created, id LIMIT -1 OFFSET 0 * ?2"
            ))?;
            let archived = query(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions WHERE user_id = ?1 AND archived_at IS NOT NULL \
                 ORDER BY archived_at DESC, id LIMIT ?2"
            ))?;
            Ok(SessionList { active, archived })
        }))
    }

    fn set_title(&self, id: &str, title: &str, now: i64) -> StoreFuture<'_, ()> {
        let id = id.to_string();
        let title = title.to_string();
        Box::pin(self.run("set_title", move |conn| {
            let changed = conn
                .execute(
                    "UPDATE sessions SET title = ?1, updated = ?2 WHERE id = ?3",
                    params![title, now, id],
                )
                .map_err(sql("set_title"))?;
            changed_or_not_found(changed)
        }))
    }

    fn set_title_if_null(&self, id: &str, title: &str, now: i64) -> StoreFuture<'_, bool> {
        let id = id.to_string();
        let title = title.to_string();
        Box::pin(self.run("set_title_if_null", move |conn| {
            if session_by_id(conn, &id)?.is_none() {
                return Err(StoreError::NotFound);
            }
            let changed = conn
                .execute(
                    "UPDATE sessions SET title = ?1, updated = ?2 WHERE id = ?3 AND title IS NULL",
                    params![title, now, id],
                )
                .map_err(sql("set_title_if_null"))?;
            Ok(changed == 1)
        }))
    }

    fn set_archived(&self, id: &str, archived: bool, now: i64) -> StoreFuture<'_, ()> {
        let id = id.to_string();
        Box::pin(self.run("set_archived", move |conn| {
            let archived_at: Option<i64> = archived.then_some(now);
            let changed = conn
                .execute(
                    "UPDATE sessions SET archived_at = ?1, updated = ?2 WHERE id = ?3",
                    params![archived_at, now, id],
                )
                .map_err(sql("set_archived"))?;
            changed_or_not_found(changed)
        }))
    }

    fn delete_session(&self, id: &str) -> StoreFuture<'_, ()> {
        let id = id.to_string();
        Box::pin(self.run("delete_session", move |conn| {
            let changed = conn
                .execute("DELETE FROM sessions WHERE id = ?1", params![id])
                .map_err(sql("delete_session"))?;
            changed_or_not_found(changed)
        }))
    }

    fn append_user(
        &self,
        session_id: &str,
        blocks: &[Block],
        text: &str,
        created: i64,
    ) -> StoreFuture<'_, i64> {
        let session_id = session_id.to_string();
        let text = text.to_string();
        let content = encode_blocks(blocks, "append_user");
        Box::pin(self.run("append_user", move |conn| {
            let content = content?;
            conn.execute(
                "INSERT INTO messages (session_id, role, content, text, created) \
                 VALUES (?1, 'user', ?2, ?3, ?4)",
                params![session_id, content, text, created],
            )
            .map_err(sql("append_user"))?;
            Ok(conn.last_insert_rowid())
        }))
    }

    fn append_assistant(
        &self,
        session_id: &str,
        blocks: &[Block],
        usage: Option<Usage>,
        rejected: bool,
        created: i64,
    ) -> StoreFuture<'_, i64> {
        let session_id = session_id.to_string();
        let content = encode_blocks(blocks, "append_assistant");
        Box::pin(self.run("append_assistant", move |conn| {
            let content = content?;
            let (input, output, cache_read, cache_write) = match usage {
                Some(u) => (
                    Some(i64::from(u.input)),
                    Some(i64::from(u.output)),
                    Some(i64::from(u.cache_read)),
                    Some(i64::from(u.cache_write)),
                ),
                None => (None, None, None, None),
            };
            conn.execute(
                "INSERT INTO messages (session_id, role, content, input_tokens, output_tokens, \
                 cache_read_tokens, cache_write_tokens, rejected, created) \
                 VALUES (?1, 'assistant', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session_id,
                    content,
                    input,
                    output,
                    cache_read,
                    cache_write,
                    i64::from(rejected),
                    created
                ],
            )
            .map_err(sql("append_assistant"))?;
            Ok(conn.last_insert_rowid())
        }))
    }

    fn mark_rejected(&self, message_ids: &[i64]) -> StoreFuture<'_, ()> {
        let ids = message_ids.to_vec();
        Box::pin(self.run("mark_rejected", move |conn| {
            if ids.is_empty() {
                return Ok(());
            }
            let tx = conn.transaction().map_err(sql("mark_rejected"))?;
            for id in &ids {
                tx.execute(
                    "UPDATE messages SET rejected = 1 WHERE id = ?1",
                    params![id],
                )
                .map_err(sql("mark_rejected"))?;
            }
            tx.commit().map_err(sql("mark_rejected"))
        }))
    }

    fn load_window(
        &self,
        session_id: &str,
        max_rows: usize,
        max_bytes: usize,
    ) -> StoreFuture<'_, MessageWindow> {
        let session_id = session_id.to_string();
        Box::pin(self.run("load_window", move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, role, content, text, input_tokens, output_tokens, \
                     cache_read_tokens, cache_write_tokens, rejected, created, \
                     length(CAST(content AS BLOB)) AS content_bytes \
                     FROM messages WHERE session_id = ?1 ORDER BY id DESC",
                )
                .map_err(sql("load_window"))?;
            let mut rows = stmt
                .query(params![session_id])
                .map_err(sql("load_window"))?;
            let mut kept: Vec<MessageRow> = Vec::new();
            let mut bytes: usize = 0;
            let mut complete = true;
            while let Some(row) = rows.next().map_err(sql("load_window"))? {
                let size: i64 = row.get("content_bytes").map_err(sql("load_window"))?;
                let size = size.max(0) as usize;
                // The newest row is always kept; every later one only while
                // both bounds hold. Nothing past the first excluded row is
                // decoded, or even fetched.
                if !kept.is_empty() && (kept.len() >= max_rows || bytes + size > max_bytes) {
                    complete = false;
                    break;
                }
                kept.push(message_row(row)?);
                bytes += size;
            }
            kept.reverse();
            Ok(MessageWindow {
                rows: kept,
                complete,
            })
        }))
    }

    fn message_stats(&self, session_id: &str) -> StoreFuture<'_, MessageStats> {
        let session_id = session_id.to_string();
        Box::pin(self.run("message_stats", move |conn| {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(CAST(content AS BLOB))), 0) \
                 FROM messages WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok(MessageStats {
                        count: row.get::<_, i64>(0)?.max(0) as u64,
                        bytes: row.get::<_, i64>(1)?.max(0) as u64,
                    })
                },
            )
            .map_err(sql("message_stats"))
        }))
    }

    fn create_credential(
        &self,
        owner: Option<&str>,
        creator: &str,
        provider: &str,
        label: &str,
        payload: &Value,
        now: i64,
    ) -> StoreFuture<'_, CredentialRow> {
        let owner = owner.map(str::to_string);
        let creator = creator.to_string();
        let provider = provider.to_string();
        let label = label.to_string();
        let payload = payload.to_string();
        let key = self.keys.credential;
        Box::pin(self.run("create_credential", move |conn| {
            if !user_exists(conn, &creator)? {
                return Err(StoreError::NotFound);
            }
            let id = new_id();
            let (nonce, ciphertext) = seal(&key, &id, payload.as_bytes());
            let tx = conn.transaction().map_err(sql("create_credential"))?;
            tx.execute(
                "INSERT INTO credentials (id, user_id, provider, label, ciphertext, nonce, created, updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![id, owner, provider, label, ciphertext, nonce, now],
            )
            .map_err(sql("create_credential"))?;
            tx.execute(
                "INSERT INTO grants (user_id, credential_id) VALUES (?1, ?2)",
                params![creator, id],
            )
            .map_err(sql("create_credential"))?;
            tx.commit().map_err(sql("create_credential"))?;
            Ok(CredentialRow {
                id,
                user_id: owner,
                provider,
                label,
                created: now,
                updated: now,
            })
        }))
    }

    fn credentials(
        &self,
        owner: Option<&str>,
        provider: &str,
    ) -> StoreFuture<'_, Vec<CredentialRow>> {
        let owner = owner.map(str::to_string);
        let provider = provider.to_string();
        Box::pin(self.run("credentials", move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, user_id, provider, label, created, updated FROM credentials \
                     WHERE user_id IS ?1 AND provider = ?2 ORDER BY created, id",
                )
                .map_err(sql("credentials"))?;
            let rows = stmt
                .query_map(params![owner, provider], credential_row)
                .map_err(sql("credentials"))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql("credentials"))?;
            Ok(rows)
        }))
    }

    fn credential_payload(&self, id: &str) -> StoreFuture<'_, Value> {
        let id = id.to_string();
        let key = self.keys.credential;
        Box::pin(self.run("credential_payload", move |conn| {
            let sealed: Option<(Vec<u8>, Vec<u8>)> = conn
                .query_row(
                    "SELECT ciphertext, nonce FROM credentials WHERE id = ?1",
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sql("credential_payload"))?;
            let (ciphertext, nonce) = sealed.ok_or(StoreError::NotFound)?;
            let plain = open(&key, &id, &nonce, &ciphertext).map_err(|_| StoreError::Tampered)?;
            serde_json::from_slice(&plain).map_err(|_| StoreError::Tampered)
        }))
    }

    fn delete_credential(&self, id: &str) -> StoreFuture<'_, ()> {
        let id = id.to_string();
        Box::pin(self.run("delete_credential", move |conn| {
            let changed = conn
                .execute("DELETE FROM credentials WHERE id = ?1", params![id])
                .map_err(sql("delete_credential"))?;
            changed_or_not_found(changed)
        }))
    }

    fn drop_unopenable_credentials(&self) -> StoreFuture<'_, u64> {
        let key = self.keys.credential;
        Box::pin(self.run("drop_unopenable_credentials", move |conn| {
            let sealed: Vec<(String, Vec<u8>, Vec<u8>)> = {
                let mut stmt = conn
                    .prepare("SELECT id, nonce, ciphertext FROM credentials")
                    .map_err(sql("drop_unopenable_credentials"))?;
                let rows = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .map_err(sql("drop_unopenable_credentials"))?;
                rows.collect::<Result<_, _>>()
                    .map_err(sql("drop_unopenable_credentials"))?
            };
            let unopenable: Vec<String> = sealed
                .into_iter()
                .filter(|(id, nonce, ciphertext)| open(&key, id, nonce, ciphertext).is_err())
                .map(|(id, _, _)| id)
                .collect();
            if unopenable.is_empty() {
                return Ok(0);
            }
            // The sessions that ran on a dropped profile are unlinked first:
            // `sessions.profile_id` references `profiles(id)` with no
            // cascade, so the profile could not go otherwise, and a session
            // is a conversation worth more than the profile it ran on.
            let tx = conn
                .transaction()
                .map_err(sql("drop_unopenable_credentials"))?;
            for id in &unopenable {
                tx.execute(
                    "UPDATE sessions SET profile_id = NULL WHERE profile_id IN \
                     (SELECT id FROM profiles WHERE credential_id = ?1)",
                    params![id],
                )
                .map_err(sql("drop_unopenable_credentials"))?;
                tx.execute("DELETE FROM profiles WHERE credential_id = ?1", params![id])
                    .map_err(sql("drop_unopenable_credentials"))?;
                tx.execute("DELETE FROM credentials WHERE id = ?1", params![id])
                    .map_err(sql("drop_unopenable_credentials"))?;
            }
            tx.commit().map_err(sql("drop_unopenable_credentials"))?;
            Ok(unopenable.len() as u64)
        }))
    }

    fn global_profile(&self) -> StoreFuture<'_, Option<ProfileRow>> {
        Box::pin(self.run("global_profile", |conn| global_profile_of(conn)))
    }

    fn upsert_global_profile(&self, profile: &NewProfile) -> StoreFuture<'_, ProfileRow> {
        let new = profile.clone();
        Box::pin(self.run("upsert_global_profile", move |conn| {
            let budget = new.thinking_budget.map(i64::from);
            let ceiling = new.max_output_tokens.map(i64::from);
            let id = match global_profile_of(conn)? {
                Some(existing) => {
                    conn.execute(
                        "UPDATE profiles SET credential_id = ?1, model = ?2, thinking = ?3, \
                         thinking_budget = ?4, max_output_tokens = ?5 WHERE id = ?6",
                        params![
                            new.credential_id,
                            new.model,
                            new.thinking,
                            budget,
                            ceiling,
                            existing.id
                        ],
                    )
                    .map_err(sql("upsert_global_profile"))?;
                    existing.id
                }
                None => {
                    let id = new_id();
                    conn.execute(
                        "INSERT INTO profiles (id, user_id, credential_id, model, thinking, \
                         thinking_budget, max_output_tokens) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            id,
                            new.credential_id,
                            new.model,
                            new.thinking,
                            budget,
                            ceiling
                        ],
                    )
                    .map_err(sql("upsert_global_profile"))?;
                    id
                }
            };
            Ok(ProfileRow {
                id,
                user_id: None,
                credential_id: new.credential_id,
                model: new.model,
                thinking: new.thinking,
                thinking_budget: new.thinking_budget,
                max_output_tokens: new.max_output_tokens,
            })
        }))
    }
}

// The trait's futures borrow `&self`; `BoxFuture<'static, _>` is never
// needed because the store is shared behind an `Arc`.
#[allow(dead_code)]
fn _assert_object_safe(_: &dyn Store) -> BoxFuture<'static, ()> {
    Box::pin(async {})
}
