use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionUsageState {
    Loading,
    Ready,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderUsageState {
    Loading,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionUsageWindow {
    pub used_percent: u16,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSubscriptionUsage {
    pub state: ProviderUsageState,
    pub updated_at: Option<String>,
    pub session: Option<SubscriptionUsageWindow>,
    pub weekly: Option<SubscriptionUsageWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexSubscriptionUsageWindow {
    pub used_percent: u16,
    pub resets_at: Option<String>,
    pub window_duration_mins: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexSubscriptionUsage {
    pub state: ProviderUsageState,
    pub updated_at: Option<String>,
    pub weekly: Option<CodexSubscriptionUsageWindow>,
    pub limit_reached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionUsageSnapshot {
    pub schema_version: u32,
    pub state: SubscriptionUsageState,
    pub generated_at: String,
    pub updated_at: Option<String>,
    pub retry_after_ms: Option<u64>,
    pub claude: ClaudeSubscriptionUsage,
    pub codex: CodexSubscriptionUsage,
}

#[cfg(test)]
mod tests {
    use super::{ProviderUsageState, SubscriptionUsageSnapshot, SubscriptionUsageState};

    #[test]
    fn subscription_usage_snapshot_matches_desktop_publisher_schema() {
        let snapshot: SubscriptionUsageSnapshot = serde_json::from_str(
            r#"{
                "schemaVersion": 1,
                "state": "ready",
                "generatedAt": "2026-09-08T00:00:00.000Z",
                "updatedAt": "2026-09-08T00:00:00.000Z",
                "retryAfterMs": null,
                "claude": {
                    "state": "ready",
                    "updatedAt": "2026-09-08T00:00:00.000Z",
                    "session": { "usedPercent": 17, "resetsAt": null },
                    "weekly": { "usedPercent": 41, "resetsAt": "2026-09-09T00:00:00.000Z" }
                },
                "codex": {
                    "state": "ready",
                    "updatedAt": "2026-09-08T00:00:00.000Z",
                    "weekly": {
                        "usedPercent": 73,
                        "resetsAt": "2026-09-10T00:00:00.000Z",
                        "windowDurationMins": 10080
                    },
                    "limitReached": false
                }
            }"#,
        )
        .unwrap();

        assert_eq!(snapshot.state, SubscriptionUsageState::Ready);
        assert_eq!(snapshot.claude.state, ProviderUsageState::Ready);
        assert_eq!(snapshot.codex.weekly.unwrap().used_percent, 73);
    }
}
