
CREATE TABLE IF NOT EXISTS follow_requests (
    requester_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    target_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (requester_id, target_id)
);

CREATE INDEX IF NOT EXISTS idx_follow_requests_target ON follow_requests(target_id, requester_id);

CREATE TABLE IF NOT EXISTS username_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    username TEXT NOT NULL,
    normalized_username TEXT NOT NULL,
    changed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (user_id, normalized_username)
);

CREATE INDEX IF NOT EXISTS idx_username_history_normalized ON username_history(normalized_username);

CREATE TABLE IF NOT EXISTS instance_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS account_imports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    archive_id TEXT NOT NULL,
    format_version INTEGER NOT NULL,
    imported_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    posts_imported INTEGER NOT NULL DEFAULT 0,
    media_imported INTEGER NOT NULL DEFAULT 0,
    follows_imported INTEGER NOT NULL DEFAULT 0,
    follows_pending INTEGER NOT NULL DEFAULT 0,
    follows_skipped INTEGER NOT NULL DEFAULT 0,
    UNIQUE (user_id, archive_id)
);

CREATE INDEX IF NOT EXISTS idx_account_imports_user ON account_imports(user_id, imported_at DESC);
