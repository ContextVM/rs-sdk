//! Internal tracing subscriber setup for ContextVM transports.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing::Event;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Registry};

use crate::core::error::{Error, Result};

static TRACING_SETUP_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static TRACING_INITIALIZED: OnceLock<()> = OnceLock::new();
static LOG_DESTINATION: OnceLock<Mutex<LogDestination>> = OnceLock::new();

fn tracing_setup_lock() -> &'static Mutex<()> {
    TRACING_SETUP_LOCK.get_or_init(|| Mutex::new(()))
}

fn log_destination() -> &'static Mutex<LogDestination> {
    LOG_DESTINATION.get_or_init(|| Mutex::new(LogDestination::default()))
}

pub(crate) fn init_tracer(log_file_path: Option<&str>) -> Result<()> {
    let _guard = tracing_setup_lock()
        .lock()
        .map_err(|_| Error::Other("failed to acquire tracing setup lock".to_string()))?;

    configure_file_output(log_file_path)?;

    if TRACING_INITIALIZED.get().is_some() {
        return Ok(());
    }

    let subscriber = Registry::default().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(ContextVmMakeWriter)
            .event_format(ContextVmEventFormatter)
            .with_filter(build_env_filter()),
    );

    match tracing::subscriber::set_global_default(subscriber) {
        Ok(()) => {
            let _ = TRACING_INITIALIZED.set(());
            Ok(())
        }
        Err(error) => {
            let text = error.to_string();
            if text.contains("global default trace dispatcher has already been set") {
                let _ = TRACING_INITIALIZED.set(());
                Ok(())
            } else {
                Err(Error::Other(format!(
                    "failed to initialize tracing subscriber: {text}"
                )))
            }
        }
    }
}

fn configure_file_output(log_file_path: Option<&str>) -> Result<()> {
    let Some(path) = normalize_log_file_path(log_file_path) else {
        return Ok(());
    };

    ensure_parent_exists(&path)?;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| {
            Error::Other(format!(
                "failed to open log file {}: {error}",
                path.display()
            ))
        })?;

    let mut destination = log_destination()
        .lock()
        .map_err(|_| Error::Other("failed to acquire log destination lock".to_string()))?;
    destination.file = Some(file);

    Ok(())
}

fn normalize_log_file_path(log_file_path: Option<&str>) -> Option<PathBuf> {
    let trimmed = log_file_path?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn ensure_parent_exists(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::Other(format!(
                    "failed to create log directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
    }

    Ok(())
}

fn build_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("contextvm_sdk=info,rmcp=warn"))
}

#[derive(Default)]
struct LogDestination {
    file: Option<File>,
}

#[derive(Clone, Copy)]
struct ContextVmMakeWriter;

impl<'a> MakeWriter<'a> for ContextVmMakeWriter {
    type Writer = ContextVmWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ContextVmWriter {
            stdout: io::stdout(),
        }
    }
}

struct ContextVmWriter {
    stdout: io::Stdout,
}

impl Write for ContextVmWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stdout.write_all(buf)?;

        if let Ok(mut destination) = log_destination().lock() {
            if let Some(file) = destination.file.as_mut() {
                let _ = file.write_all(buf);
            }
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()?;

        if let Ok(mut destination) = log_destination().lock() {
            if let Some(file) = destination.file.as_mut() {
                let _ = file.flush();
            }
        }

        Ok(())
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    extra_fields: Vec<(String, String)>,
}

impl MessageVisitor {
    fn record_field(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = Some(value);
        } else {
            self.extra_fields.push((name.to_string(), value));
        }
    }
}

impl tracing::field::Visit for MessageVisitor {
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record_field(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record_field(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record_field(field.name(), value.to_string());
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record_field(field.name(), value.to_string());
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.record_field(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.record_field(field.name(), format!("{value:?}"));
    }
}

struct ContextVmEventFormatter;

impl<S, N> FormatEvent<S, N> for ContextVmEventFormatter
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let metadata = event.metadata();
        let timestamp = unix_timestamp();
        let level = metadata.level().to_string().to_lowercase();
        let message = visitor.message.unwrap_or_default();

        write!(
            writer,
            "{timestamp}:{level}::{}:{message}",
            metadata.target()
        )?;

        for (key, value) in visitor.extra_fields {
            write!(writer, " {key}={value}")?;
        }

        writeln!(writer)
    }
}

fn unix_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now();
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:03}", duration.as_secs(), duration.subsec_millis())
}
