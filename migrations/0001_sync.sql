CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    relative_path TEXT NOT NULL UNIQUE,
    remote_id TEXT UNIQUE,
    generation INTEGER NOT NULL,
    sync_status TEXT NOT NULL,
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS remote_observations (
    id TEXT PRIMARY KEY, data TEXT NOT NULL, pending INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS sync_operations (
    id TEXT PRIMARY KEY, operation_key TEXT NOT NULL UNIQUE,
    file_id TEXT NOT NULL REFERENCES files(id),
    stage TEXT NOT NULL, data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS operation_stage ON sync_operations(stage);
CREATE TABLE IF NOT EXISTS sync_cursors (name TEXT PRIMARY KEY, token TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS pending_reconciliations (file_id TEXT PRIMARY KEY REFERENCES files(id));
CREATE TABLE IF NOT EXISTS cache_objects (
    key TEXT PRIMARY KEY, data TEXT NOT NULL, complete INTEGER NOT NULL CHECK(complete=1)
);
CREATE TABLE IF NOT EXISTS conflicts (
    op_id TEXT PRIMARY KEY, file_id TEXT NOT NULL, relative_path TEXT NOT NULL, resolved INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS displaced_files (path TEXT PRIMARY KEY, data TEXT NOT NULL);
PRAGMA user_version=1;
