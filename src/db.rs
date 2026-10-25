use crate::{model::*, Error, Result};
use futures_util::TryStreamExt;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    Row, SqliteConnection, SqlitePool,
};
use std::{collections::HashSet, path::Path, time::Duration};

pub const RECONCILIATION_FILTER: &str = "sync_status!='online_only' OR coalesce(json_extract(data,'$.materialized'),0)!=0 OR json_extract(data,'$.fingerprint') IS NOT NULL OR (json_extract(data,'$.baseline.relative_path') IS NOT NULL AND relative_path!=json_extract(data,'$.baseline.relative_path'))";

pub(crate) enum JsonTable {
    Files,
    Remotes,
}

/// Bulk delivery avoids one async worker wake per row. Indexed keyset pages
/// bound raw JSON buffers without OFFSET rescans of an entire large account.
pub(crate) async fn read_json_rows<T: serde::de::DeserializeOwned>(
    connection: &mut SqliteConnection,
    table: JsonTable,
) -> Result<Vec<T>> {
    let (first, next) = match table {
        JsonTable::Files => ("SELECT relative_path,data FROM files ORDER BY relative_path LIMIT 1024", "SELECT relative_path,data FROM files WHERE relative_path>? ORDER BY relative_path LIMIT 1024"),
        JsonTable::Remotes => ("SELECT id,data FROM remote_observations ORDER BY id LIMIT 1024", "SELECT id,data FROM remote_observations WHERE id>? ORDER BY id LIMIT 1024"),
    };
    let mut cursor: Option<String> = None;
    let mut output = Vec::new();
    loop {
        let batch: Vec<(String, String)> = match &cursor {
            Some(cursor) => {
                sqlx::query_as(next)
                    .bind(cursor)
                    .fetch_all(&mut *connection)
                    .await?
            }
            None => sqlx::query_as(first).fetch_all(&mut *connection).await?,
        };
        let count = batch.len();
        for (key, data) in batch {
            cursor = Some(key);
            output.push(serde_json::from_str(&data)?);
        }
        if count < 1024 {
            return Ok(output);
        }
    }
}

#[derive(Clone)]
pub struct Database {
    pub pool: SqlitePool,
    pub account: String,
}

impl Database {
    pub async fn reset_inventory(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM sync_cursors WHERE name IN ('changes','inventory','bootstrap_start')",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn read_only(path: &Path, account: &str) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let db = Self {
            pool,
            account: account.into(),
        };
        if db.setting("account").await?.as_deref() != Some(account) {
            return Err(Error::Invalid(
                "state belongs to a different account".into(),
            ));
        }
        Ok(db)
    }
    pub async fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key=?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }
    pub async fn open(path: &Path, account: &str) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        // One connection serializes DB writers, but no HTTP/hash runs in a transaction.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await?;
        if version > 4 {
            return Err(Error::Invalid(
                "database schema is newer than this application".into(),
            ));
        }
        let mut tx = pool.begin().await?;
        if version == 0 {
            sqlx::raw_sql(include_str!("../migrations/0001_sync.sql"))
                .execute(&mut *tx)
                .await?;
        }
        if version < 2 {
            sqlx::raw_sql(include_str!("../migrations/0002_cache_policy.sql"))
                .execute(&mut *tx)
                .await?;
        }
        if version < 3 {
            sqlx::raw_sql(include_str!("../migrations/0003_offline_selections.sql"))
                .execute(&mut *tx)
                .await?;
        }
        if version < 4 {
            sqlx::raw_sql(include_str!("../migrations/0004_dehydration.sql"))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        sqlx::raw_sql(crate::directory_sync::SCHEMA)
            .execute(&pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS files_sync_status ON files(sync_status)")
            .execute(&pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS operation_file_stage ON sync_operations(file_id,stage)",
        )
        .execute(&pool)
        .await?;
        // Optional performance index is idempotent for existing schema-3 profiles too.
        sqlx::query("CREATE INDEX IF NOT EXISTS offline_remote_parent ON remote_observations(json_extract(data,'$.parents[0]'))").execute(&pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS active_operations_file ON sync_operations(file_id) WHERE stage NOT IN ('done','cancelled')").execute(&pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS scanner_materialized_files ON files(relative_path) WHERE json_extract(data,'$.materialized')=1 AND sync_status!='tombstone'")
            .execute(&pool).await?;
        sqlx::query(&format!("CREATE INDEX IF NOT EXISTS reconciliation_candidates ON files(relative_path) WHERE {RECONCILIATION_FILTER}"))
            .execute(&pool).await?;
        let db = Self {
            pool,
            account: account.into(),
        };
        db.bind("account", account).await?;
        sqlx::query(
            "INSERT OR IGNORE INTO settings(key,value) VALUES('storage_mode','online_only')",
        )
        .execute(&db.pool)
        .await?;
        Ok(db)
    }

    pub async fn bind(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO settings(key,value) VALUES(?,?)")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        let actual: String = sqlx::query_scalar("SELECT value FROM settings WHERE key=?")
            .bind(key)
            .fetch_one(&self.pool)
            .await?;
        if actual != value {
            return Err(Error::Invalid(format!(
                "state directory is bound to another {key}"
            )));
        }
        Ok(())
    }

    pub(crate) async fn write_file(
        conn: &mut SqliteConnection,
        account: &str,
        file: &LocalFile,
    ) -> Result<()> {
        sqlx::query("INSERT INTO files(id,account_id,relative_path,remote_id,generation,sync_status,data) VALUES(?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET relative_path=excluded.relative_path,remote_id=excluded.remote_id,generation=excluded.generation,sync_status=excluded.sync_status,data=excluded.data WHERE files.data IS NOT excluded.data")
            .bind(&file.id).bind(account).bind(&file.relative_path).bind(&file.remote_id)
            .bind(file.generation).bind(&file.status).bind(serde_json::to_string(file)?)
            .execute(&mut *conn).await?;
        sqlx::query("INSERT OR IGNORE INTO pending_reconciliations(file_id) VALUES(?)")
            .bind(&file.id)
            .execute(conn)
            .await?;
        Ok(())
    }

    pub async fn save_file(&self, file: &LocalFile) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        Self::write_file(&mut tx, &self.account, file).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Batch first-time metadata registration without repeatedly reading all file rows.
    pub async fn register_files(&self, files: &[LocalFile]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        // Bound each writer lease so status/pause requests can interleave during
        // a large bootstrap. Restart simply registers the remaining remote IDs.
        for chunk in files.chunks(512) {
            let mut tx = self.pool.begin().await?;
            for file in chunk {
                Self::write_file(&mut tx, &self.account, file).await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }

    pub async fn finish_move(&self, file: &LocalFile, op: &Operation) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        Self::write_file(&mut tx, &self.account, file).await?;
        sqlx::query("UPDATE sync_operations SET stage=?,data=? WHERE id=?")
            .bind(&op.stage)
            .bind(serde_json::to_string(op)?)
            .bind(&op.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn files(&self) -> Result<Vec<LocalFile>> {
        let mut connection = self.pool.acquire().await?;
        read_json_rows(&mut connection, JsonTable::Files).await
    }

    /// Online-only records without local intent need no reconciliation. Read
    /// the small partial index, then add selected files through primary keys.
    pub(crate) async fn reconciliation_files(
        &self,
        requested: &HashSet<String>,
        mirror: bool,
    ) -> Result<Vec<LocalFile>> {
        if mirror {
            return self.files().await;
        }
        let sql =
            format!("SELECT data FROM files WHERE {RECONCILIATION_FILTER} ORDER BY relative_path");
        let mut rows = sqlx::query_scalar::<_, String>(&sql).fetch(&self.pool);
        let mut files = Vec::<LocalFile>::new();
        while let Some(data) = rows.try_next().await? {
            files.push(serde_json::from_str(&data)?);
        }
        drop(rows);
        let mut known: HashSet<_> = files.iter().map(|f| f.id.clone()).collect();
        let extras: Vec<_> = requested.iter().filter(|id| !known.contains(*id)).collect();
        for chunk in extras.chunks(512) {
            let mut query =
                sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT data FROM files WHERE id IN (");
            let mut separated = query.separated(",");
            for id in chunk {
                separated.push_bind(*id);
            }
            separated.push_unseparated(")");
            let mut rows = query.build_query_scalar::<String>().fetch(&self.pool);
            while let Some(data) = rows.try_next().await? {
                let file: LocalFile = serde_json::from_str(&data)?;
                if known.insert(file.id.clone()) {
                    files.push(file);
                }
            }
        }
        files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        Ok(files)
    }
    /// Watcher scans only need files that have existed in the backing tree.
    /// The partial index avoids decoding the metadata-only cloud inventory.
    pub async fn materialized_files(&self) -> Result<Vec<LocalFile>> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM files WHERE json_extract(data,'$.materialized')=1 AND sync_status!='tombstone' ORDER BY relative_path")
            .fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_str(&row)?))
            .collect()
    }

    /// Include metadata-only rows when checking whether a rename destination
    /// already owns a cloud identity. Bounded batches use the unique path index.
    pub async fn occupied_file_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        let mut occupied = std::collections::HashSet::new();
        for batch in paths.chunks(512) {
            let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                "SELECT relative_path FROM files WHERE relative_path IN (",
            );
            let mut separated = query.separated(",");
            for path in batch {
                separated.push_bind(path);
            }
            separated.push_unseparated(")");
            let rows: Vec<String> = query.build_query_scalar().fetch_all(&self.pool).await?;
            occupied.extend(rows);
        }
        Ok(occupied)
    }
    pub async fn file(&self, id: &str) -> Result<LocalFile> {
        let data: String = sqlx::query_scalar("SELECT data FROM files WHERE id=?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(serde_json::from_str(&data)?)
    }
    pub async fn by_remote(&self, id: &str) -> Result<Option<LocalFile>> {
        let data: Option<String> = sqlx::query_scalar("SELECT data FROM files WHERE remote_id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        data.map(|s| serde_json::from_str(&s).map_err(Error::from))
            .transpose()
    }

    /// Called by the serialized filesystem worker. Content-identical internal writes are clean.
    pub async fn record_local(&self, path: &str, fingerprint: Option<Fingerprint>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let data: Option<String> =
            sqlx::query_scalar("SELECT data FROM files WHERE relative_path=?")
                .bind(path)
                .fetch_optional(&mut *tx)
                .await?;
        let mut file = if let Some(data) = data {
            serde_json::from_str::<LocalFile>(&data)?
        } else {
            if fingerprint.is_none() {
                return Ok(());
            }
            LocalFile {
                id: uuid::Uuid::new_v4().to_string(),
                relative_path: path.into(),
                remote_id: None,
                generation: 0,
                fingerprint: None,
                baseline: None,
                status: "new_local".into(),
                materialized: true,
                review_required: false,
                available_offline: false,
            }
        };
        // A local file can disappear before its first upload (including editor
        // sidecars tracked by an older version). No cloud deletion is needed,
        // but an unfinished journal may still own a reserved/committed cloud ID.
        // Check that journal in this transaction before retiring the local row.
        let unpublished_deletion = fingerprint.is_none()
            && file.materialized
            && file.remote_id.is_none()
            && file.baseline.is_none()
            && !file.review_required
            && file.status != "recovery_review"
            && sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sync_operations WHERE file_id=? AND stage NOT IN ('done','cancelled')",
            )
            .bind(&file.id)
            .fetch_one(&mut *tx)
            .await?
                == 0;
        if file.fingerprint == fingerprint && (!unpublished_deletion || file.status == "tombstone")
        {
            return Ok(());
        }
        file.review_required |= file.status == "recovery_review";
        let content_changed =
            file.fingerprint.as_ref().map(|f| &f.digest) != fingerprint.as_ref().map(|f| &f.digest);
        // Inode/mtime changes alone must not cancel an in-flight content-identical operation.
        if content_changed {
            file.generation += 1;
        }
        file.fingerprint = fingerprint;
        file.materialized = file.materialized || file.fingerprint.is_some();
        file.status = if unpublished_deletion {
            "tombstone".into()
        } else {
            status(&file)
        };
        Self::write_file(&mut tx, &self.account, &file).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn acknowledge(
        &self,
        id: &str,
        generation: i64,
        digest: Digest,
        remote: RemoteFile,
        baseline_path: String,
        remote_path: Option<String>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let data: String = sqlx::query_scalar("SELECT data FROM files WHERE id=?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let mut file: LocalFile = serde_json::from_str(&data)?;
        file.remote_id = Some(remote.id.clone());
        file.baseline = Some(Baseline {
            digest,
            remote,
            relative_path: baseline_path,
            remote_path,
        });
        file.status = status(&file);
        if file.generation != generation && file.status == "synced" {
            file.status = "modified_local".into();
        }
        Self::write_file(&mut tx, &self.account, &file).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn operation(&self, op: &Operation) -> Result<Operation> {
        sqlx::query("INSERT OR IGNORE INTO sync_operations(id,operation_key,file_id,stage,data) VALUES(?,?,?,?,?)")
            .bind(&op.id).bind(&op.key).bind(&op.file_id).bind(&op.stage).bind(serde_json::to_string(op)?)
            .execute(&self.pool).await?;
        let data: String =
            sqlx::query_scalar("SELECT data FROM sync_operations WHERE operation_key=?")
                .bind(&op.key)
                .fetch_one(&self.pool)
                .await?;
        Ok(serde_json::from_str(&data)?)
    }
    pub async fn save_operation(&self, op: &Operation) -> Result<()> {
        sqlx::query("UPDATE sync_operations SET operation_key=?,stage=?,data=? WHERE id=?")
            .bind(&op.key)
            .bind(&op.stage)
            .bind(serde_json::to_string(op)?)
            .bind(&op.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn operations(&self) -> Result<Vec<Operation>> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM sync_operations WHERE stage NOT IN ('done','cancelled') ORDER BY rowid")
            .fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    pub(crate) async fn file_operations(&self, id: &str) -> Result<Vec<Operation>> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM sync_operations WHERE file_id=? AND stage NOT IN ('done','cancelled') ORDER BY rowid")
            .bind(id).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    pub(crate) async fn operation_by_id(&self, id: &str) -> Result<Operation> {
        let data: String = sqlx::query_scalar("SELECT data FROM sync_operations WHERE id=?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(serde_json::from_str(&data)?)
    }
    pub async fn conflict(&self, op: &Operation, relative: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO conflicts(op_id,file_id,relative_path) VALUES(?,?,?)")
            .bind(&op.id)
            .bind(&op.file_id)
            .bind(relative)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn displaced(&self, entry: &DisplacedFile) -> Result<()> {
        sqlx::query("INSERT INTO displaced_files(path,data) VALUES(?,?) ON CONFLICT(path) DO UPDATE SET data=excluded.data")
            .bind(entry.path.to_string_lossy().as_ref()).bind(serde_json::to_string(entry)?)
            .execute(&self.pool).await?;
        Ok(())
    }
    pub async fn displaced_files(&self) -> Result<Vec<DisplacedFile>> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT data FROM displaced_files")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    pub async fn cache(&self, key: &str, snapshot: &Snapshot, remote: &RemoteFile) -> Result<()> {
        sqlx::query("INSERT INTO cache_objects(key,data,complete,remote_id,remote_version,last_access_ms) VALUES(?,?,1,?,?,?) ON CONFLICT(key) DO UPDATE SET data=excluded.data,remote_id=excluded.remote_id,remote_version=excluded.remote_version,last_access_ms=excluded.last_access_ms")
            .bind(key).bind(serde_json::to_string(snapshot)?).bind(&remote.id).bind(&remote.version).bind(crate::now_ms()).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn cached_objects(&self) -> Result<Vec<CachedObject>> {
        sqlx::query("SELECT key,data,remote_id,last_access_ms FROM cache_objects WHERE complete=1 ORDER BY last_access_ms,key")
            .fetch_all(&self.pool).await?.into_iter().map(|row| Ok(CachedObject {
                key:row.get("key"),snapshot:serde_json::from_str(row.get("data"))?,
                remote_id:row.get("remote_id"),last_access_ms:row.get("last_access_ms"),
            })).collect()
    }

    pub async fn forget_cache(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM cache_objects WHERE key=?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn cursor(&self, name: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT token FROM sync_cursors WHERE name=?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    /// Observations and the next cursor commit together; unfinished transfers are replayable.
    pub async fn remote_page(
        &self,
        files: &[RemoteFile],
        cursor_name: &str,
        token: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for remote in files {
            sqlx::query("INSERT INTO remote_observations(id,data,pending) VALUES(?,?,1) ON CONFLICT(id) DO UPDATE SET data=excluded.data,pending=1")
                .bind(&remote.id).bind(serde_json::to_string(remote)?).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO sync_cursors(name,token) VALUES(?,?) ON CONFLICT(name) DO UPDATE SET token=excluded.token")
            .bind(cursor_name).bind(token).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn remotes(&self) -> Result<Vec<RemoteFile>> {
        let mut connection = self.pool.acquire().await?;
        read_json_rows(&mut connection, JsonTable::Remotes).await
    }
}

pub fn status(file: &LocalFile) -> String {
    if file.review_required {
        return "recovery_review".into();
    }
    if file.fingerprint.is_none() {
        return if file.materialized {
            "deleted_local"
        } else {
            "remote_only"
        }
        .into();
    }
    match &file.baseline {
        None => "new_local",
        Some(base)
            if file.fingerprint.as_ref().unwrap().digest != base.digest
                || file.relative_path != base.relative_path =>
        {
            "modified_local"
        }
        _ => "synced",
    }
    .into()
}
