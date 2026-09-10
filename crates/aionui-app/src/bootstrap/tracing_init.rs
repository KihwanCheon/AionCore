//! Tracing subscriber + log file initialization for the binary.
//!
//! Lives in the binary tree (not lib) because it owns process-global
//! subscriber registration that should never be invoked from tests or
//! external consumers of the library.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use chrono::{Datelike, Duration, NaiveDate};
use tracing_appender::non_blocking::NonBlockingBuilder;
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use super::{BootstrapError, BootstrapErrorCode};

const NOISE_SUPPRESSIONS: &[&str] = &[
    "sqlx::query=warn",
    "hyper_util=warn",
    "reqwest=warn",
    // The ACP SDK logs raw UntypedMessage values at debug/trace, including
    // session/update chunks with user/agent text. Keep its protocol internals
    // out of default dev logs; aionui_ai_agent::protocol::acp emits sanitized
    // summaries for the ACP flow we need to debug.
    "agent_client_protocol::jsonrpc=info",
    // Aionrs provider/agent debug logs include raw request bodies and SSE
    // chunks. Keep lifecycle info logs, but do not write prompt/output
    // payloads by default.
    "aion_agent=info",
    "aion_providers=info",
];

const AIONRS_TARGETS: &[&str] = &[
    "aion_agent",
    "aion_config",
    "aion_compact",
    "aion_mcp",
    "aion_providers",
    "aion_protocol",
    "aion_tools",
    "aion_skills",
    "aion_memory",
];

const RAW_AIONRS_PAYLOAD_TARGETS: &[&str] = &["aion_agent", "aion_providers"];
const LOG_FILE_SIZE_LIMIT_BYTES: u64 = 10 * 1024 * 1024;
const LOG_FILES_PER_DAY_LIMIT: u32 = 5;
const LOG_RETENTION_DAYS: i64 = 14;
const LOG_BUFFERED_LINES_LIMIT: usize = 8_192;

fn build_env_filter(log_level: Option<&str>) -> EnvFilter {
    let user_directives = log_level.unwrap_or("info");
    let suppressions = NOISE_SUPPRESSIONS.join(",");
    EnvFilter::new(format!("{suppressions},{user_directives}"))
}

fn build_backend_filter(log_level: Option<&str>) -> EnvFilter {
    let user_directives = log_level.unwrap_or("info");
    let suppressions = NOISE_SUPPRESSIONS.join(",");
    let aionrs_off: String = AIONRS_TARGETS
        .iter()
        .map(|t| format!("{t}=off"))
        .collect::<Vec<_>>()
        .join(",");
    EnvFilter::new(format!("{suppressions},{aionrs_off},{user_directives}"))
}

fn build_aionrs_level(log_level: Option<&str>) -> String {
    let level = log_level.unwrap_or("info");
    AIONRS_TARGETS
        .iter()
        .map(|target| {
            let target_level = if RAW_AIONRS_PAYLOAD_TARGETS.contains(target) {
                "info"
            } else {
                level
            };
            format!("{target}={target_level}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// RAII guards that flush log buffers on drop. Hold for the process lifetime.
pub struct LogGuards {
    _backend: tracing_appender::non_blocking::WorkerGuard,
    _aionrs: tracing_appender::non_blocking::WorkerGuard,
}

const LOGGING_INIT_MESSAGE: &str = "failed to initialize logging";

/// Outcome of picking the log root: which root is active plus, when the
/// requested custom directory was unusable, the details needed to report the
/// degradation once the tracing subscriber is ready.
#[derive(Debug)]
struct LogDirSelection {
    root: PathBuf,
    active_dir: PathBuf,
    fallback: Option<LogDirFallback>,
}

/// Custom log-dir failure that triggered the default-dir fallback. Held until
/// the subscriber is initialized, then emitted as a structured warn.
#[derive(Debug)]
struct LogDirFallback {
    requested_dir: PathBuf,
    error_kind: io::ErrorKind,
}

fn logging_dir_error(active_log_dir: &Path, error: io::Error) -> BootstrapError {
    // ErrorKind is the enum name only (no path, no OS message), so it can ride
    // the stable stderr boundary line while the raw source stays private.
    let error_kind = format!("{:?}", error.kind());
    BootstrapError::new(
        BootstrapErrorCode::LoggingInitFailed,
        "logging.dir",
        LOGGING_INIT_MESSAGE,
    )
    .with_source(error)
    .with_field("logDir", active_log_dir.display().to_string())
    .with_field("errorKind", error_kind)
}

/// Pick the log root directory, creating today's dated partition.
///
/// A custom directory that cannot be created (permissions, AV interference,
/// path occupied by a file — Jira AIONUI-231) must not permanently brick
/// bootstrap: fall back to the default directory instead. Failure to create
/// the default directory itself remains fatal.
fn select_log_root(custom_log_dir: Option<&Path>, default_log_dir: &Path) -> Result<LogDirSelection, BootstrapError> {
    if let Some(custom) = custom_log_dir
        && custom != default_log_dir
    {
        let custom_active = dated_log_dir(custom);
        match std::fs::create_dir_all(&custom_active) {
            Ok(()) => {
                return Ok(LogDirSelection {
                    root: custom.to_path_buf(),
                    active_dir: custom_active,
                    fallback: None,
                });
            }
            Err(custom_error) => {
                let custom_kind = custom_error.kind();
                let default_active = dated_log_dir(default_log_dir);
                std::fs::create_dir_all(&default_active).map_err(|default_error| {
                    logging_dir_error(&default_active, default_error)
                        .with_field("requestedLogDir", custom.display().to_string())
                        .with_field("requestedErrorKind", format!("{custom_kind:?}"))
                })?;
                return Ok(LogDirSelection {
                    root: default_log_dir.to_path_buf(),
                    active_dir: default_active,
                    fallback: Some(LogDirFallback {
                        requested_dir: custom.to_path_buf(),
                        error_kind: custom_kind,
                    }),
                });
            }
        }
    }

    let root = custom_log_dir.unwrap_or(default_log_dir);
    let active_dir = dated_log_dir(root);
    std::fs::create_dir_all(&active_dir).map_err(|e| logging_dir_error(&active_dir, e))?;
    Ok(LogDirSelection {
        root: root.to_path_buf(),
        active_dir,
        fallback: None,
    })
}

pub fn init_tracing(
    custom_log_dir: Option<&Path>,
    default_log_dir: &Path,
    log_level: Option<&str>,
) -> Result<LogGuards, BootstrapError> {
    let selection = select_log_root(custom_log_dir, default_log_dir)?;
    let log_dir = selection.root.as_path();
    let active_log_dir = selection.active_dir.as_path();

    let console_layer = fmt::layer().with_target(true).with_filter(build_env_filter(log_level));

    // Backend file layer — excludes aion_* targets
    let file_appender = DailyDatedLogWriter::new(log_dir.to_path_buf(), "aioncore.log");
    let (non_blocking, backend_guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(LOG_BUFFERED_LINES_LIMIT)
        .lossy(true)
        .thread_name("aioncore-log-writer")
        .finish(file_appender);

    let backend_file_layer = fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_filter(build_backend_filter(log_level));

    // Aionrs file layer — only aion_* targets
    let aionrs_level = build_aionrs_level(log_level);
    let aionrs_filter = EnvFilter::try_new(&aionrs_level).map_err(|e| {
        BootstrapError::new(
            BootstrapErrorCode::LoggingInitFailed,
            "logging.filter",
            LOGGING_INIT_MESSAGE,
        )
        .with_source(e)
        .with_field("filter", aionrs_level.clone())
        .with_field("logDir", active_log_dir.display().to_string())
    })?;
    let aionrs_appender = DailyDatedLogWriter::new(log_dir.to_path_buf(), "aionrs.log");
    let (aionrs_non_blocking, aionrs_guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(LOG_BUFFERED_LINES_LIMIT)
        .lossy(true)
        .thread_name("aionrs-log-writer")
        .finish(aionrs_appender);
    let aionrs_layer = fmt::layer()
        .json()
        .with_writer(aionrs_non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_filter(aionrs_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(backend_file_layer)
        .with(aionrs_layer)
        .try_init()
        .map_err(|e| {
            BootstrapError::new(
                BootstrapErrorCode::LoggingInitFailed,
                "logging.subscriber",
                LOGGING_INIT_MESSAGE,
            )
            .with_source(e)
            .with_field("logDir", active_log_dir.display().to_string())
        })?;

    if let Some(fallback) = &selection.fallback {
        // Production-visible degradation marker (AIONUI-231): the requested
        // custom log dir was unusable and logging continues in the default dir.
        tracing::warn!(
            code = "BOOTSTRAP_DEGRADED_LOG_DIR",
            stage = "logging.dir.fallback",
            requested_log_dir = %fallback.requested_dir.display(),
            active_log_dir = %active_log_dir.display(),
            error_kind = ?fallback.error_kind,
            "custom log directory is unusable; falling back to default log directory"
        );
    }

    Ok(LogGuards {
        _backend: backend_guard,
        _aionrs: aionrs_guard,
    })
}

fn dated_log_dir(log_root: &Path) -> PathBuf {
    dated_log_dir_for(log_root, LogDate::today())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogDate {
    year: i32,
    month: u32,
    day: u32,
}

impl LogDate {
    fn today() -> Self {
        let now = chrono::Local::now();
        Self {
            year: now.year(),
            month: now.month(),
            day: now.day(),
        }
    }

    fn file_name(self, suffix: &str) -> String {
        format!("{:04}-{:02}-{:02}.{}", self.year, self.month, self.day, suffix)
    }

    fn rolled_file_name(self, part: u32, suffix: &str) -> String {
        if part == 0 {
            self.file_name(suffix)
        } else {
            format!("{:04}-{:02}-{:02}.{part}.{}", self.year, self.month, self.day, suffix)
        }
    }

    fn naive_date(self) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(self.year, self.month, self.day)
    }
}

fn dated_log_dir_for(log_root: &Path, date: LogDate) -> PathBuf {
    log_root
        .join(format!("{:04}", date.year))
        .join(format!("{:02}", date.month))
        .join(format!("{:02}", date.day))
}

fn rolled_log_file_path(log_root: &Path, date: LogDate, part: u32, suffix: &str) -> PathBuf {
    dated_log_dir_for(log_root, date).join(date.rolled_file_name(part, suffix))
}

fn parse_log_partition(year: &str, month: &str, day: &str) -> Option<NaiveDate> {
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, day.parse().ok()?)
}

fn remove_dir_if_empty(path: &Path) {
    if fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none()) {
        let _ = fs::remove_dir(path);
    }
}

fn cleanup_expired_log_partitions(log_root: &Path, today: LogDate, retention_days: i64) -> io::Result<()> {
    let Some(today) = today.naive_date() else {
        return Ok(());
    };
    let cutoff = today - Duration::days(retention_days.max(1) - 1);

    for year_entry in fs::read_dir(log_root)? {
        let year_entry = year_entry?;
        if !year_entry.file_type()?.is_dir() {
            continue;
        }
        let year_name = year_entry.file_name();
        let year = year_name.to_string_lossy();
        let year_path = year_entry.path();

        for month_entry in match fs::read_dir(&year_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        } {
            let month_entry = match month_entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !month_entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let month_name = month_entry.file_name();
            let month = month_name.to_string_lossy();
            let month_path = month_entry.path();

            for day_entry in match fs::read_dir(&month_path) {
                Ok(entries) => entries,
                Err(_) => continue,
            } {
                let day_entry = match day_entry {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };
                if !day_entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let day_name = day_entry.file_name();
                let day = day_name.to_string_lossy();
                if parse_log_partition(&year, &month, &day).is_some_and(|date| date < cutoff) {
                    let _ = fs::remove_dir_all(day_entry.path());
                }
            }

            remove_dir_if_empty(&month_path);
        }

        remove_dir_if_empty(&year_path);
    }

    Ok(())
}

struct DailyDatedLogWriter {
    log_root: PathBuf,
    filename_suffix: &'static str,
    date_provider: Box<dyn Fn() -> LogDate + Send + Sync>,
    max_file_bytes: u64,
    max_files_per_day: u32,
    retention_days: i64,
    active_date: Option<LogDate>,
    active_part: u32,
    active_bytes: u64,
    active_file: Option<File>,
}

impl DailyDatedLogWriter {
    fn new(log_root: PathBuf, filename_suffix: &'static str) -> Self {
        Self::new_with_date_provider(log_root, filename_suffix, Box::new(LogDate::today))
    }

    fn new_with_date_provider(
        log_root: PathBuf,
        filename_suffix: &'static str,
        date_provider: Box<dyn Fn() -> LogDate + Send + Sync>,
    ) -> Self {
        Self::new_with_policy(
            log_root,
            filename_suffix,
            date_provider,
            LOG_FILE_SIZE_LIMIT_BYTES,
            LOG_FILES_PER_DAY_LIMIT,
            LOG_RETENTION_DAYS,
        )
    }

    fn new_with_policy(
        log_root: PathBuf,
        filename_suffix: &'static str,
        date_provider: Box<dyn Fn() -> LogDate + Send + Sync>,
        max_file_bytes: u64,
        max_files_per_day: u32,
        retention_days: i64,
    ) -> Self {
        Self {
            log_root,
            filename_suffix,
            date_provider,
            max_file_bytes: max_file_bytes.max(1),
            max_files_per_day: max_files_per_day.max(1),
            retention_days: retention_days.max(1),
            active_date: None,
            active_part: 0,
            active_bytes: 0,
            active_file: None,
        }
    }

    fn open_part(&mut self, date: LogDate, part: u32) -> io::Result<()> {
        let file_path = rolled_log_file_path(&self.log_root, date, part, self.filename_suffix);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(file_path)?;
        self.active_bytes = file.metadata()?.len();
        self.active_file = Some(file);
        self.active_date = Some(date);
        self.active_part = part;
        Ok(())
    }

    fn ensure_active_file(&mut self) -> io::Result<()> {
        let date = (self.date_provider)();
        if self.active_date != Some(date) {
            self.active_file = None;
            let _ = cleanup_expired_log_partitions(&self.log_root, date, self.retention_days);

            let mut active_part = 0;
            for part in 0..self.max_files_per_day {
                if rolled_log_file_path(&self.log_root, date, part, self.filename_suffix).exists() {
                    active_part = part;
                } else {
                    break;
                }
            }
            self.open_part(date, active_part)?;
        }

        Ok(())
    }

    fn shift_daily_parts(&mut self, date: LogDate) {
        self.active_file = None;
        let base = rolled_log_file_path(&self.log_root, date, 0, self.filename_suffix);
        let _ = fs::remove_file(base);
        for source_part in 1..self.max_files_per_day {
            let source = rolled_log_file_path(&self.log_root, date, source_part, self.filename_suffix);
            let target = rolled_log_file_path(&self.log_root, date, source_part - 1, self.filename_suffix);
            if source.exists() {
                let _ = fs::rename(source, target);
            }
        }
    }

    fn rotate_if_needed(&mut self, incoming_bytes: usize) -> io::Result<()> {
        self.ensure_active_file()?;
        if self.active_bytes == 0 || self.active_bytes.saturating_add(incoming_bytes as u64) <= self.max_file_bytes {
            return Ok(());
        }

        let date = self
            .active_date
            .ok_or_else(|| io::Error::other("log date was not selected"))?;
        let next_part = if self.active_part + 1 < self.max_files_per_day {
            self.active_part + 1
        } else {
            self.shift_daily_parts(date);
            self.max_files_per_day - 1
        };
        self.open_part(date, next_part)
    }
}

impl Write for DailyDatedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed(buf.len())?;
        self.active_file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file was not opened"))?
            .write_all(buf)?;
        self.active_bytes = self.active_bytes.saturating_add(buf.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.active_file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn env_filter_suppresses_raw_acp_sdk_jsonrpc_debug_even_when_debug_enabled() {
        let subscriber = tracing_subscriber::registry().with(build_env_filter(Some("debug")));
        tracing::subscriber::with_default(subscriber, || {
            assert!(
                !tracing::enabled!(target: "agent_client_protocol::jsonrpc::handlers", Level::DEBUG),
                "ACP SDK JSON-RPC debug logs include raw UntypedMessage payloads"
            );
            assert!(
                tracing::enabled!(target: "aionui_ai_agent::protocol::acp", Level::DEBUG),
                "AionUi ACP sanitized debug summaries should still be available"
            );
        });
    }

    #[test]
    fn backend_filter_suppresses_raw_acp_sdk_jsonrpc_debug_even_when_debug_enabled() {
        let subscriber = tracing_subscriber::registry().with(build_backend_filter(Some("debug")));
        tracing::subscriber::with_default(subscriber, || {
            assert!(
                !tracing::enabled!(target: "agent_client_protocol::jsonrpc::handlers", Level::DEBUG),
                "ACP SDK JSON-RPC debug logs include raw UntypedMessage payloads"
            );
            assert!(
                tracing::enabled!(target: "aionui_ai_agent::protocol::acp", Level::DEBUG),
                "AionUi ACP sanitized debug summaries should still be available"
            );
        });
    }

    #[test]
    fn env_filter_suppresses_raw_aionrs_provider_debug_even_when_debug_enabled() {
        let subscriber = tracing_subscriber::registry().with(build_env_filter(Some("debug")));
        tracing::subscriber::with_default(subscriber, || {
            assert!(
                !tracing::enabled!(target: "aion_agent", Level::DEBUG),
                "aion_agent debug logs include raw request bodies"
            );
            assert!(
                !tracing::enabled!(target: "aion_providers", Level::DEBUG),
                "aion_providers debug logs include raw SSE chunks"
            );
            assert!(
                tracing::enabled!(target: "aionui_ai_agent::manager::aionrs::agent", Level::DEBUG),
                "AionUi aionrs lifecycle debug logs should still be available"
            );
        });
    }

    #[test]
    fn aionrs_file_level_suppresses_raw_provider_targets_even_when_debug_enabled() {
        let level = build_aionrs_level(Some("debug"));
        assert!(level.contains("aion_agent=info"), "{level}");
        assert!(level.contains("aion_providers=info"), "{level}");
        assert!(level.contains("aion_tools=debug"), "{level}");
    }

    #[test]
    fn dated_log_dir_appends_date_partition() {
        let root = Path::new("/tmp/aionui-logs");
        let dated = dated_log_dir(root);
        let relative = dated.strip_prefix(root).expect("dated log dir should stay under root");
        let parts = relative
            .iter()
            .map(|part| part.to_str().expect("log dir should be utf-8"))
            .collect::<Vec<_>>();

        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 4);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 2);
        assert!(parts[0].chars().all(|ch| ch.is_ascii_digit()));
        assert!(parts[1].chars().all(|ch| ch.is_ascii_digit()));
        assert!(parts[2].chars().all(|ch| ch.is_ascii_digit()));
    }

    #[test]
    fn dated_file_writer_moves_new_day_files_into_matching_day_directory() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let first_day = LogDate {
            year: 2026,
            month: 7,
            day: 2,
        };
        let second_day = LogDate {
            year: 2026,
            month: 7,
            day: 3,
        };
        let days = std::sync::Arc::new(std::sync::Mutex::new(vec![second_day, first_day]));
        let mut writer = DailyDatedLogWriter::new_with_date_provider(
            tmp.path().to_path_buf(),
            "aioncore.log",
            Box::new({
                let days = std::sync::Arc::clone(&days);
                move || days.lock().expect("date queue").pop().expect("date")
            }),
        );

        std::io::Write::write_all(&mut writer, b"july 2\n").expect("write first day");
        std::io::Write::write_all(&mut writer, b"july 3\n").expect("write second day");
        std::io::Write::flush(&mut writer).expect("flush");

        let first_path = tmp.path().join("2026/07/02/2026-07-02.aioncore.log");
        let second_path = tmp.path().join("2026/07/03/2026-07-03.aioncore.log");
        assert_eq!(std::fs::read_to_string(first_path).expect("first day log"), "july 2\n");
        assert_eq!(
            std::fs::read_to_string(second_path).expect("second day log"),
            "july 3\n"
        );
        assert!(!tmp.path().join("2026/07/02/2026-07-03.aioncore.log").exists());
    }

    #[test]
    fn dated_file_writer_rolls_by_size_and_caps_daily_parts() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let date = LogDate {
            year: 2026,
            month: 7,
            day: 3,
        };
        let mut writer = DailyDatedLogWriter::new_with_policy(
            tmp.path().to_path_buf(),
            "aioncore.log",
            Box::new(move || date),
            8,
            3,
            14,
        );

        for line in [&b"first\n"[..], &b"second\n"[..], &b"third\n"[..], &b"fourth\n"[..]] {
            std::io::Write::write_all(&mut writer, line).expect("write rolled line");
        }
        std::io::Write::flush(&mut writer).expect("flush");

        let day = tmp.path().join("2026/07/03");
        assert_eq!(
            std::fs::read_to_string(day.join("2026-07-03.aioncore.log")).expect("base log"),
            "second\n"
        );
        assert_eq!(
            std::fs::read_to_string(day.join("2026-07-03.1.aioncore.log")).expect("part one"),
            "third\n"
        );
        assert_eq!(
            std::fs::read_to_string(day.join("2026-07-03.2.aioncore.log")).expect("part two"),
            "fourth\n"
        );
        assert_eq!(std::fs::read_dir(day).expect("day logs").count(), 3);
    }

    #[test]
    fn cleanup_expired_log_partitions_keeps_retention_window_and_unknown_dirs() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let expired = tmp.path().join("2026/07/06");
        let boundary = tmp.path().join("2026/07/07");
        let current = tmp.path().join("2026/07/20");
        let unknown = tmp.path().join("manual");
        for dir in [&expired, &boundary, &current, &unknown] {
            std::fs::create_dir_all(dir).expect("create log partition");
            std::fs::write(dir.join("keep.log"), b"log").expect("write log");
        }

        cleanup_expired_log_partitions(
            tmp.path(),
            LogDate {
                year: 2026,
                month: 7,
                day: 20,
            },
            14,
        )
        .expect("cleanup logs");

        assert!(!expired.exists());
        assert!(boundary.exists());
        assert!(current.exists());
        assert!(unknown.exists());
    }

    #[test]
    fn select_log_root_uses_creatable_custom_dir_without_fallback() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let custom = tmp.path().join("custom-logs");
        let default = tmp.path().join("default-logs");

        let selection = select_log_root(Some(&custom), &default).expect("creatable custom dir should be selected");

        assert_eq!(selection.root, custom);
        assert!(selection.fallback.is_none());
        assert!(selection.active_dir.starts_with(&custom));
        assert!(selection.active_dir.is_dir());
    }

    #[test]
    fn select_log_root_falls_back_to_default_when_custom_dir_is_unusable() {
        let tmp = tempfile::tempdir().expect("temp dir");
        // A file occupying the custom path makes create_dir_all fail the same
        // way an unwritable path does (AIONUI-231 repro without root).
        let custom = tmp.path().join("occupied");
        std::fs::write(&custom, b"not a directory").expect("occupy custom path");
        let default = tmp.path().join("default-logs");

        let selection = select_log_root(Some(&custom), &default).expect("unusable custom dir must degrade, not fail");

        assert_eq!(selection.root, default);
        assert!(selection.active_dir.starts_with(&default));
        assert!(selection.active_dir.is_dir());
        let fallback = selection
            .fallback
            .expect("fallback details must be recorded for the warn log");
        assert_eq!(fallback.requested_dir, custom);
    }

    #[test]
    fn select_log_root_stays_fatal_with_error_kind_when_default_dir_is_unusable() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let default = tmp.path().join("occupied");
        std::fs::write(&default, b"not a directory").expect("occupy default path");

        let err = select_log_root(None, &default).expect_err("default dir failure must stay fatal");

        assert_eq!(err.code(), BootstrapErrorCode::LoggingInitFailed);
        assert_eq!(err.stage(), "logging.dir");
        let stderr = err.stderr_line();
        assert!(stderr.contains("BOOTSTRAP_LOGGING_INIT_FAILED"), "{stderr}");
        assert!(stderr.contains("errorKind="), "{stderr}");
    }

    #[test]
    fn select_log_root_stays_fatal_when_both_custom_and_default_dirs_are_unusable() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let custom = tmp.path().join("custom-occupied");
        let default = tmp.path().join("default-occupied");
        std::fs::write(&custom, b"not a directory").expect("occupy custom path");
        std::fs::write(&default, b"not a directory").expect("occupy default path");

        let err = select_log_root(Some(&custom), &default).expect_err("both dirs failing must stay fatal");

        assert_eq!(err.code(), BootstrapErrorCode::LoggingInitFailed);
        assert_eq!(err.stage(), "logging.dir");
        let stderr = err.stderr_line();
        assert!(stderr.contains("errorKind="), "{stderr}");
        assert!(stderr.contains("requestedErrorKind="), "{stderr}");
    }

    /// The only test allowed to call `init_tracing`: it registers the
    /// process-global subscriber, and a second registration would fail.
    #[test]
    fn init_tracing_survives_unusable_custom_dir_by_falling_back_to_default() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let custom = tmp.path().join("occupied");
        std::fs::write(&custom, b"not a directory").expect("occupy custom path");
        let default = tmp.path().join("default-logs");

        let _guards = init_tracing(Some(&custom), &default, Some("info"))
            .expect("bootstrap must survive an unusable custom log dir");

        assert!(dated_log_dir(&default).is_dir());
    }
}
