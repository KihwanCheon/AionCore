use async_trait::async_trait;
use sqlx::{FromRow, SqlitePool};

use crate::DbError;

#[derive(Debug, Clone, FromRow)]
pub struct ArchiveRetentionRow {
    pub user_id: String,
    pub enabled: bool,
    pub retention_days: i64,
    pub run_at: String,
    pub timezone: String,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_deleted: i64,
    pub last_failed: i64,
    pub updated_at: i64,
}

#[async_trait]
pub trait IArchiveRetentionRepository: Send + Sync {
    async fn get(&self, user_id: &str) -> Result<Option<ArchiveRetentionRow>, DbError>;
    async fn save(&self, row: &ArchiveRetentionRow) -> Result<(), DbError>;
    async fn due(&self, now: i64) -> Result<Vec<ArchiveRetentionRow>, DbError>;
    /// Advance the schedule atomically before deletion. A stale snapshot cannot claim it.
    async fn claim(&self, row: &ArchiveRetentionRow, now: i64, next: i64) -> Result<bool, DbError>;
    async fn finish(&self, user_id: &str, started_at: i64, deleted: i64, failed: i64) -> Result<(), DbError>;
    async fn expired_conversations(&self, user_id: &str, cutoff: i64) -> Result<Vec<String>, DbError>;
    async fn is_expired(&self, user_id: &str, id: &str, cutoff: i64) -> Result<bool, DbError>;
}

// Team members are owned by the team lifecycle and must not be deleted individually.
const EXPIRED_CONVERSATION: &str = "user_id = ? AND archived_at IS NOT NULL AND archived_at < ? \
    AND COALESCE(NULLIF(json_extract(extra, '$.team_id'), ''), NULLIF(json_extract(extra, '$.teamId'), '')) IS NULL \
    AND COALESCE(json_extract(extra, '$.is_health_check'), 0) != 1";

pub struct SqliteArchiveRetentionRepository {
    pool: SqlitePool,
}

impl SqliteArchiveRetentionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl IArchiveRetentionRepository for SqliteArchiveRetentionRepository {
    async fn get(&self, user_id: &str) -> Result<Option<ArchiveRetentionRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM archive_retention WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn save(&self, row: &ArchiveRetentionRow) -> Result<(), DbError> {
        sqlx::query("INSERT INTO archive_retention (user_id, enabled, retention_days, run_at, timezone, next_run_at, updated_at) \
            VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(user_id) DO UPDATE SET \
            enabled = excluded.enabled, retention_days = excluded.retention_days, run_at = excluded.run_at, \
            timezone = excluded.timezone, next_run_at = excluded.next_run_at, updated_at = excluded.updated_at")
            .bind(&row.user_id).bind(row.enabled).bind(row.retention_days).bind(&row.run_at)
            .bind(&row.timezone).bind(row.next_run_at).bind(row.updated_at).execute(&self.pool).await?;
        Ok(())
    }

    async fn due(&self, now: i64) -> Result<Vec<ArchiveRetentionRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM archive_retention WHERE enabled = 1 AND next_run_at <= ? ORDER BY next_run_at, user_id",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn claim(&self, row: &ArchiveRetentionRow, now: i64, next: i64) -> Result<bool, DbError> {
        Ok(sqlx::query(
            "UPDATE archive_retention SET next_run_at = ?, last_run_at = ?, last_deleted = 0, last_failed = 0 \
            WHERE user_id = ? AND enabled = 1 AND next_run_at = ? AND updated_at = ?",
        )
        .bind(next)
        .bind(now)
        .bind(&row.user_id)
        .bind(row.next_run_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    async fn finish(&self, user_id: &str, started_at: i64, deleted: i64, failed: i64) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE archive_retention SET last_deleted = ?, last_failed = ? WHERE user_id = ? AND last_run_at = ?",
        )
        .bind(deleted)
        .bind(failed)
        .bind(user_id)
        .bind(started_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn expired_conversations(&self, user_id: &str, cutoff: i64) -> Result<Vec<String>, DbError> {
        let sql = format!("SELECT id FROM conversations WHERE {EXPIRED_CONVERSATION} ORDER BY archived_at, id");
        Ok(sqlx::query_scalar(&sql)
            .bind(user_id)
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn is_expired(&self, user_id: &str, id: &str, cutoff: i64) -> Result<bool, DbError> {
        let sql = format!("SELECT EXISTS(SELECT 1 FROM conversations WHERE {EXPIRED_CONVERSATION} AND id = ?)");
        Ok(sqlx::query_scalar(&sql)
            .bind(user_id)
            .bind(cutoff)
            .bind(id)
            .fetch_one(&self.pool)
            .await?)
    }
}
