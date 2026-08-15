use sqlx::SqlitePool;

use crate::error::DbError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExternalDispatchRecord {
    pub operation_id: String,
    pub request_fingerprint: String,
    pub actor_conversation_id: String,
    pub target_conversation_id: Option<String>,
    pub state: String,
    pub response_json: String,
    pub workspace_lease_json: Option<String>,
    pub boot_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub terminal_at: Option<i64>,
}

#[async_trait::async_trait]
pub trait IExternalDispatchRepository: Send + Sync {
    async fn get(&self, operation_id: &str) -> Result<Option<ExternalDispatchRecord>, DbError>;
    async fn insert(&self, record: &ExternalDispatchRecord) -> Result<bool, DbError>;
    async fn update(&self, record: &ExternalDispatchRecord) -> Result<(), DbError>;
    async fn delete(&self, operation_id: &str) -> Result<(), DbError>;
    async fn delete_terminal_before(&self, cutoff: i64) -> Result<u64, DbError>;
}

#[derive(Clone, Debug)]
pub struct SqliteExternalDispatchRepository {
    pool: SqlitePool,
}

impl SqliteExternalDispatchRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IExternalDispatchRepository for SqliteExternalDispatchRepository {
    async fn get(&self, operation_id: &str) -> Result<Option<ExternalDispatchRecord>, DbError> {
        Ok(sqlx::query_as::<_, ExternalDispatchRecord>(
            "SELECT operation_id, request_fingerprint, actor_conversation_id, target_conversation_id, state, \
                    response_json, workspace_lease_json, boot_id, created_at, updated_at, terminal_at \
             FROM external_conversation_dispatches WHERE operation_id = ?",
        )
        .bind(operation_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn insert(&self, record: &ExternalDispatchRecord) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO external_conversation_dispatches (operation_id, request_fingerprint, actor_conversation_id, \
                    target_conversation_id, state, response_json, workspace_lease_json, boot_id, created_at, updated_at, terminal_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(&record.operation_id)
        .bind(&record.request_fingerprint)
        .bind(&record.actor_conversation_id)
        .bind(&record.target_conversation_id)
        .bind(&record.state)
        .bind(&record.response_json)
        .bind(&record.workspace_lease_json)
        .bind(&record.boot_id)
        .bind(record.created_at)
        .bind(record.updated_at)
        .bind(record.terminal_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn update(&self, record: &ExternalDispatchRecord) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE external_conversation_dispatches SET target_conversation_id = ?, state = ?, response_json = ?, \
                    workspace_lease_json = ?, boot_id = ?, updated_at = ?, terminal_at = ? WHERE operation_id = ?",
        )
        .bind(&record.target_conversation_id)
        .bind(&record.state)
        .bind(&record.response_json)
        .bind(&record.workspace_lease_json)
        .bind(&record.boot_id)
        .bind(record.updated_at)
        .bind(record.terminal_at)
        .bind(&record.operation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete(&self, operation_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM external_conversation_dispatches WHERE operation_id = ?")
            .bind(operation_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_terminal_before(&self, cutoff: i64) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM external_conversation_dispatches WHERE terminal_at IS NOT NULL AND terminal_at < ?",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    fn record(operation_id: &str) -> ExternalDispatchRecord {
        ExternalDispatchRecord {
            operation_id: operation_id.to_owned(),
            request_fingerprint: "fingerprint".to_owned(),
            actor_conversation_id: "actor".to_owned(),
            target_conversation_id: Some("target".to_owned()),
            state: "running".to_owned(),
            response_json: "{\"state\":\"running\"}".to_owned(),
            workspace_lease_json: Some("{\"workspaceId\":\"fork1\"}".to_owned()),
            boot_id: "boot-1".to_owned(),
            created_at: 10,
            updated_at: 10,
            terminal_at: None,
        }
    }

    #[tokio::test]
    async fn insert_get_update_and_cleanup() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteExternalDispatchRepository::new(db.pool().clone());
        let mut value = record("operation-1");

        assert!(repo.insert(&value).await.unwrap());
        assert!(!repo.insert(&value).await.unwrap());
        assert_eq!(repo.get("operation-1").await.unwrap().unwrap().state, "running");

        value.state = "completed".to_owned();
        value.response_json = "{\"state\":\"completed\"}".to_owned();
        value.updated_at = 20;
        value.terminal_at = Some(20);
        repo.update(&value).await.unwrap();
        assert_eq!(repo.delete_terminal_before(21).await.unwrap(), 1);
        assert!(repo.get("operation-1").await.unwrap().is_none());
    }
}
