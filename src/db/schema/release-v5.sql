CREATE TABLE drafts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    text TEXT NOT NULL DEFAULT '',
    parent_post_id INTEGER,
    scheduled_at INTEGER,
    status TEXT NOT NULL DEFAULT 'draft' CHECK(status IN ('draft','scheduled','failed','published')),
    revision INTEGER NOT NULL DEFAULT 1,
    client_key TEXT NOT NULL,
    published_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    error TEXT NOT NULL DEFAULT '',
    poll_options TEXT NOT NULL DEFAULT '',
    poll_multiple INTEGER NOT NULL DEFAULT 0,
    poll_closes_at INTEGER,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, client_key)
);
CREATE INDEX idx_drafts_owner ON drafts(user_id, id);
CREATE INDEX idx_drafts_due ON drafts(status, scheduled_at, id);
CREATE TABLE draft_media (
    draft_id INTEGER NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
    media_id INTEGER NOT NULL REFERENCES media(id),
    position INTEGER NOT NULL,
    PRIMARY KEY(draft_id, media_id),
    UNIQUE(media_id),
    UNIQUE(draft_id, position)
);
CREATE TABLE polls (
    post_id INTEGER PRIMARY KEY REFERENCES posts(id) ON DELETE CASCADE,
    multiple INTEGER NOT NULL DEFAULT 0 CHECK(multiple IN (0,1)),
    closes_at INTEGER
);
CREATE TABLE poll_options (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    post_id INTEGER NOT NULL REFERENCES polls(post_id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    position INTEGER NOT NULL,
    UNIQUE(post_id, position),
    UNIQUE(id, post_id)
);
CREATE TABLE poll_votes (
    post_id INTEGER NOT NULL REFERENCES polls(post_id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    option_id INTEGER NOT NULL,
    PRIMARY KEY(post_id, user_id, option_id),
    FOREIGN KEY(option_id, post_id) REFERENCES poll_options(id, post_id) ON DELETE CASCADE
);
CREATE TABLE media_work (
    media_id INTEGER PRIMARY KEY REFERENCES media(id) ON DELETE CASCADE,
    status TEXT NOT NULL CHECK(status IN ('queued','running','failed','done')),
    attempts INTEGER NOT NULL DEFAULT 0,
    run_after INTEGER NOT NULL DEFAULT 0,
    lease_token TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_media_work_due ON media_work(status, run_after, media_id);

CREATE TABLE post_requests (
    actor TEXT NOT NULL,
    request_key TEXT NOT NULL,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    PRIMARY KEY(actor, request_key)
);
CREATE TABLE recoverable_deletions (
    post_id INTEGER PRIMARY KEY REFERENCES posts(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE INDEX idx_recoverable_due ON recoverable_deletions(expires_at, post_id);

CREATE TABLE storage_reservations (
    id TEXT PRIMARY KEY,
    owner_user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    bytes INTEGER NOT NULL CHECK(bytes>=0)
);
