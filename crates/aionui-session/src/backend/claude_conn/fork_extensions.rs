use std::time::Duration;

const DEFAULT_CLAUDE_FIRST_FRAME_TIMEOUT_SECS: u64 = 120;
const CLAUDE_FIRST_FRAME_TIMEOUT_ENV: &str = "AIONUI_CLAUDE_FIRST_FRAME_TIMEOUT_SECS";
#[cfg(test)]
const UPSTREAM_TEST_TIMEOUT_ENV: &str = "AIONUI_HANDSHAKE_TIMEOUT_SECS";

fn positive_seconds(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|seconds| *seconds > 0)
}

fn first_frame_budget_from(claude_value: Option<&str>) -> Duration {
    let seconds = claude_value
        .and_then(positive_seconds)
        .unwrap_or(DEFAULT_CLAUDE_FIRST_FRAME_TIMEOUT_SECS);
    Duration::from_secs(seconds)
}

pub(super) fn first_frame_budget() -> Duration {
    let claude_value = std::env::var(CLAUDE_FIRST_FRAME_TIMEOUT_ENV).ok();
    #[cfg(test)]
    let claude_value = claude_value.or_else(|| std::env::var(UPSTREAM_TEST_TIMEOUT_ENV).ok());
    first_frame_budget_from(claude_value.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_two_minutes_and_accepts_claude_override() {
        assert_eq!(first_frame_budget_from(None), Duration::from_secs(120));
        assert_eq!(first_frame_budget_from(Some("45")), Duration::from_secs(45));
        assert_eq!(
            first_frame_budget_from(Some("0")),
            Duration::from_secs(120),
            "an invalid Claude-specific value must retain the safe default"
        );
        assert_eq!(
            first_frame_budget_from(Some("invalid")),
            Duration::from_secs(120),
            "an invalid Claude-specific value must retain the safe default"
        );
    }
}
