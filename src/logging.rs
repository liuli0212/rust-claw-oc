use crate::config::AppConfig;
use std::fs;
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};

pub fn init_logging(
    _config: &AppConfig,
) -> Result<Option<WorkerGuard>, Box<dyn std::error::Error + Send + Sync>> {
    // 1. File Logging
    // We always want to log to file if possible, or if configured via ENV.
    // Default to strict env filter for file logs (debug or info).
    let file_filter = std::env::var("CLAW_LOG_LEVEL")
        .ok()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .and_then(|v| {
            // Validate the filter string by trying to parse it
            EnvFilter::try_new(v).ok()
        })
        .unwrap_or_else(|| EnvFilter::new("info"))
        .add_directive("rustyline=off".parse().unwrap());


    let enable_file = std::env::var("CLAW_FILE_LOG")
            .map(|v| v != "0")
            .unwrap_or(true);

    let log_dir = std::env::var("CLAW_LOG_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("logs"));

    if enable_file {
        if !log_dir.exists() {
            fs::create_dir_all(&log_dir)?;
        }
    }

    let file_appender = tracing_appender::rolling::daily(&log_dir, "claw.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_filter(file_filter);

    // Only create console layer if explicitly requested.
    // By default, stdout is reserved exclusively for CLI output (print!/println!).
    let console_layer = std::env::var("CLAW_CONSOLE_LOG_LEVEL")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .map(|filter| {
            fmt::layer()
                .with_target(false)
                .with_filter(filter.add_directive("rustyline=off".parse().unwrap()))
        });

    if enable_file {
        tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .try_init()?;
        Ok(Some(guard))
    } else {
        tracing_subscriber::registry()
            .with(console_layer)
            .try_init()?;
        Ok(None)
    }
}
