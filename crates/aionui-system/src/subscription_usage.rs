use std::fs;
use std::path::{Path, PathBuf};

use aionui_api_types::SubscriptionUsageSnapshot;

const SUBSCRIPTION_USAGE_FILE: &str = "aionui-subscription-usage.json";

pub fn read_subscription_usage() -> Option<SubscriptionUsageSnapshot> {
    read_subscription_usage_from_path(&subscription_usage_path())
}

fn subscription_usage_path() -> PathBuf {
    std::env::temp_dir().join(SUBSCRIPTION_USAGE_FILE)
}

fn read_subscription_usage_from_path(path: &Path) -> Option<SubscriptionUsageSnapshot> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::read_subscription_usage_from_path;

    fn test_path(case: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aioncore-subscription-usage-{case}-{}.json",
            std::process::id()
        ))
    }

    #[test]
    fn reads_snapshot_published_by_desktop_process() {
        let path = test_path("ready");
        fs::write(
            &path,
            r#"{
                "schemaVersion":1,
                "state":"partial",
                "generatedAt":"2026-09-08T00:00:00.000Z",
                "updatedAt":"2026-09-08T00:00:00.000Z",
                "retryAfterMs":2000,
                "claude":{"state":"loading","updatedAt":null,"session":null,"weekly":null},
                "codex":{
                    "state":"ready",
                    "updatedAt":"2026-09-08T00:00:00.000Z",
                    "weekly":{"usedPercent":29,"resetsAt":null,"windowDurationMins":10080},
                    "limitReached":false
                }
            }"#,
        )
        .unwrap();

        let snapshot = read_subscription_usage_from_path(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(snapshot.retry_after_ms, Some(2000));
        assert_eq!(snapshot.codex.weekly.unwrap().used_percent, 29);
    }

    #[test]
    fn ignores_missing_or_malformed_snapshots() {
        let path = test_path("invalid");
        let _ = fs::remove_file(&path);
        assert!(read_subscription_usage_from_path(&path).is_none());

        fs::write(&path, "not-json").unwrap();
        let result = read_subscription_usage_from_path(&path);
        fs::remove_file(path).unwrap();

        assert!(result.is_none());
    }
}
