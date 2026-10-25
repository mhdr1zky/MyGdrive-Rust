CREATE TABLE IF NOT EXISTS dehydration_requests (
    file_id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL DEFAULT '',
    journal TEXT
);
PRAGMA user_version=4;
