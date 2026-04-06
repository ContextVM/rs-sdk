use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_TARGET: &str = "contextvm_sdk";

static GLOBAL_LOG_FILE_PATH: OnceLock<RwLock<Option<String>>> = OnceLock::new();

fn global_log_file_path_store() -> &'static RwLock<Option<String>> {
    GLOBAL_LOG_FILE_PATH.get_or_init(|| RwLock::new(None))
}

fn normalize_path(path: Option<String>) -> Option<String> {
    path.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// Set the global log file path used when no per-call path is supplied.
pub fn set_global_log_file_path(file_path: Option<String>) {
    let normalized = normalize_path(file_path);
    if let Ok(mut guard) = global_log_file_path_store().write() {
        *guard = normalized;
    }
}

/// Read the currently configured global log file path.
pub fn get_global_log_file_path() -> Option<String> {
    global_log_file_path_store()
        .read()
        .ok()
        .and_then(|guard| guard.clone())
}

/// Generic log level for string-based logging.
#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

fn timestamp_string() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0));
    format!("{}.{:03}", duration.as_secs(), duration.subsec_millis())
}

fn resolve_file_path(file_path: Option<&str>) -> Option<String> {
    if let Some(path) = file_path {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    get_global_log_file_path()
}

fn append_log_to_file(line: &str, file_path: Option<&str>) {
    let Some(file_path) = resolve_file_path(file_path) else {
        return;
    };

    let path = Path::new(file_path.as_str());

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(error) = fs::create_dir_all(parent) {
                eprintln!(
                    "{}:warn:{}: Failed to create log file parent directory {}: {error}",
                    timestamp_string(),
                    DEFAULT_TARGET,
                    parent.display()
                );
                return;
            }
        }
    }

    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "{}:warn:{}: Failed to open log file {}: {error}",
                timestamp_string(),
                DEFAULT_TARGET,
                path.display()
            );
            return;
        }
    };

    if let Err(error) = writeln!(file, "{line}") {
        eprintln!(
            "{}:warn:{}: Failed to write log entry to {}: {error}",
            timestamp_string(),
            DEFAULT_TARGET,
            path.display()
        );
    }
}

fn emit_console(level: LogLevel, line: &str) {
    match level {
        LogLevel::Warn | LogLevel::Error => eprintln!("{line}"),
        LogLevel::Debug | LogLevel::Info => println!("{line}"),
    }
}

fn log_internal(level: LogLevel, target: &str, message: impl AsRef<str>, file_path: Option<&str>) {
    let line = format!(
        "{}:{}:{}: {}",
        timestamp_string(),
        level.as_str(),
        target,
        message.as_ref()
    );

    emit_console(level, &line);
    append_log_to_file(&line, file_path);
}

fn target_or_default(target: Option<&str>) -> &str {
    target.filter(|t| !t.is_empty()).unwrap_or(DEFAULT_TARGET)
}

fn log_with_optional_target(
    level: LogLevel,
    target: Option<&str>,
    message: impl AsRef<str>,
    file_path: Option<&str>,
) {
    log_internal(level, target_or_default(target), message, file_path);
}

/// Log plain text at the given level.
///
/// Accepts both `&str` and `String` via `AsRef<str>`.
pub fn log(level: LogLevel, message: impl AsRef<str>) {
    log_with_optional_target(level, None, message, None);
}

/// Log plain text at the given level and optionally append to a file.
pub fn log_with_file(level: LogLevel, message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_optional_target(level, None, message, file_path);
}

/// Log plain text at the given level with an explicit module target.
pub fn log_with_target(level: LogLevel, target: &str, message: impl AsRef<str>) {
    log_with_optional_target(level, Some(target), message, None);
}

/// Log plain text at the given level with an explicit target and optional file output.
pub fn log_with_target_and_file(
    level: LogLevel,
    target: &str,
    message: impl AsRef<str>,
    file_path: Option<&str>,
) {
    log_with_optional_target(level, Some(target), message, file_path);
}

pub fn debug(message: impl AsRef<str>) {
    log(LogLevel::Debug, message);
}

pub fn info(message: impl AsRef<str>) {
    log(LogLevel::Info, message);
}

pub fn warn(message: impl AsRef<str>) {
    log(LogLevel::Warn, message);
}

pub fn error(message: impl AsRef<str>) {
    log(LogLevel::Error, message);
}

pub fn debug_with_file(message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_file(LogLevel::Debug, message, file_path);
}

pub fn info_with_file(message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_file(LogLevel::Info, message, file_path);
}

pub fn warn_with_file(message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_file(LogLevel::Warn, message, file_path);
}

pub fn error_with_file(message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_file(LogLevel::Error, message, file_path);
}

pub fn debug_with_target(target: &str, message: impl AsRef<str>) {
    log_with_target(LogLevel::Debug, target, message);
}

pub fn info_with_target(target: &str, message: impl AsRef<str>) {
    log_with_target(LogLevel::Info, target, message);
}

pub fn warn_with_target(target: &str, message: impl AsRef<str>) {
    log_with_target(LogLevel::Warn, target, message);
}

pub fn error_with_target(target: &str, message: impl AsRef<str>) {
    log_with_target(LogLevel::Error, target, message);
}

pub fn debug_with_target_and_file(target: &str, message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_target_and_file(LogLevel::Debug, target, message, file_path);
}

pub fn info_with_target_and_file(target: &str, message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_target_and_file(LogLevel::Info, target, message, file_path);
}

pub fn warn_with_target_and_file(target: &str, message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_target_and_file(LogLevel::Warn, target, message, file_path);
}

pub fn error_with_target_and_file(target: &str, message: impl AsRef<str>, file_path: Option<&str>) {
    log_with_target_and_file(LogLevel::Error, target, message, file_path);
}
