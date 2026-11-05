
CREATE INDEX IF NOT EXISTS idx_posts_root ON posts(root_post_id);
CREATE INDEX IF NOT EXISTS idx_media_owner ON media(owner_user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_post_media_media ON post_media(media_id);
CREATE INDEX IF NOT EXISTS idx_rate_limit_created ON rate_limit_events(created_at);
