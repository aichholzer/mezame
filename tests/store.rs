//! The `Store` over an in-memory SQLite database: every operation, the
//! migration, the bounds, and the thread (Requirements 2 and 3 of the
//! phase 2 spec, and criteria 4.5 and 4.6).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mezame::conversation::Block;
use mezame::provider::Usage;
use mezame::store::crypto::{MasterKey, KEY_LEN};
use mezame::store::sqlite::{migrate_with, SqliteStore, MIGRATIONS, QUEUE_CAPACITY};
use mezame::store::{
    new_id, MessageRole, NewProfile, Role, Store, StoreError, ARCHIVED_LIST_MAX,
    USER_NAME_MAX_CHARS,
};
use rusqlite::Connection;
use serde_json::{json, Value};

fn store() -> SqliteStore {
    SqliteStore::open_in_memory(MasterKey::from_bytes_for_test([5u8; KEY_LEN]).keys()).unwrap()
}

fn text(t: &str) -> Block {
    Block::Text {
        text: t.to_string(),
    }
}

async fn user(store: &SqliteStore, name: &str) -> String {
    store
        .create_user(
            name,
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
            Role::User,
            1,
        )
        .await
        .unwrap()
        .id
}

async fn session(store: &SqliteStore, user_id: &str) -> String {
    let id = new_id();
    store
        .create_session(user_id, &id, Some(Path::new("/tmp/work")), 10)
        .await
        .unwrap();
    id
}

// ---------- ids and the migration ----------

#[test]
fn ids_are_32_lowercase_hex_and_distinct() {
    let a = new_id();
    let b = new_id();
    assert_eq!(a.len(), 32);
    assert!(a
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(a, b);
}

#[test]
fn the_migration_creates_every_table_column_and_index() {
    let conn = Connection::open_in_memory().unwrap();
    migrate_with(&conn, MIGRATIONS, "test").unwrap();
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, MIGRATIONS.len() as u32);

    let columns = |table: &str| -> Vec<(String, String, bool)> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    let names = |table: &str| -> Vec<String> { columns(table).into_iter().map(|c| c.0).collect() };
    assert_eq!(
        names("users"),
        [
            "id",
            "name",
            "password_hash",
            "role",
            "session_epoch",
            "email",
            "settings",
            "created"
        ]
    );
    assert_eq!(
        names("workspaces"),
        [
            "id",
            "user_id",
            "name",
            "root",
            "permission_policy",
            "tool_limits"
        ]
    );
    assert_eq!(
        names("sessions"),
        [
            "id",
            "user_id",
            "workspace_id",
            "profile_id",
            "title",
            "archived_at",
            "created",
            "updated"
        ]
    );
    assert_eq!(
        names("messages"),
        [
            "id",
            "session_id",
            "role",
            "content",
            "text",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "compaction_id",
            "rejected",
            "created"
        ]
    );
    assert_eq!(
        names("profiles"),
        [
            "id",
            "user_id",
            "credential_id",
            "model",
            "thinking",
            "thinking_budget",
            "max_output_tokens",
            "temperature",
            "system_prompt"
        ]
    );
    assert_eq!(
        names("credentials"),
        [
            "id",
            "user_id",
            "provider",
            "label",
            "ciphertext",
            "nonce",
            "created",
            "updated"
        ]
    );
    assert_eq!(
        names("grants"),
        ["user_id", "credential_id", "model_allowlist"]
    );
    assert_eq!(
        names("mcp_grants"),
        ["user_id", "server_name", "enabled", "tool_allowlist"]
    );
    assert_eq!(
        names("mcp_servers"),
        [
            "id",
            "user_id",
            "name",
            "ciphertext",
            "nonce",
            "created",
            "updated"
        ]
    );
    assert_eq!(
        names("memories"),
        [
            "id",
            "user_id",
            "workspace_id",
            "path",
            "title",
            "summary",
            "content_hash",
            "mtime",
            "size"
        ]
    );
    // Constraints the rows rely on.
    let not_null: Vec<String> = columns("messages")
        .into_iter()
        .filter(|c| c.2)
        .map(|c| c.0)
        .collect();
    assert_eq!(
        not_null,
        ["session_id", "role", "content", "rejected", "created"]
    );
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'messages'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("ON DELETE CASCADE"), "{sql}");
    assert!(sql.contains("role IN ('user', 'assistant')"), "{sql}");
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'users'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("UNIQUE"), "{sql}");
    assert!(sql.contains("role IN ('admin', 'user')"), "{sql}");
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'grants'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("ON DELETE CASCADE"), "{sql}");
    let mut indexes: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    indexes.sort();
    assert_eq!(
        indexes,
        [
            "credentials_by_owner",
            "messages_by_session",
            "sessions_by_user"
        ]
    );
    // Applying again changes nothing.
    migrate_with(&conn, MIGRATIONS, "test").unwrap();
}

#[test]
fn a_database_from_the_future_is_refused_naming_both_numbers() {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    let err = migrate_with(&conn, MIGRATIONS, "the datastore").unwrap_err();
    let text = err.to_string();
    assert!(text.contains("schema version 99"), "{text}");
    assert!(text.contains("knows version 1"), "{text}");
    assert!(text.contains("newer Mezame"), "{text}");
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "nothing was altered");
}

#[test]
fn a_failing_migration_leaves_the_version_unchanged() {
    let conn = Connection::open_in_memory().unwrap();
    let bad: &[(u32, &str)] = &[
        (1, MIGRATIONS[0].1),
        (
            2,
            "CREATE TABLE fine (id INTEGER); CREATE TABLE fine (id INTEGER);",
        ),
    ];
    let err = migrate_with(&conn, bad, "the datastore").unwrap_err();
    let text = err.to_string();
    assert!(text.contains("migration 0002"), "{text}");
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1, "the failed migration's number was not stamped");
    let fine: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fine'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fine, 0, "the transaction rolled back");
}

// ---------- the thread ----------

#[tokio::test]
async fn a_panicking_job_answers_its_caller_and_the_thread_survives() {
    let store = store();
    let err = store
        .run_for_test(|_conn| panic!("a bad statement"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Internal(ref why) if why.contains("store thread panicked")),
        "{err:?}"
    );
    store.health().await.unwrap();
    assert_eq!(store.count_users().await.unwrap(), 0);
}

#[tokio::test]
async fn a_burst_past_the_queue_bound_completes_in_order() {
    let store = Arc::new(store());
    let user_id = user(&store, "burst").await;
    let sid = session(&store, &user_id).await;
    // Park the thread behind one slow job, then queue more than the channel
    // holds; every caller waits rather than failing, and every write lands.
    let parked = store.run_for_test(|_| std::thread::sleep(Duration::from_millis(200)));
    let mut writers = Vec::new();
    for i in 0..(QUEUE_CAPACITY + 1) {
        let store = Arc::clone(&store);
        let sid = sid.clone();
        writers.push(tokio::spawn(async move {
            store
                .append_user(&sid, &[text(&format!("m{i}"))], &format!("m{i}"), i as i64)
                .await
                .unwrap()
        }));
    }
    parked.await.unwrap();
    let mut ids = Vec::new();
    for w in writers {
        ids.push(w.await.unwrap());
    }
    assert_eq!(ids.len(), QUEUE_CAPACITY + 1);
    assert_eq!(
        store.message_stats(&sid).await.unwrap().count,
        (QUEUE_CAPACITY + 1) as u64
    );
}

#[tokio::test]
async fn health_and_backend_name() {
    let store = store();
    assert_eq!(store.backend_name(), "sqlite");
    store.health().await.unwrap();
}

// ---------- users ----------

#[tokio::test]
async fn users_are_created_found_listed_and_counted_without_their_hash() {
    let store = store();
    assert_eq!(store.count_users().await.unwrap(), 0);
    let alice = store
        .create_user("alice", "$argon2id$hash-a", Role::Admin, 100)
        .await
        .unwrap();
    assert_eq!(alice.name, "alice");
    assert_eq!(alice.role, Role::Admin);
    assert_eq!(alice.session_epoch, 0);
    assert_eq!(alice.settings, json!({}));
    assert_eq!(alice.created, 100);
    let bob = store
        .create_user("  bob ", "$argon2id$hash-b", Role::User, 200)
        .await
        .unwrap();
    assert_eq!(bob.name, "bob", "trimmed");
    assert_eq!(store.count_users().await.unwrap(), 2);
    let listed = store.list_users().await.unwrap();
    assert_eq!(
        listed.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(),
        ["alice", "bob"]
    );
    assert_eq!(
        store.user_by_name("alice").await.unwrap().unwrap().id,
        alice.id
    );
    assert_eq!(
        store.user_by_id(&bob.id).await.unwrap().unwrap().name,
        "bob"
    );
    assert!(store.user_by_name("carol").await.unwrap().is_none());
    assert!(store.user_by_id("nope").await.unwrap().is_none());
    // The hash reaches the login handler alone.
    assert_eq!(
        store.password_hash_of("alice").await.unwrap().as_deref(),
        Some("$argon2id$hash-a")
    );
    assert!(store.password_hash_of("carol").await.unwrap().is_none());
    let debug = format!("{alice:?}{bob:?}{listed:?}");
    assert!(
        !debug.contains("hash-a") && !debug.contains("hash-b"),
        "{debug}"
    );
}

#[tokio::test]
async fn user_names_are_ruled_and_unique() {
    let store = store();
    let empty = store
        .create_user("  ", "h", Role::User, 1)
        .await
        .unwrap_err();
    assert!(matches!(empty, StoreError::Invalid(_)), "{empty:?}");
    let long = "x".repeat(USER_NAME_MAX_CHARS + 1);
    let err = store
        .create_user(&long, "h", Role::User, 1)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Invalid(ref why) if why.contains("64")),
        "{err:?}"
    );
    let ok = "y".repeat(USER_NAME_MAX_CHARS);
    store.create_user(&ok, "h", Role::User, 1).await.unwrap();
    store.create_user("dup", "h", Role::User, 1).await.unwrap();
    let err = store
        .create_user("dup", "h2", Role::User, 2)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(ref what) if what.contains("`dup`")),
        "{err:?}"
    );
    assert!(!err.to_string().contains("h2"), "no hash in the error");
}

#[tokio::test]
async fn a_password_change_bumps_the_epoch_in_the_same_statement() {
    let store = store();
    let id = user(&store, "alice").await;
    store.set_password_hash(&id, "$argon2id$new").await.unwrap();
    let row = store.user_by_id(&id).await.unwrap().unwrap();
    assert_eq!(row.session_epoch, 1);
    assert_eq!(
        store.password_hash_of("alice").await.unwrap().as_deref(),
        Some("$argon2id$new")
    );
    assert_eq!(store.bump_session_epoch(&id).await.unwrap(), 2);
    assert_eq!(store.bump_session_epoch(&id).await.unwrap(), 3);
    assert_eq!(
        store.set_password_hash("nope", "h").await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        store.bump_session_epoch("nope").await.unwrap_err(),
        StoreError::NotFound
    );
}

#[tokio::test]
async fn settings_round_trip_as_an_opaque_object() {
    let store = store();
    let id = user(&store, "alice").await;
    assert_eq!(store.settings(&id).await.unwrap(), json!({}));
    let settings = json!({ "theme": "dark", "idleSuspendMinutes": 10, "nested": { "a": [1, 2] } });
    store.set_settings(&id, &settings).await.unwrap();
    assert_eq!(store.settings(&id).await.unwrap(), settings);
    assert_eq!(
        store.settings("nope").await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        store.set_settings("nope", &settings).await.unwrap_err(),
        StoreError::NotFound
    );
}

// ---------- sessions and workspaces ----------

#[tokio::test]
async fn a_session_creates_the_default_workspace_once_and_reuses_it() {
    let store = store();
    let alice = user(&store, "alice").await;
    assert!(store.default_workspace(&alice).await.unwrap().is_none());
    let first = new_id();
    let row = store
        .create_session(&alice, &first, Some(Path::new("/srv/project")), 10)
        .await
        .unwrap();
    let workspace = store.default_workspace(&alice).await.unwrap().unwrap();
    assert_eq!(workspace.root, "/srv/project");
    assert_eq!(workspace.name, "default");
    assert_eq!(row.workspace_id.as_deref(), Some(workspace.id.as_str()));
    assert_eq!(row.profile_id, None, "no global profile yet");
    assert_eq!(row.title, None);
    assert_eq!((row.created, row.updated), (10, 10));
    let second = new_id();
    let row2 = store
        .create_session(&alice, &second, Some(Path::new("/elsewhere")), 11)
        .await
        .unwrap();
    assert_eq!(
        row2.workspace_id, row.workspace_id,
        "the existing workspace is reused"
    );
    assert_eq!(
        store.default_workspace(&alice).await.unwrap().unwrap().root,
        "/srv/project"
    );
    // Another user with no eligible root gets none.
    let bob = user(&store, "bob").await;
    let bare = store
        .create_session(&bob, &new_id(), None, 12)
        .await
        .unwrap();
    assert_eq!(bare.workspace_id, None);
    assert!(store.default_workspace(&bob).await.unwrap().is_none());
    // The global profile, once it exists, is stamped on new sessions.
    let profile = store
        .upsert_global_profile(&NewProfile {
            model: "m".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let stamped = store
        .create_session(&bob, &new_id(), None, 13)
        .await
        .unwrap();
    assert_eq!(stamped.profile_id.as_deref(), Some(profile.id.as_str()));
    // Unknown user, duplicate id.
    assert_eq!(
        store
            .create_session("nope", &new_id(), None, 1)
            .await
            .unwrap_err(),
        StoreError::NotFound
    );
    let dup = store
        .create_session(&alice, &first, None, 1)
        .await
        .unwrap_err();
    assert!(matches!(dup, StoreError::Conflict(_)), "{dup:?}");
}

#[tokio::test]
async fn sessions_are_listed_per_user_active_by_creation_and_archived_newest_first_capped() {
    let store = store();
    let alice = user(&store, "alice").await;
    let bob = user(&store, "bob").await;
    let mut ids = Vec::new();
    for i in 0..(ARCHIVED_LIST_MAX + 5) {
        let id = new_id();
        store
            .create_session(&alice, &id, None, 100 + i as i64)
            .await
            .unwrap();
        ids.push(id);
    }
    let theirs = new_id();
    store.create_session(&bob, &theirs, None, 5).await.unwrap();
    // Archive all but the first two, at increasing times.
    for (i, id) in ids.iter().enumerate().skip(2) {
        store.set_archived(id, true, 1000 + i as i64).await.unwrap();
    }
    let list = store.list_sessions(&alice).await.unwrap();
    assert_eq!(
        list.active
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        [ids[0].as_str(), ids[1].as_str()]
    );
    assert_eq!(list.archived.len(), ARCHIVED_LIST_MAX);
    assert_eq!(
        list.archived[0].id,
        ids[ids.len() - 1],
        "newest archived first"
    );
    assert!(list.archived.iter().all(|s| s.archived_at.is_some()));
    assert!(
        !list
            .active
            .iter()
            .chain(list.archived.iter())
            .any(|s| s.id == theirs),
        "another user's session is not listed"
    );
    let bobs = store.list_sessions(&bob).await.unwrap();
    assert_eq!(bobs.active.len(), 1);
    assert!(bobs.archived.is_empty());
    // Restore one.
    store.set_archived(&ids[3], false, 2000).await.unwrap();
    let row = store.session(&ids[3]).await.unwrap().unwrap();
    assert_eq!(row.archived_at, None);
    assert_eq!(row.updated, 2000);
    assert_eq!(store.list_sessions(&alice).await.unwrap().active.len(), 3);
}

#[tokio::test]
async fn titles_are_set_renamed_and_set_only_while_null() {
    let store = store();
    let alice = user(&store, "alice").await;
    let id = session(&store, &alice).await;
    assert!(store.set_title_if_null(&id, "first", 20).await.unwrap());
    assert!(!store.set_title_if_null(&id, "second", 21).await.unwrap());
    let row = store.session(&id).await.unwrap().unwrap();
    assert_eq!(row.title.as_deref(), Some("first"));
    assert_eq!(row.updated, 20);
    store.set_title(&id, "renamed", 30).await.unwrap();
    let row = store.session(&id).await.unwrap().unwrap();
    assert_eq!(row.title.as_deref(), Some("renamed"));
    assert_eq!(row.updated, 30);
    assert_eq!(
        store.set_title("nope", "x", 1).await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        store.set_title_if_null("nope", "x", 1).await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        store.set_archived("nope", true, 1).await.unwrap_err(),
        StoreError::NotFound
    );
    assert!(store.session("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn deleting_a_session_cascades_to_its_messages() {
    let store = store();
    let alice = user(&store, "alice").await;
    let id = session(&store, &alice).await;
    let other = session(&store, &alice).await;
    store.append_user(&id, &[text("q")], "q", 1).await.unwrap();
    store
        .append_assistant(&id, &[text("a")], None, false, 2)
        .await
        .unwrap();
    store
        .append_user(&other, &[text("o")], "o", 3)
        .await
        .unwrap();
    assert_eq!(store.message_stats(&id).await.unwrap().count, 2);
    store.delete_session(&id).await.unwrap();
    assert!(store.session(&id).await.unwrap().is_none());
    assert_eq!(store.message_stats(&id).await.unwrap().count, 0, "cascaded");
    assert_eq!(
        store.message_stats(&other).await.unwrap().count,
        1,
        "the other survives"
    );
    assert_eq!(
        store.delete_session(&id).await.unwrap_err(),
        StoreError::NotFound
    );
}

// ---------- messages ----------

#[tokio::test]
async fn messages_round_trip_with_text_usage_and_the_rejected_flag() {
    let store = store();
    let alice = user(&store, "alice").await;
    let id = session(&store, &alice).await;
    let blocks = vec![
        text("summarise"),
        Block::Text {
            text: "Attached file file:///notes.txt (text/plain):\nbody".into(),
        },
    ];
    let u = store
        .append_user(&id, &blocks, "summarise", 10)
        .await
        .unwrap();
    let reply = vec![
        Block::Thinking {
            text: "hmm".into(),
            signature: Some("sig".into()),
            provider: "bedrock".into(),
            model: "m".into(),
        },
        text("done"),
    ];
    let usage = Usage {
        input: 65,
        output: 4,
        cache_read: 1000,
        cache_write: 7,
    };
    let a = store
        .append_assistant(&id, &reply, Some(usage), false, 11)
        .await
        .unwrap();
    let refused = store
        .append_assistant(&id, &[text("I cannot")], None, true, 12)
        .await
        .unwrap();
    assert!(u < a && a < refused, "ids count up");
    let window = store.load_window(&id, 100, 1 << 20).await.unwrap();
    assert!(window.complete);
    assert_eq!(window.rows.len(), 3);
    let first = &window.rows[0];
    assert_eq!(first.id, u);
    assert_eq!(first.role, MessageRole::User);
    assert_eq!(first.blocks, blocks);
    assert_eq!(first.text.as_deref(), Some("summarise"));
    assert_eq!(first.usage, None);
    assert!(!first.rejected);
    assert_eq!(first.created, 10);
    let second = &window.rows[1];
    assert_eq!(second.role, MessageRole::Assistant);
    assert_eq!(second.blocks, reply);
    assert_eq!(second.text, None);
    assert_eq!(second.usage, Some(usage));
    assert!(window.rows[2].rejected);
    assert_eq!(window.rows[2].usage, None);
    // A stats reading agrees with the rows.
    let stats = store.message_stats(&id).await.unwrap();
    assert_eq!(stats.count, 3);
    let bytes: usize = [&blocks, &reply, &vec![text("I cannot")]]
        .iter()
        .map(|b| serde_json::to_string(b).unwrap().len())
        .sum();
    assert_eq!(stats.bytes, bytes as u64);
    // Marking rejected, including the assistant row, and an empty list.
    store.mark_rejected(&[u, a]).await.unwrap();
    store.mark_rejected(&[]).await.unwrap();
    let window = store.load_window(&id, 100, 1 << 20).await.unwrap();
    assert!(window.rows.iter().all(|r| r.rejected));
    // An unknown session has no rows and no stats, and cannot be appended.
    let empty = store.load_window("nope", 10, 10).await.unwrap();
    assert!(empty.rows.is_empty() && empty.complete);
    assert_eq!(store.message_stats("nope").await.unwrap().count, 0);
    assert_eq!(
        store
            .append_user("nope", &[text("x")], "x", 1)
            .await
            .unwrap_err(),
        StoreError::NotFound
    );
}

#[tokio::test]
async fn the_window_is_bounded_in_rows_and_bytes_inside_the_query() {
    let store = store();
    let alice = user(&store, "alice").await;
    let id = session(&store, &alice).await;
    // Five rows of known stored size: `[{"type":"text","text":"<n a's>"}]`.
    let mut ids = Vec::new();
    let mut sizes = Vec::new();
    for (i, n) in [10usize, 20, 30, 40, 50].iter().enumerate() {
        let blocks = vec![text(&"a".repeat(*n))];
        sizes.push(serde_json::to_string(&blocks).unwrap().len());
        ids.push(
            store
                .append_user(&id, &blocks, "t", i as i64)
                .await
                .unwrap(),
        );
    }
    // Row cap: the newest three.
    let w = store.load_window(&id, 3, usize::MAX).await.unwrap();
    assert_eq!(w.rows.iter().map(|r| r.id).collect::<Vec<_>>(), ids[2..]);
    assert!(!w.complete);
    // Byte cap: newest first, stop before the row that would cross.
    let newest_two = sizes[4] + sizes[3];
    let w = store.load_window(&id, 100, newest_two).await.unwrap();
    assert_eq!(w.rows.iter().map(|r| r.id).collect::<Vec<_>>(), ids[3..]);
    assert!(!w.complete);
    let w = store
        .load_window(&id, 100, newest_two + sizes[2] - 1)
        .await
        .unwrap();
    assert_eq!(w.rows.len(), 2, "the crossing row is excluded");
    // The newest row is always kept, even over the cap.
    let w = store.load_window(&id, 100, 1).await.unwrap();
    assert_eq!(w.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[4]]);
    assert!(!w.complete);
    // Everything fits: complete, oldest first.
    let w = store.load_window(&id, 5, sizes.iter().sum()).await.unwrap();
    assert_eq!(w.rows.iter().map(|r| r.id).collect::<Vec<_>>(), ids);
    assert!(w.complete);
    let w = store.load_window(&id, 4, usize::MAX).await.unwrap();
    assert!(!w.complete, "one row left out");
}

// ---------- credentials and profiles ----------

#[tokio::test]
async fn a_credential_is_sealed_with_a_grant_and_opened_only_through_the_payload_method() {
    let store = store();
    let admin = store
        .create_user("admin", "h", Role::Admin, 1)
        .await
        .unwrap()
        .id;
    let payload = json!({ "region": "us-east-1", "profile": "work" });
    let row = store
        .create_credential(None, &admin, "bedrock", "Bedrock", &payload, 50)
        .await
        .unwrap();
    assert_eq!(row.user_id, None);
    assert_eq!(row.provider, "bedrock");
    assert_eq!(row.label, "Bedrock");
    assert_eq!((row.created, row.updated), (50, 50));
    let listed = store.credentials(None, "bedrock").await.unwrap();
    assert_eq!(listed, vec![row.clone()]);
    assert!(store
        .credentials(Some(&admin), "bedrock")
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .credentials(None, "anthropic")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(store.credential_payload(&row.id).await.unwrap(), payload);
    let debug = format!("{row:?}{listed:?}");
    assert!(
        !debug.contains("us-east-1") && !debug.contains("work"),
        "{debug}"
    );
    // Neither the label nor the provider carries a payload value, and the
    // ciphertext is not the plaintext; the grant row names the creator.
    store
        .run_for_test(|conn| {
            let (label, provider, ciphertext): (String, String, Vec<u8>) = conn
                .query_row(
                    "SELECT label, provider, ciphertext FROM credentials",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert!(!label.contains("us-east-1") && !label.contains("work"));
            assert!(!provider.contains("work"));
            let plain = br#"{"profile":"work","region":"us-east-1"}"#;
            assert!(!ciphertext.windows(plain.len()).any(|w| w == plain));
            let grants: i64 = conn
                .query_row("SELECT COUNT(*) FROM grants", [], |r| r.get(0))
                .unwrap();
            assert_eq!(grants, 1);
        })
        .await
        .unwrap();
    assert_eq!(
        store.credential_payload("nope").await.unwrap_err(),
        StoreError::NotFound
    );
    // An unknown creator writes nothing.
    assert_eq!(
        store
            .create_credential(None, "nope", "bedrock", "Bedrock", &payload, 1)
            .await
            .unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(store.credentials(None, "bedrock").await.unwrap().len(), 1);
    // Deleting removes the grant through the cascade.
    store.delete_credential(&row.id).await.unwrap();
    assert!(store.credentials(None, "bedrock").await.unwrap().is_empty());
    store
        .run_for_test(|conn| {
            let grants: i64 = conn
                .query_row("SELECT COUNT(*) FROM grants", [], |r| r.get(0))
                .unwrap();
            assert_eq!(grants, 0);
        })
        .await
        .unwrap();
    assert_eq!(
        store.delete_credential(&row.id).await.unwrap_err(),
        StoreError::NotFound
    );
}

#[tokio::test]
async fn a_credential_sealed_under_another_key_is_tampered() {
    let keys_a = MasterKey::from_bytes_for_test([1u8; KEY_LEN]).keys();
    let keys_b = MasterKey::from_bytes_for_test([2u8; KEY_LEN]).keys();
    let a = SqliteStore::open_in_memory(keys_a).unwrap();
    let admin = a
        .create_user("admin", "h", Role::Admin, 1)
        .await
        .unwrap()
        .id;
    let row = a
        .create_credential(
            None,
            &admin,
            "bedrock",
            "Bedrock",
            &json!({ "region": "eu-west-1" }),
            1,
        )
        .await
        .unwrap();
    // Copy the sealed row into a store opened under another key.
    let sealed: (Vec<u8>, Vec<u8>) = {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = row.id.clone();
        a.run_for_test(move |conn| {
            let sealed = conn
                .query_row(
                    "SELECT ciphertext, nonce FROM credentials WHERE id = ?1",
                    [id],
                    |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
                )
                .unwrap();
            let _ = tx.send(sealed);
        })
        .await
        .unwrap();
        rx.await.unwrap()
    };
    let b = SqliteStore::open_in_memory(keys_b).unwrap();
    let admin_b = b
        .create_user("admin", "h", Role::Admin, 1)
        .await
        .unwrap()
        .id;
    let id = row.id.clone();
    let (ciphertext, nonce) = sealed;
    b.run_for_test(move |conn| {
        conn.execute(
            "INSERT INTO credentials (id, user_id, provider, label, ciphertext, nonce, created, updated) \
             VALUES (?1, NULL, 'bedrock', 'Bedrock', ?2, ?3, 1, 1)",
            rusqlite::params![id, ciphertext, nonce],
        )
        .unwrap();
        let _ = admin_b;
    })
    .await
    .unwrap();
    assert_eq!(
        b.credential_payload(&row.id).await.unwrap_err(),
        StoreError::Tampered
    );
}

#[tokio::test]
async fn the_global_profile_is_created_once_and_updated_after() {
    let store = store();
    assert!(store.global_profile().await.unwrap().is_none());
    let admin = store
        .create_user("admin", "h", Role::Admin, 1)
        .await
        .unwrap()
        .id;
    let cred = store
        .create_credential(None, &admin, "bedrock", "Bedrock", &json!({}), 1)
        .await
        .unwrap();
    let first = store
        .upsert_global_profile(&NewProfile {
            model: "anthropic.claude-sonnet-5".into(),
            credential_id: Some(cred.id.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(first.user_id, None);
    assert_eq!(first.model, "anthropic.claude-sonnet-5");
    assert_eq!(first.credential_id.as_deref(), Some(cred.id.as_str()));
    assert_eq!(first.thinking, None);
    assert_eq!(first.thinking_budget, None);
    assert_eq!(first.max_output_tokens, None);
    let second = store
        .upsert_global_profile(&NewProfile {
            model: "anthropic.claude-opus-5".into(),
            credential_id: None,
            thinking: Some("off".into()),
            thinking_budget: Some(2048),
            max_output_tokens: Some(4096),
        })
        .await
        .unwrap();
    assert_eq!(second.id, first.id, "one global row, updated in place");
    let read = store.global_profile().await.unwrap().unwrap();
    assert_eq!(read, second);
    assert_eq!(read.thinking.as_deref(), Some("off"));
    assert_eq!(read.thinking_budget, Some(2048));
    store
        .run_for_test(|conn| {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
        })
        .await
        .unwrap();
}

#[test]
fn errors_render_one_line_each() {
    assert_eq!(StoreError::NotFound.to_string(), "no such row");
    assert_eq!(StoreError::Closed.to_string(), "the store is closed");
    assert!(StoreError::Conflict("the user name `x`".into())
        .to_string()
        .contains("already taken"));
    assert_eq!(StoreError::Invalid("why".into()).to_string(), "why");
    assert!(StoreError::Tampered.to_string().contains("master key"));
    assert_eq!(
        StoreError::Internal("op: boom".into()).to_string(),
        "op: boom"
    );
    assert_eq!(Role::parse("admin"), Some(Role::Admin));
    assert_eq!(Role::parse("root"), None);
    assert_eq!(Role::User.as_str(), "user");
    assert_eq!(MessageRole::Assistant.as_str(), "assistant");
    let _: Value = json!(null);
}
