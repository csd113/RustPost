
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO schema_migrations (version) VALUES (5);

CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL,
    normalized_username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    bio TEXT NOT NULL DEFAULT '',
    website TEXT NOT NULL DEFAULT '',
    profile_picture_media_id INTEGER,
    banner_media_id INTEGER,
    pinned_post_id INTEGER,
    is_admin INTEGER NOT NULL DEFAULT 0,
    is_suspended INTEGER NOT NULL DEFAULT 0,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    theme TEXT NOT NULL DEFAULT 'light' CHECK (theme IN ('light', 'dark')),
    location TEXT NOT NULL DEFAULT '',
    nsfw_blur_enabled INTEGER NOT NULL DEFAULT 1,
    onboarding_completed_at TEXT,
    liked_posts_public INTEGER NOT NULL DEFAULT 1 CHECK (liked_posts_public IN (0, 1)),
    follow_approval_required INTEGER NOT NULL DEFAULT 0,
    must_change_password INTEGER NOT NULL DEFAULT 0,
    deletion_requested_at TEXT,
    deletion_scheduled_at TEXT
);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    csrf_token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TEXT NOT NULL,
    revoked_at TEXT,
    previous_csrf_token_hash TEXT,
    delete_account_token_hash TEXT,
    delete_account_token_expires_at TEXT
);

CREATE TABLE posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    anonymous_label TEXT,
    text TEXT NOT NULL,
    parent_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    root_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    deleted_at TEXT,
    edited_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    quote_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL
);

CREATE TABLE media (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    original_filename TEXT NOT NULL,
    stored_path TEXT NOT NULL,
    public_path TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    media_kind TEXT NOT NULL,
    byte_len INTEGER NOT NULL,
    alt_text TEXT NOT NULL DEFAULT '',
    conversion_state TEXT NOT NULL DEFAULT 'original',
    ffmpeg_stderr TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    thumbnail_path TEXT,
    thumbnail_public_path TEXT,
    is_nsfw INTEGER NOT NULL DEFAULT 0,
    original_path TEXT,
    original_public_path TEXT,
    original_sha256 TEXT NOT NULL DEFAULT '',
    normalized_sha256 TEXT,
    canonical_media_id INTEGER REFERENCES media(id) ON DELETE SET NULL
);

CREATE TABLE post_media (
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (post_id, media_id)
);

CREATE TABLE post_embeds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    provider TEXT NOT NULL CHECK (provider = 'youtube'),
    video_id TEXT NOT NULL,
    original_url TEXT NOT NULL,
    canonical_url TEXT NOT NULL,
    title TEXT,
    thumbnail_url TEXT NOT NULL,
    embed_url TEXT NOT NULL,
    position INTEGER NOT NULL,
    fetched_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(post_id, provider, video_id),
    UNIQUE(post_id, position)
);

CREATE TABLE reposts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, post_id)
);

CREATE TABLE follows (
    follower_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    followed_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (follower_id, followed_id)
);

CREATE TABLE blocks (
    blocker_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    blocked_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (blocker_id, blocked_id)
);

CREATE TABLE mutes (
    muter_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    muted_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (muter_id, muted_id)
);

CREATE TABLE likes (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, post_id)
);

CREATE TABLE bookmarks (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, post_id)
);

CREATE TABLE notifications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    read_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE rate_limit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    actor TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE admin_audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    admin_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE media_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id INTEGER REFERENCES media(id) ON DELETE SET NULL,
    status TEXT NOT NULL,
    stderr_summary TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at TEXT
);

CREATE TABLE reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    reporter_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    reason TEXT NOT NULL,
    dismissed_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE VIRTUAL TABLE IF NOT EXISTS posts_fts USING fts5(text, content='posts', content_rowid='id');

CREATE TRIGGER posts_ai AFTER INSERT ON posts BEGIN
  INSERT INTO posts_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER posts_ad AFTER DELETE ON posts BEGIN
  INSERT INTO posts_fts(posts_fts, rowid, text) VALUES('delete', old.id, old.text);
END;
CREATE TRIGGER posts_au AFTER UPDATE ON posts BEGIN
  INSERT INTO posts_fts(posts_fts, rowid, text) VALUES('delete', old.id, old.text);
  INSERT INTO posts_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE INDEX idx_posts_created ON posts(created_at DESC, id DESC);
CREATE INDEX idx_posts_user_created ON posts(user_id, created_at DESC, id DESC);
CREATE INDEX idx_posts_parent ON posts(parent_post_id, created_at ASC, id ASC);
CREATE INDEX idx_posts_root ON posts(root_post_id);
CREATE INDEX idx_follows_followed ON follows(followed_id, follower_id);
CREATE INDEX idx_blocks_blocked ON blocks(blocked_id, blocker_id);
CREATE INDEX idx_mutes_muted ON mutes(muted_id, muter_id);
CREATE INDEX idx_notifications_user_read_created ON notifications(user_id, read_at, created_at DESC);
CREATE INDEX idx_reposts_post ON reposts(post_id, user_id);
CREATE INDEX idx_likes_post ON likes(post_id, user_id);
CREATE INDEX idx_bookmarks_post ON bookmarks(post_id, user_id);
CREATE INDEX idx_rate_limit_scope_actor_created ON rate_limit_events(scope, actor, created_at);
CREATE INDEX idx_post_embeds_post ON post_embeds(post_id, position);
CREATE INDEX idx_post_embeds_video ON post_embeds(provider, video_id);
CREATE TABLE muted_words (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    term TEXT NOT NULL,
    normalized_term TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, normalized_term)
);

CREATE INDEX idx_muted_words_user ON muted_words(user_id, normalized_term);

CREATE INDEX idx_posts_quote ON posts(quote_post_id, created_at DESC, id DESC);
CREATE UNIQUE INDEX idx_posts_quote_dedupe
    ON posts(user_id, quote_post_id, text)
    WHERE quote_post_id IS NOT NULL AND is_deleted = 0;

CREATE INDEX idx_notifications_user_id_desc ON notifications(user_id, id DESC);
CREATE INDEX idx_notifications_dedupe ON notifications(user_id, actor_user_id, post_id, kind);

CREATE INDEX idx_media_is_nsfw ON media(is_nsfw);
CREATE INDEX idx_media_owner ON media(owner_user_id);
CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_post_media_media ON post_media(media_id);
CREATE INDEX idx_rate_limit_created ON rate_limit_events(created_at);

CREATE INDEX idx_media_original_sha256
    ON media(media_kind, original_sha256)
    WHERE original_sha256 != '';
CREATE INDEX idx_media_normalized_sha256
    ON media(media_kind, normalized_sha256)
    WHERE normalized_sha256 IS NOT NULL AND normalized_sha256 != '';
CREATE INDEX idx_media_canonical_media_id ON media(canonical_media_id);

CREATE UNIQUE INDEX idx_media_unique_canonical_original_sha256
    ON media(media_kind, original_sha256)
    WHERE canonical_media_id IS NULL AND original_sha256 != '';
CREATE UNIQUE INDEX idx_media_unique_canonical_normalized_sha256
    ON media(media_kind, normalized_sha256)
    WHERE canonical_media_id IS NULL AND normalized_sha256 IS NOT NULL AND normalized_sha256 != '';

CREATE TRIGGER media_canonical_ref_insert
BEFORE INSERT ON media
WHEN NEW.canonical_media_id IS NOT NULL
BEGIN
    SELECT CASE
        WHEN NEW.id = NEW.canonical_media_id
        THEN RAISE(ABORT, 'media cannot reference itself as canonical')
    END;
    SELECT CASE
        WHEN NOT EXISTS (
            SELECT 1
            FROM media canonical
            WHERE canonical.id = NEW.canonical_media_id
              AND canonical.canonical_media_id IS NULL
              AND canonical.media_kind = NEW.media_kind
        )
        THEN RAISE(ABORT, 'media canonical reference must point to same-kind canonical media')
    END;
END;

CREATE TRIGGER media_canonical_ref_update
BEFORE UPDATE OF canonical_media_id, media_kind ON media
WHEN NEW.canonical_media_id IS NOT NULL
BEGIN
    SELECT CASE
        WHEN NEW.id = NEW.canonical_media_id
        THEN RAISE(ABORT, 'media cannot reference itself as canonical')
    END;
    SELECT CASE
        WHEN NOT EXISTS (
            SELECT 1
            FROM media canonical
            WHERE canonical.id = NEW.canonical_media_id
              AND canonical.canonical_media_id IS NULL
              AND canonical.media_kind = NEW.media_kind
        )
        THEN RAISE(ABORT, 'media canonical reference must point to same-kind canonical media')
    END;
END;

CREATE TABLE follow_requests (
    requester_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    target_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (requester_id, target_id)
);

CREATE INDEX idx_follow_requests_target ON follow_requests(target_id, requester_id);

CREATE TABLE username_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    username TEXT NOT NULL,
    normalized_username TEXT NOT NULL,
    changed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (user_id, normalized_username)
);

CREATE INDEX idx_username_history_normalized ON username_history(normalized_username);

CREATE TABLE instance_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE account_imports (
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

CREATE INDEX idx_account_imports_user ON account_imports(user_id, imported_at DESC);

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
