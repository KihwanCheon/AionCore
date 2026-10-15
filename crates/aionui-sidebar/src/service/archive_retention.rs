use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{ArchiveRetentionResponse, ArchiveRetentionSettings};
use aionui_common::now_ms;
use aionui_db::fork_extensions::archive_retention_repository::{ArchiveRetentionRow, IArchiveRetentionRepository};
use chrono::{Days, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use super::{SidebarError, SidebarService};

const DAY_MS: i64 = 86_400_000;

fn validate(settings: &ArchiveRetentionSettings) -> Result<(NaiveTime, Tz), SidebarError> {
    if !(1..=3650).contains(&settings.retention_days) {
        return Err(SidebarError::BadRequest(
            "retention_days must be between 1 and 3650".into(),
        ));
    }
    if settings.run_at.len() != 5 {
        return Err(SidebarError::BadRequest("run_at must use HH:mm".into()));
    }
    let time = NaiveTime::parse_from_str(&settings.run_at, "%H:%M")
        .map_err(|_| SidebarError::BadRequest("run_at must use HH:mm".into()))?;
    let zone = settings
        .timezone
        .parse::<Tz>()
        .map_err(|_| SidebarError::BadRequest("timezone must be a valid IANA timezone".into()))?;
    Ok((time, zone))
}

/// Choose a future occurrence. Ambiguous DST times run once; nonexistent times skip that day.
fn next_run(settings: &ArchiveRetentionSettings, after: i64) -> Result<i64, SidebarError> {
    let (time, zone) = validate(settings)?;
    let today = Utc
        .timestamp_millis_opt(after)
        .single()
        .ok_or_else(|| SidebarError::Internal("invalid retention clock".into()))?
        .with_timezone(&zone)
        .date_naive();
    for day in 0..=3 {
        if let Some(date) = today.checked_add_days(Days::new(day))
            && let Some(at) = zone.from_local_datetime(&date.and_time(time)).earliest()
            && at.timestamp_millis() > after
        {
            return Ok(at.timestamp_millis());
        }
    }
    Err(SidebarError::Internal("no future retention schedule".into()))
}

fn response(row: ArchiveRetentionRow) -> ArchiveRetentionResponse {
    ArchiveRetentionResponse {
        settings: ArchiveRetentionSettings {
            enabled: row.enabled,
            retention_days: row.retention_days,
            run_at: row.run_at,
            timezone: row.timezone,
        },
        next_run_at: row.next_run_at,
        last_run_at: row.last_run_at,
        last_deleted: row.last_deleted,
        last_failed: row.last_failed,
    }
}

impl SidebarService {
    pub fn set_archive_retention_repository(&self, repository: Arc<dyn IArchiveRetentionRepository>) {
        let _ = self.archive_retention.set(repository);
    }

    fn retention_repository(&self) -> Result<&Arc<dyn IArchiveRetentionRepository>, SidebarError> {
        self.archive_retention
            .get()
            .ok_or_else(|| SidebarError::Internal("archive retention repository not wired".into()))
    }

    pub async fn get_archive_retention(&self, user_id: &str) -> Result<ArchiveRetentionResponse, SidebarError> {
        if let Some(row) = self.retention_repository()?.get(user_id).await? {
            return Ok(response(row));
        }
        Ok(ArchiveRetentionResponse {
            settings: ArchiveRetentionSettings {
                enabled: false,
                retention_days: 30,
                run_at: "04:30".into(),
                timezone: iana_time_zone::get_timezone()
                    .ok()
                    .filter(|zone| zone.parse::<Tz>().is_ok())
                    .unwrap_or_else(|| "UTC".into()),
            },
            next_run_at: None,
            last_run_at: None,
            last_deleted: 0,
            last_failed: 0,
        })
    }

    pub async fn save_archive_retention(
        &self,
        user_id: &str,
        settings: ArchiveRetentionSettings,
    ) -> Result<ArchiveRetentionResponse, SidebarError> {
        validate(&settings)?;
        let _guard = self.archive_mutation.lock().await;
        let repository = self.retention_repository()?;
        let now = now_ms();
        let previous = repository.get(user_id).await?;
        let updated_at = previous
            .as_ref()
            .map_or(now, |row| now.max(row.updated_at.saturating_add(1)));
        let next_run_at = if settings.enabled {
            Some(next_run(&settings, now)?)
        } else {
            None
        };
        repository
            .save(&ArchiveRetentionRow {
                user_id: user_id.into(),
                enabled: settings.enabled,
                retention_days: settings.retention_days,
                run_at: settings.run_at,
                timezone: settings.timezone,
                next_run_at,
                last_run_at: None,
                last_deleted: 0,
                last_failed: 0,
                updated_at,
            })
            .await?;
        tracing::info!(
            user_id,
            enabled = settings.enabled,
            next_run_at,
            "Archive retention schedule saved"
        );
        self.get_archive_retention(user_id).await
    }

    /// Claim each due schedule once, including one missed occurrence after a restart.
    pub async fn process_archive_retention(&self, now: i64) -> Result<(), SidebarError> {
        let repository = self.retention_repository()?;
        for policy in repository.due(now).await? {
            if let Err(error) = self.run_archive_retention(&policy, now).await {
                tracing::error!(user_id = %policy.user_id, error = %error, "Archive retention run failed");
            }
        }
        Ok(())
    }

    async fn run_archive_retention(&self, policy: &ArchiveRetentionRow, now: i64) -> Result<(), SidebarError> {
        let repository = self.retention_repository()?;
        let settings = response(policy.clone()).settings;
        let next = next_run(&settings, now)?;
        let ports = self
            .remove_project_ports
            .get()
            .ok_or_else(|| SidebarError::Internal("archive deletion ports not wired".into()))?;
        if !repository.claim(policy, now, next).await? {
            return Ok(());
        }
        let cutoff = now - policy.retention_days * DAY_MS;
        let mut deleted = 0;
        let mut failed = 0;
        tracing::info!(user_id = %policy.user_id, cutoff, "Archive retention run started");
        let result: Result<(), SidebarError> = async {
            for id in repository.expired_conversations(&policy.user_id, cutoff).await? {
                // Serialize the final expiry check and delete with restore/rearchive.
                // A changed/disabled policy stops the remaining batch at its next item.
                let _guard = self.archive_mutation.lock().await;
                let current = repository.get(&policy.user_id).await?;
                if !current.is_some_and(|row| row.enabled && row.updated_at == policy.updated_at) {
                    break;
                }
                if !repository.is_expired(&policy.user_id, &id, cutoff).await? {
                    continue;
                }
                match ports.delete_conversation(&policy.user_id, &id).await {
                    Ok(()) => deleted += 1,
                    Err(error) => {
                        failed += 1;
                        tracing::warn!(user_id = %policy.user_id, conversation_id = %id, error = %error,
                            "Archive retention conversation deletion failed");
                    }
                }
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            failed += 1;
        }
        repository.finish(&policy.user_id, now, deleted, failed).await?;
        tracing::info!(user_id = %policy.user_id, deleted, failed, "Archive retention run finished");
        result
    }

    /// One backend-owned worker serves every device; no renderer timer can delete data.
    pub fn start_archive_retention(
        self: Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    _ = interval.tick() => {
                        if *shutdown.borrow() { break; }
                        if let Err(error) = self.process_archive_retention(now_ms()).await {
                            tracing::error!(error = %error, "Archive retention scheduler failed");
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
