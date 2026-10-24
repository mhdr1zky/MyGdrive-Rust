ALTER TABLE cache_objects ADD COLUMN remote_id TEXT;
ALTER TABLE cache_objects ADD COLUMN remote_version TEXT;
ALTER TABLE cache_objects ADD COLUMN last_access_ms INTEGER NOT NULL DEFAULT 0;
CREATE INDEX cache_lru ON cache_objects(last_access_ms, key);
PRAGMA user_version=2;
