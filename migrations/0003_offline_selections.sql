CREATE TABLE offline_selections (target TEXT PRIMARY KEY, pinned INTEGER NOT NULL CHECK(pinned IN (0,1)));
CREATE TABLE offline_commands (request_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
PRAGMA user_version=3;
