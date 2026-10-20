//! Explicit desktop maintenance. Inspection never migrates or creates a database.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection};

use crate::DataDirInstanceGuard;

#[derive(Debug, Serialize)]
pub struct MessageUsage {
    pub message_type: String,
    pub count: u64,
    pub content_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct DatabaseUsage {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub required_free_bytes: u64,
    pub conversation_count: i64,
    pub messages: Vec<MessageUsage>,
}

#[derive(Debug, Serialize)]
pub struct CompactionResult {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum MaintenanceError {
    #[error("database_missing")]
    Missing,
    #[error("database_in_use")]
    InUse,
    #[error("insufficient_space")]
    InsufficientSpace,
    #[error("database_io")]
    Io(#[from] std::io::Error),
    #[error("database_query")]
    Query(#[from] sqlx::Error),
}

fn file_bytes(path: &Path) -> Result<u64, std::io::Error> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn disk_bytes(path: &Path) -> Result<(u64, u64), MaintenanceError> {
    Ok((file_bytes(path)?, file_bytes(&path.with_extension("db-wal"))?))
}

async fn connect(path: &Path, readonly: bool) -> Result<SqliteConnection, MaintenanceError> {
    if !path.is_file() {
        return Err(MaintenanceError::Missing);
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(readonly)
        .busy_timeout(Duration::from_secs(5));
    Ok(SqliteConnection::connect_with(&options).await?)
}

/// Returns physical usage and logical message sizes without loading message bodies.
pub async fn inspect(path: &Path) -> Result<DatabaseUsage, MaintenanceError> {
    let mut connection = connect(path, true).await?;
    sqlx::query("PRAGMA query_only = ON").execute(&mut connection).await?;
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut connection)
        .await?;
    let free_pages: i64 = sqlx::query_scalar("PRAGMA freelist_count")
        .fetch_one(&mut connection)
        .await?;
    let conversation_count = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(&mut connection)
        .await?;
    // Evaluate octet_length before aggregation. Sorting entire content values
    // or reading columns after content would load large overflow-page chains.
    let rows = sqlx::query("SELECT type, octet_length(content) AS bytes FROM messages NOT INDEXED")
        .fetch_all(&mut connection)
        .await?;
    let mut groups = BTreeMap::<String, (u64, u64)>::new();
    for row in rows {
        let entry = groups.entry(row.try_get("type")?).or_default();
        entry.0 += 1;
        entry.1 += row.try_get::<i64, _>("bytes")?.max(0) as u64;
    }
    connection.close().await?;
    let (database_bytes, wal_bytes) = disk_bytes(path)?;
    let mut messages: Vec<_> = groups
        .into_iter()
        .map(|(message_type, (count, content_bytes))| MessageUsage {
            message_type,
            count,
            content_bytes,
        })
        .collect();
    messages.sort_by_key(|entry| std::cmp::Reverse(entry.content_bytes));
    Ok(DatabaseUsage {
        database_bytes,
        wal_bytes,
        free_bytes: (free_pages * page_size).max(0) as u64,
        available_bytes: fs2::available_space(path)?.min(fs2::available_space(std::env::temp_dir())?),
        required_free_bytes: database_bytes.saturating_add(wal_bytes).saturating_mul(2),
        conversation_count,
        messages,
    })
}

/// Reclaims free pages only while the server's data-directory lock is unowned.
/// Existing rows are retained; no migrations or application deletes are run.
pub async fn compact(path: &Path) -> Result<CompactionResult, MaintenanceError> {
    if !path.is_file() {
        return Err(MaintenanceError::Missing);
    }
    // Fail closed if the platform cannot provide the lock.
    let _guard = DataDirInstanceGuard::try_acquire(path)?.ok_or(MaintenanceError::InUse)?;
    let (database_bytes, wal_bytes) = disk_bytes(path)?;
    let before_bytes = database_bytes.saturating_add(wal_bytes);
    let required = before_bytes.saturating_mul(2);
    if fs2::available_space(path)? < required || fs2::available_space(std::env::temp_dir())? < required {
        return Err(MaintenanceError::InsufficientSpace);
    }
    let mut connection = connect(path, false).await?;
    // Retain SQLite's exclusive connection lock through VACUUM and checkpoint,
    // including against clients that do not use the process-level instance lock.
    sqlx::query("PRAGMA locking_mode = EXCLUSIVE")
        .execute(&mut connection)
        .await?;
    sqlx::query("BEGIN EXCLUSIVE").execute(&mut connection).await?;
    sqlx::query("COMMIT").execute(&mut connection).await?;
    sqlx::query("VACUUM").execute(&mut connection).await?;
    let checkpoint = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(&mut connection)
        .await?;
    if checkpoint.try_get::<i64, _>(0)? != 0 {
        return Err(MaintenanceError::InUse);
    }
    connection.close().await?;
    let (database_bytes, wal_bytes) = disk_bytes(path)?;
    let after_bytes = database_bytes.saturating_add(wal_bytes);
    Ok(CompactionResult {
        before_bytes,
        after_bytes,
        reclaimed_bytes: before_bytes.saturating_sub(after_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture(path: &Path) -> SqliteConnection {
        let mut connection =
            SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path).create_if_missing(true))
                .await
                .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE conversations(id TEXT PRIMARY KEY); \
            CREATE TABLE messages(id INTEGER PRIMARY KEY, type TEXT, content TEXT); \
            INSERT INTO conversations VALUES ('kept'); \
            INSERT INTO messages VALUES (1, 'text', 'kept'); \
            INSERT INTO messages VALUES (2, 'tool_call', hex(zeroblob(1000000))); \
            DELETE FROM messages WHERE id = 2;",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        connection
    }

    #[tokio::test]
    async fn compact_preserves_rows_and_reclaims_deleted_pages() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aionui-backend.db");
        fixture(&path).await.close().await.unwrap();
        let before = inspect(&path).await.unwrap();
        assert!(before.free_bytes > 1000000);
        let result = compact(&path).await.unwrap();
        assert!(result.reclaimed_bytes > 1000000);
        let mut connection = connect(&path, true).await.unwrap();
        let content: String = sqlx::query_scalar("SELECT content FROM messages WHERE id = 1")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(content, "kept");
        assert_eq!(inspect(&path).await.unwrap().conversation_count, 1);
    }

    #[tokio::test]
    async fn refuses_compaction_while_server_owns_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aionui-backend.db");
        fixture(&path).await.close().await.unwrap();
        let _guard = DataDirInstanceGuard::try_acquire(&path).unwrap().unwrap();
        assert!(matches!(compact(&path).await, Err(MaintenanceError::InUse)));
        assert_eq!(inspect(&path).await.unwrap().messages[0].content_bytes, 4);
    }

    #[tokio::test]
    async fn inspection_does_not_create_missing_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.db");
        assert!(matches!(inspect(&path).await, Err(MaintenanceError::Missing)));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn inspection_includes_uncheckpointed_wal_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aionui-backend.db");
        let mut connection = fixture(&path).await;
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("PRAGMA wal_autocheckpoint=0")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages VALUES (3, 'tool_call', 'wal content')")
            .execute(&mut connection)
            .await
            .unwrap();
        let usage = inspect(&path).await.unwrap();
        assert!(usage.wal_bytes > 0);
        assert_eq!(usage.messages.iter().map(|entry| entry.count).sum::<u64>(), 2);
        connection.close().await.unwrap();
        compact(&path).await.unwrap();
        assert_eq!(
            inspect(&path)
                .await
                .unwrap()
                .messages
                .iter()
                .map(|entry| entry.count)
                .sum::<u64>(),
            2
        );
    }

    #[tokio::test]
    async fn sqlite_lock_blocks_compaction_even_without_a_server_instance_guard() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aionui-backend.db");
        let mut connection = fixture(&path).await;
        sqlx::query("BEGIN EXCLUSIVE").execute(&mut connection).await.unwrap();
        assert!(compact(&path).await.is_err());
        sqlx::query("ROLLBACK").execute(&mut connection).await.unwrap();
        assert_eq!(inspect(&path).await.unwrap().conversation_count, 1);
    }
}
