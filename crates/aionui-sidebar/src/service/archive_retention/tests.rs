use super::*;
use crate::ports::RemoveProjectPorts;
use aionui_db::fork_extensions::archive_retention_repository::SqliteArchiveRetentionRepository;
use aionui_db::{Database, SqlitePool, SqliteSidebarStore, SqliteUserOrderStore, init_database_memory};
use async_trait::async_trait;
use std::sync::Mutex;
use tokio::sync::Notify;

fn timestamp(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value).unwrap().timestamp_millis()
}

fn settings() -> ArchiveRetentionSettings {
    ArchiveRetentionSettings {
        enabled: true,
        retention_days: 30,
        run_at: "04:30".into(),
        timezone: "Asia/Seoul".into(),
    }
}

#[test]
fn archive_retention_schedule_is_future_and_uses_saved_timezone() {
    let config = settings();
    let due = timestamp("2026-10-15T04:30:00+09:00");
    assert_eq!(next_run(&config, due - 1).unwrap(), due);
    assert_eq!(next_run(&config, due).unwrap(), due + DAY_MS);
    assert_eq!(next_run(&config, due + 1).unwrap(), due + DAY_MS);
}

#[test]
fn archive_retention_dst_skips_missing_time_and_does_not_run_twice() {
    let mut config = settings();
    config.timezone = "America/New_York".into();
    config.run_at = "02:30".into();
    assert_eq!(
        next_run(&config, timestamp("2026-03-08T00:00:00-05:00")).unwrap(),
        timestamp("2026-03-09T02:30:00-04:00")
    );
    config.run_at = "01:30".into();
    let first = timestamp("2026-11-01T01:30:00-04:00");
    assert_eq!(next_run(&config, first - 1).unwrap(), first);
    assert_eq!(
        next_run(&config, first).unwrap(),
        timestamp("2026-11-02T01:30:00-05:00")
    );
}

#[test]
fn archive_retention_rejects_invalid_days_times_and_zones() {
    for days in [0, -1, 3651, i64::MAX] {
        assert!(
            validate(&ArchiveRetentionSettings {
                retention_days: days,
                ..settings()
            })
            .is_err()
        );
    }
    for time in ["4:30", "24:00", "04:60", "04:30:00", "garbage", ""] {
        assert!(
            validate(&ArchiveRetentionSettings {
                run_at: time.into(),
                ..settings()
            })
            .is_err()
        );
    }
    assert!(
        validate(&ArchiveRetentionSettings {
            timezone: "unknown".into(),
            ..settings()
        })
        .is_err()
    );
}

struct DeletionPorts {
    pool: SqlitePool,
    calls: Mutex<Vec<String>>,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl RemoveProjectPorts for DeletionPorts {
    async fn delete_conversation(&self, user: &str, id: &str) -> Result<(), String> {
        self.calls.lock().unwrap().push(id.into());
        if id == "blocked" {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if id == "failed" {
            return Err("injected deletion failure".into());
        }
        sqlx::query("DELETE FROM conversations WHERE user_id = ? AND id = ?")
            .bind(user)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    async fn remove_team(&self, _user: &str, _id: &str) -> Result<(), String> {
        panic!("must not delete teams")
    }
    async fn delete_project_record(&self, _user: &str, _id: &str) -> Result<(), String> {
        panic!("must not delete projects")
    }
}

struct Fixture {
    db: Database,
    _tmp: tempfile::TempDir,
    service: Arc<SidebarService>,
    repository: Arc<SqliteArchiveRetentionRepository>,
    ports: Arc<DeletionPorts>,
}

async fn fixture() -> Fixture {
    let db = init_database_memory().await.unwrap();
    for user in ["owner", "other"] {
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
            .bind(user)
            .bind(user)
            .execute(db.pool())
            .await
            .unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let service = Arc::new(SidebarService::new(
        Arc::new(SqliteSidebarStore::new(db.pool().clone())),
        Arc::new(SqliteUserOrderStore::new(db.pool().clone())),
        tmp.path().into(),
    ));
    let repository = Arc::new(SqliteArchiveRetentionRepository::new(db.pool().clone()));
    let ports = Arc::new(DeletionPorts {
        pool: db.pool().clone(),
        calls: Mutex::new(Vec::new()),
        entered: Notify::new(),
        release: Notify::new(),
    });
    service.set_archive_retention_repository(repository.clone());
    service.set_remove_project_ports(ports.clone());
    Fixture {
        db,
        _tmp: tmp,
        service,
        repository,
        ports,
    }
}

impl Fixture {
    async fn native_project(&self) -> String {
        let path = self._tmp.path().join("native-project");
        let uri = aionui_project::canonical::to_file_uri(&path).unwrap();
        let canonical = aionui_project::canonical::canonicalize(&uri).unwrap();
        sqlx::query("INSERT INTO projects (project_id, user_id, name, kind, created_at, updated_at) VALUES ('project', 'owner', 'Native project', 'standard', 0, 0)")
            .execute(self.db.pool()).await.unwrap();
        sqlx::query("INSERT INTO folders (folder_id, resource_uri, resource_canonical, created_at, updated_at) VALUES ('folder', ?, ?, 0, 0)")
            .bind(uri).bind(canonical.as_str()).execute(self.db.pool()).await.unwrap();
        sqlx::query("INSERT INTO project_explorer (pe_id, project_id, folder_id, role, order_index, created_at, updated_at) VALUES ('explorer', 'project', 'folder', 'workspace', 0, 0, 0)")
            .execute(self.db.pool()).await.unwrap();
        serde_json::json!({ "workspace": path }).to_string()
    }
    async fn conversation(&self, user: &str, id: &str, archived: Option<i64>, extra: &str) {
        sqlx::query("INSERT INTO conversations (id, user_id, name, type, extra, archived_at, created_at, updated_at) VALUES (?, ?, ?, 'acp', ?, ?, 0, 0)")
            .bind(id).bind(user).bind(id).bind(extra).bind(archived).execute(self.db.pool()).await.unwrap();
    }
    async fn due_policy(&self, due: i64) -> ArchiveRetentionRow {
        let policy = ArchiveRetentionRow {
            user_id: "owner".into(),
            enabled: true,
            retention_days: 30,
            run_at: "04:30".into(),
            timezone: "Asia/Seoul".into(),
            next_run_at: Some(due),
            last_run_at: None,
            last_deleted: 0,
            last_failed: 0,
            updated_at: due - DAY_MS,
        };
        self.repository.save(&policy).await.unwrap();
        policy
    }
    async fn exists(&self, id: &str) -> bool {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ?)")
            .bind(id)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn archive_retention_defaults_off_and_save_only_schedules_future_run() {
    let f = fixture().await;
    let initial = f.service.get_archive_retention("owner").await.unwrap();
    assert!(!initial.settings.enabled);
    assert!(initial.next_run_at.is_none());
    f.conversation("owner", "old", Some(1), "{}").await;
    let now = now_ms();
    let saved = f.service.save_archive_retention("owner", settings()).await.unwrap();
    assert!(saved.next_run_at.unwrap() > now);
    assert!(f.exists("old").await);
    assert!(f.ports.calls.lock().unwrap().is_empty());
    let other = f.service.get_archive_retention("other").await.unwrap();
    assert!(!other.settings.enabled);
    let disabled = f
        .service
        .save_archive_retention(
            "owner",
            ArchiveRetentionSettings {
                enabled: false,
                ..settings()
            },
        )
        .await
        .unwrap();
    assert!(disabled.next_run_at.is_none());
    assert!(f.repository.due(i64::MAX).await.unwrap().is_empty());
}

#[tokio::test]
async fn archive_retention_deletes_only_expired_owned_independent_conversations() {
    let f = fixture().await;
    let due = timestamp("2026-10-15T04:30:00+09:00");
    let cutoff = due - 30 * DAY_MS;
    f.due_policy(due).await;
    f.conversation("owner", "expired", Some(cutoff - 1), "{}").await;
    f.conversation("owner", "boundary", Some(cutoff), "{}").await;
    f.conversation("owner", "recent", Some(cutoff + 1), "{}").await;
    f.conversation("owner", "active", None, "{}").await;
    f.conversation("other", "foreign", Some(1), "{}").await;
    f.conversation("owner", "team", Some(1), r#"{"teamId":"t"}"#).await;
    f.conversation("owner", "team_snake", Some(1), r#"{"team_id":"t"}"#)
        .await;
    f.conversation("owner", "health", Some(1), r#"{"is_health_check":true}"#)
        .await;
    f.service.process_archive_retention(due - 1).await.unwrap();
    assert!(f.exists("expired").await);
    f.service.process_archive_retention(due).await.unwrap();
    assert!(!f.exists("expired").await);
    for id in [
        "boundary",
        "recent",
        "active",
        "foreign",
        "team",
        "team_snake",
        "health",
    ] {
        assert!(f.exists(id).await, "must retain {id}");
    }
    let saved = f.repository.get("owner").await.unwrap().unwrap();
    assert_eq!((saved.last_deleted, saved.last_failed), (1, 0));
    assert_eq!(saved.next_run_at, Some(due + DAY_MS));
}

#[tokio::test]
async fn archive_retention_catches_up_once_and_continues_after_item_failure() {
    let f = fixture().await;
    let due = timestamp("2026-10-15T04:30:00+09:00");
    f.due_policy(due - 3 * DAY_MS).await;
    f.conversation("owner", "failed", Some(1), "{}").await;
    f.conversation("owner", "success", Some(1), "{}").await;
    let (one, two) = tokio::join!(
        f.service.process_archive_retention(due),
        f.service.process_archive_retention(due)
    );
    one.unwrap();
    two.unwrap();
    assert_eq!(*f.ports.calls.lock().unwrap(), vec!["failed", "success"]);
    assert!(f.exists("failed").await);
    assert!(!f.exists("success").await);
    let saved = f.repository.get("owner").await.unwrap().unwrap();
    assert_eq!((saved.last_deleted, saved.last_failed), (1, 1));
    assert_eq!(saved.next_run_at, Some(due + DAY_MS));
    // A new scheduler using persisted state must not repeat the same occurrence.
    let restarted = SidebarService::new(
        Arc::new(SqliteSidebarStore::new(f.db.pool().clone())),
        Arc::new(SqliteUserOrderStore::new(f.db.pool().clone())),
        f._tmp.path().into(),
    );
    restarted.set_archive_retention_repository(f.repository.clone());
    restarted.set_remove_project_ports(f.ports.clone());
    restarted.process_archive_retention(due + 1).await.unwrap();
    assert_eq!(f.ports.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn archive_retention_stale_snapshot_cannot_claim_changed_policy() {
    let f = fixture().await;
    let due = timestamp("2026-10-15T04:30:00+09:00");
    let stale = f.due_policy(due).await;
    f.service
        .save_archive_retention(
            "owner",
            ArchiveRetentionSettings {
                enabled: false,
                ..settings()
            },
        )
        .await
        .unwrap();
    assert!(!f.repository.claim(&stale, due, due + DAY_MS).await.unwrap());
}

#[tokio::test]
async fn archive_retention_rechecks_restore_or_rearchive_before_each_delete() {
    for action in ["restore", "rearchive", "restore_project"] {
        let f = fixture().await;
        let due = timestamp("2026-10-15T04:30:00+09:00");
        f.due_policy(due).await;
        f.conversation("owner", "blocked", Some(1), "{}").await;
        let extra = f.native_project().await;
        f.conversation("owner", "rescued", Some(2), &extra).await;
        let service = f.service.clone();
        let runner = tokio::spawn(async move { service.process_archive_retention(due).await.unwrap() });
        f.ports.entered.notified().await;
        // Poll once to queue the mutation behind the first delete, ahead of the next item.
        let change = async {
            match action {
                "rearchive" => f.service.archive_conversation("owner", "rescued").await,
                "restore_project" => f.service.unarchive_project("owner", "project").await,
                _ => f.service.unarchive_conversation("owner", "rescued").await,
            }
        };
        tokio::pin!(change);
        tokio::select! { biased; _ = &mut change => panic!("must wait for delete lock"), _ = tokio::task::yield_now() => {} }
        f.ports.release.notify_one();
        change.await.unwrap();
        runner.await.unwrap();
        assert!(f.exists("rescued").await);
        assert_eq!(*f.ports.calls.lock().unwrap(), vec!["blocked"]);
    }
}

#[tokio::test]
async fn archive_retention_project_archive_and_restore_use_native_paths_and_owner_scope() {
    let f = fixture().await;
    let extra = f.native_project().await;
    f.conversation("owner", "scoped", None, &extra).await;
    f.conversation("owner", "unrelated", None, "{}").await;
    f.conversation("other", "foreign", None, &extra).await;
    f.service.archive_project("owner", "project").await.unwrap();
    assert_eq!(
        f.repository.expired_conversations("owner", now_ms() + 1).await.unwrap(),
        vec!["scoped"]
    );
    assert!(
        f.repository
            .expired_conversations("other", now_ms() + 1)
            .await
            .unwrap()
            .is_empty()
    );
    f.service.unarchive_project("owner", "project").await.unwrap();
    assert!(
        f.repository
            .expired_conversations("owner", now_ms() + 1)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn archive_retention_disabling_between_items_stops_remaining_batch() {
    let f = fixture().await;
    let due = timestamp("2026-10-15T04:30:00+09:00");
    f.due_policy(due).await;
    f.conversation("owner", "blocked", Some(1), "{}").await;
    f.conversation("owner", "remaining", Some(2), "{}").await;
    let service = f.service.clone();
    let runner = tokio::spawn(async move { service.process_archive_retention(due).await.unwrap() });
    f.ports.entered.notified().await;
    let disable = f.service.save_archive_retention(
        "owner",
        ArchiveRetentionSettings {
            enabled: false,
            ..settings()
        },
    );
    tokio::pin!(disable);
    tokio::select! { biased; _ = &mut disable => panic!("must wait for delete lock"), _ = tokio::task::yield_now() => {} }
    f.ports.release.notify_one();
    disable.await.unwrap();
    runner.await.unwrap();
    assert!(f.exists("remaining").await);
    assert_eq!(*f.ports.calls.lock().unwrap(), vec!["blocked"]);
}
