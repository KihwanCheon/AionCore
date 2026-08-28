//! Bounded, rate-limited previews for live command output.
//!
//! Completed tool results remain authoritative. This module only shapes the
//! ephemeral preview frames sent while a command is still running.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(crate) const LIVE_TOOL_OUTPUT_PREVIEW_BYTES: usize = 64 * 1024;
const LIVE_TOOL_OUTPUT_EMIT_INTERVAL: Duration = Duration::from_millis(200);
const TRUNCATION_MARKER: &str = "…[truncated]\n";

#[derive(Debug)]
struct Entry {
    tail: String,
    truncated: bool,
    last_emit_at: Option<Instant>,
    received_bytes: usize,
    emitted_frames: usize,
    coalesced_frames: usize,
}

impl Entry {
    fn new() -> Self {
        Self {
            tail: String::new(),
            truncated: false,
            last_emit_at: None,
            received_bytes: 0,
            emitted_frames: 0,
            coalesced_frames: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LiveToolOutputPush {
    pub(crate) output: Option<String>,
    pub(crate) truncation_started: bool,
    pub(crate) received_bytes: usize,
}

#[derive(Debug)]
pub(crate) struct LiveToolOutputStats {
    pub(crate) truncated: bool,
    pub(crate) received_bytes: usize,
    pub(crate) emitted_frames: usize,
    pub(crate) coalesced_frames: usize,
}

/// Keeps live command previews responsive without repeatedly broadcasting an
/// unbounded cumulative string for every output delta.
pub(crate) struct LiveToolOutputBuffer {
    entries: HashMap<String, Entry>,
    preview_bytes: usize,
    emit_interval: Duration,
}

impl LiveToolOutputBuffer {
    pub(crate) fn new() -> Self {
        Self::with_policy(LIVE_TOOL_OUTPUT_PREVIEW_BYTES, LIVE_TOOL_OUTPUT_EMIT_INTERVAL)
    }

    fn with_policy(preview_bytes: usize, emit_interval: Duration) -> Self {
        assert!(preview_bytes > TRUNCATION_MARKER.len());
        Self {
            entries: HashMap::new(),
            preview_bytes,
            emit_interval,
        }
    }

    pub(crate) fn push(&mut self, call_id: &str, delta: &str) -> LiveToolOutputPush {
        self.push_at(call_id, delta, Instant::now())
    }

    fn push_at(&mut self, call_id: &str, delta: &str, now: Instant) -> LiveToolOutputPush {
        let entry = self.entries.entry(call_id.to_owned()).or_insert_with(Entry::new);
        entry.received_bytes = entry.received_bytes.saturating_add(delta.len());
        entry.tail.push_str(delta);

        let was_truncated = entry.truncated;
        if entry.truncated || entry.tail.len() > self.preview_bytes {
            entry.truncated = true;
            let max_tail_bytes = self.preview_bytes - TRUNCATION_MARKER.len();
            trim_to_recent_bytes(&mut entry.tail, max_tail_bytes);
        }

        let should_emit = entry
            .last_emit_at
            .and_then(|last| now.checked_duration_since(last))
            .is_none_or(|elapsed| elapsed >= self.emit_interval);

        let output = if should_emit {
            entry.last_emit_at = Some(now);
            entry.emitted_frames = entry.emitted_frames.saturating_add(1);
            let mut preview =
                String::with_capacity(entry.tail.len() + usize::from(entry.truncated) * TRUNCATION_MARKER.len());
            if entry.truncated {
                preview.push_str(TRUNCATION_MARKER);
            }
            preview.push_str(&entry.tail);
            Some(preview)
        } else {
            entry.coalesced_frames = entry.coalesced_frames.saturating_add(1);
            None
        };

        LiveToolOutputPush {
            output,
            truncation_started: !was_truncated && entry.truncated,
            received_bytes: entry.received_bytes,
        }
    }

    pub(crate) fn remove(&mut self, call_id: &str) -> Option<LiveToolOutputStats> {
        self.entries.remove(call_id).map(|entry| LiveToolOutputStats {
            truncated: entry.truncated,
            received_bytes: entry.received_bytes,
            emitted_frames: entry.emitted_frames,
            coalesced_frames: entry.coalesced_frames,
        })
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.entries.retain(|call_id, _| keep(call_id));
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

fn trim_to_recent_bytes(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }

    let mut remove_bytes = value.len() - max_bytes;
    while remove_bytes < value.len() && !value.is_char_boundary(remove_bytes) {
        remove_bytes += 1;
    }
    value.drain(..remove_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_output_is_emitted_immediately() {
        let mut buffer = LiveToolOutputBuffer::with_policy(64, Duration::from_millis(200));
        let update = buffer.push_at("call-1", "line 1\n", Instant::now());

        assert_eq!(update.output.as_deref(), Some("line 1\n"));
        assert!(!update.truncation_started);
    }

    #[test]
    fn burst_updates_are_coalesced_into_the_next_snapshot() {
        let mut buffer = LiveToolOutputBuffer::with_policy(64, Duration::from_millis(200));
        let started_at = Instant::now();

        let first = buffer.push_at("call-1", "line 1\n", started_at);
        let coalesced = buffer.push_at("call-1", "line 2\n", started_at + Duration::from_millis(50));
        let next = buffer.push_at("call-1", "line 3\n", started_at + Duration::from_millis(200));

        assert_eq!(first.output.as_deref(), Some("line 1\n"));
        assert!(coalesced.output.is_none());
        assert_eq!(next.output.as_deref(), Some("line 1\nline 2\nline 3\n"));
    }

    #[test]
    fn oversized_output_keeps_a_bounded_utf8_tail() {
        let preview_bytes = 64;
        let mut buffer = LiveToolOutputBuffer::with_policy(preview_bytes, Duration::ZERO);
        let update = buffer.push_at("call-1", &"가".repeat(40), Instant::now());
        let output = update
            .output
            .expect("oversized first output should still emit a preview");

        assert!(update.truncation_started);
        assert!(output.starts_with(TRUNCATION_MARKER));
        assert!(output.len() <= preview_bytes);
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }

    #[test]
    fn large_burst_stays_bounded_and_reports_coalescing() {
        let preview_bytes = 1024;
        let mut buffer = LiveToolOutputBuffer::with_policy(preview_bytes, Duration::from_secs(1));
        let started_at = Instant::now();

        for _ in 0..10_000 {
            let _ = buffer.push_at("call-1", &"x".repeat(1024), started_at);
        }
        let final_update = buffer.push_at("call-1", "done", started_at + Duration::from_secs(1));
        let stats = buffer.remove("call-1").expect("tracked call should have statistics");

        assert!(
            final_update
                .output
                .as_ref()
                .is_some_and(|output| output.len() <= preview_bytes)
        );
        assert!(stats.truncated);
        assert_eq!(stats.emitted_frames, 2);
        assert_eq!(stats.coalesced_frames, 9_999);
    }

    #[test]
    fn retain_drops_completed_call_state() {
        let mut buffer = LiveToolOutputBuffer::with_policy(64, Duration::ZERO);
        let _ = buffer.push_at("open", "a", Instant::now());
        let _ = buffer.push_at("closed", "b", Instant::now());

        buffer.retain(|call_id| call_id == "open");

        assert!(buffer.remove("closed").is_none());
        assert!(buffer.remove("open").is_some());
    }
}
