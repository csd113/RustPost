
CREATE TABLE IF NOT EXISTS post_embeds (
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

CREATE INDEX IF NOT EXISTS idx_post_embeds_post ON post_embeds(post_id, position);
CREATE INDEX IF NOT EXISTS idx_post_embeds_video ON post_embeds(provider, video_id);
