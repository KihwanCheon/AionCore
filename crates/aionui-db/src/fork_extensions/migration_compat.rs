use sqlx::SqlitePool;
use tracing::info;

use crate::error::DbError;

const LEGACY_EXTERNAL_DISPATCH_MIGRATION_VERSION: i64 = 38;
const EXTERNAL_DISPATCH_MIGRATION_VERSION: i64 = 2_026_081_501;
const EXTERNAL_DISPATCH_MIGRATION_DESCRIPTION: &str = "external conversation dispatch recovery";

/// Move the unshipped fork migration out of upstream's sequential version
/// range before sqlx validates checksums. The migration file is pinned to LF,
/// so its recorded checksum remains valid under the fork-owned version.
pub(crate) async fn remap_external_dispatch_migration_version(pool: &SqlitePool) -> Result<(), DbError> {
    let migrations_table_exists: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'")
            .fetch_one(pool)
            .await?;
    if !migrations_table_exists {
        return Ok(());
    }

    let result = sqlx::query(
        "UPDATE _sqlx_migrations SET version = ? \
         WHERE version = ? AND description = ? AND success = 1 \
           AND NOT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = ?)",
    )
    .bind(EXTERNAL_DISPATCH_MIGRATION_VERSION)
    .bind(LEGACY_EXTERNAL_DISPATCH_MIGRATION_VERSION)
    .bind(EXTERNAL_DISPATCH_MIGRATION_DESCRIPTION)
    .bind(EXTERNAL_DISPATCH_MIGRATION_VERSION)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        info!(
            from_version = LEGACY_EXTERNAL_DISPATCH_MIGRATION_VERSION,
            to_version = EXTERNAL_DISPATCH_MIGRATION_VERSION,
            "Remapped fork migration version to avoid upstream collision"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::migrate::Migrator;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    static TEST_MIGRATOR: Migrator = sqlx::migrate!();

    #[test]
    fn external_dispatch_migration_uses_the_legacy_lf_checksum() {
        let migration = TEST_MIGRATOR
            .iter()
            .find(|migration| migration.version == EXTERNAL_DISPATCH_MIGRATION_VERSION)
            .expect("external dispatch migration must be embedded");
        let checksum = migration
            .checksum
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();

        assert_eq!(
            checksum,
            "4ECE2FBBFD89AB3DC37073BFE287E490CE27BB9FC4D2DBBC2440B523D07C1BE972AEEB4F2E176732BAF972808AD630DB"
        );
    }

    async fn migration_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (\
                version BIGINT PRIMARY KEY,\
                description TEXT NOT NULL,\
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\
                success BOOLEAN NOT NULL,\
                checksum BLOB NOT NULL,\
                execution_time BIGINT NOT NULL\
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn remaps_only_the_legacy_fork_migration() {
        let pool = migration_pool().await;
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) VALUES (?, ?, 1, X'01', 0)",
        )
        .bind(LEGACY_EXTERNAL_DISPATCH_MIGRATION_VERSION)
        .bind(EXTERNAL_DISPATCH_MIGRATION_DESCRIPTION)
        .execute(&pool)
        .await
        .unwrap();

        remap_external_dispatch_migration_version(&pool).await.unwrap();

        let version: i64 = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, EXTERNAL_DISPATCH_MIGRATION_VERSION);
    }

    #[tokio::test]
    async fn preserves_upstream_migration_with_the_same_legacy_version() {
        let pool = migration_pool().await;
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) VALUES (?, ?, 1, X'02', 0)",
        )
        .bind(LEGACY_EXTERNAL_DISPATCH_MIGRATION_VERSION)
        .bind("aionrs fork capability")
        .execute(&pool)
        .await
        .unwrap();

        remap_external_dispatch_migration_version(&pool).await.unwrap();

        let version: i64 = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, LEGACY_EXTERNAL_DISPATCH_MIGRATION_VERSION);
    }
}
