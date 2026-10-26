use sqlx::SqlitePool;

use crate::DbError;

/// Number of conversations whose `extra.claude_account_id` references `account_id`.
///
/// Claude accounts are stored outside the database (see the agent crate's
/// `claude_accounts` module); conversations only carry the account id.
pub async fn count_conversations_using_claude_account(pool: &SqlitePool, account_id: &str) -> Result<i64, DbError> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE json_extract(extra, '$.claude_account_id') = ?")
            .bind(account_id)
            .fetch_one(pool)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    #[tokio::test]
    async fn counts_only_conversations_that_reference_the_account() {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('u', 'u', 'x', 0, 0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        for (id, extra) in [
            ("c1", r#"{"claude_account_id":"max"}"#),
            ("c2", r#"{"claude_account_id":"other"}"#),
            ("c3", r#"{}"#),
        ] {
            sqlx::query(
                "INSERT INTO conversations (id, user_id, name, type, extra, created_at, updated_at) \
                 VALUES (?, 'u', 'n', 'acp', ?, 0, 0)",
            )
            .bind(id)
            .bind(extra)
            .execute(db.pool())
            .await
            .unwrap();
        }
        assert_eq!(
            count_conversations_using_claude_account(db.pool(), "max")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            count_conversations_using_claude_account(db.pool(), "none")
                .await
                .unwrap(),
            0
        );
    }
}
