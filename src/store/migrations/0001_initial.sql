-- Every table of the plan's data model, from the first alpha with a store.
-- Every user-owned table carries user_id from the start; nothing is
-- retrofitted. Timestamps are INTEGER milliseconds since the Unix epoch,
-- as the transcript already uses. Ids are 32 lowercase hexadecimal
-- characters except messages, which count.

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'user')),
    session_epoch INTEGER NOT NULL DEFAULT 0,
    email TEXT,
    settings TEXT NOT NULL DEFAULT '{}',
    created INTEGER NOT NULL
);

CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    root TEXT NOT NULL,
    permission_policy TEXT NOT NULL DEFAULT '[]',
    tool_limits TEXT
);

CREATE TABLE credentials (
    id TEXT PRIMARY KEY,
    user_id TEXT REFERENCES users(id),
    provider TEXT NOT NULL,
    label TEXT NOT NULL,
    ciphertext BLOB NOT NULL,
    nonce BLOB NOT NULL,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);

CREATE TABLE profiles (
    id TEXT PRIMARY KEY,
    user_id TEXT REFERENCES users(id),
    credential_id TEXT REFERENCES credentials(id),
    model TEXT NOT NULL,
    thinking TEXT,
    thinking_budget INTEGER,
    max_output_tokens INTEGER,
    temperature REAL,
    system_prompt TEXT
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    workspace_id TEXT REFERENCES workspaces(id),
    profile_id TEXT REFERENCES profiles(id),
    title TEXT,
    archived_at INTEGER,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    content TEXT NOT NULL,
    text TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    compaction_id INTEGER,
    rejected INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL
);

CREATE TABLE grants (
    user_id TEXT NOT NULL REFERENCES users(id),
    credential_id TEXT NOT NULL REFERENCES credentials(id) ON DELETE CASCADE,
    model_allowlist TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (user_id, credential_id)
);

CREATE TABLE mcp_grants (
    user_id TEXT NOT NULL REFERENCES users(id),
    server_name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    tool_allowlist TEXT,
    PRIMARY KEY (user_id, server_name)
);

CREATE TABLE mcp_servers (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    ciphertext BLOB NOT NULL,
    nonce BLOB NOT NULL,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);

CREATE TABLE memories (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    workspace_id TEXT REFERENCES workspaces(id),
    path TEXT NOT NULL,
    title TEXT,
    summary TEXT,
    content_hash TEXT,
    mtime INTEGER,
    size INTEGER
);

CREATE INDEX messages_by_session ON messages(session_id, id);
CREATE INDEX sessions_by_user ON sessions(user_id, archived_at);
CREATE INDEX credentials_by_owner ON credentials(user_id, provider);
