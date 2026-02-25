use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    pub log_level: Option<String>,
    pub file_log: Option<bool>,
    pub log_dir: Option<String>,
    pub log_file: Option<String>,
}

pub fn init_logging(
    config: LoggingConfig,
) -> Result<Option<WorkerGuard>, Box<dyn std::error::Error>> {
    let file_filter = EnvFilter::try_from_default_env()
        .or_else(|_| {
            let mut level = config
                .log_level
                .clone()
                .or_else(|| std::env::var("CLAW_LOG_LEVEL").ok())
                .unwrap_or_else(|| "info".to_string());
            if !level.contains("rustyline=") {
                level.push_str(",rustyline=warn");
            }
            EnvFilter::try_new(level)
        })
        .unwrap_or_else(|_| EnvFilter::new("info,rustyline=warn"));

    let console_filter = std::env::var("CLAW_CONSOLE_LOG_LEVEL")
        .ok()
        .and_then(|mut v| {
            if !v.contains("rustyline=") {
                v.push_str(",rustyline=warn");
            }
            EnvFilter::try_new(v).ok()
        })
        .unwrap_or_else(|| EnvFilter::new("warn,rustyline=warn"));

    let enable_file = config.file_log.unwrap_or_else(|| {
        std::env::var("CLAW_FILE_LOG")
            .map(|v| v != "0")
            .unwrap_or(true)
    });

    if !enable_file {
        let console_layer = fmt::layer()
            .with_target(false)
            .with_filter(console_filter);
        tracing_subscriber::registry()
            .with(console_layer)
            .try_init()?;
        return Ok(None);
    }

    let log_dir = config
        .log_dir
        .or_else(|| std::env::var("CLAW_LOG_DIR").ok())
        .unwrap_or_else(|| "logs".to_string());
    let log_file = config
        .log_file
        .or_else(|| std::env::var("CLAW_LOG_FILE").ok())
        .unwrap_or_else(|| "rusty-claw.log".to_string());
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(log_dir, log_file);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let console_layer = fmt::layer()
        .with_target(false)
        .with_filter(console_filter);

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(non_blocking)
        .with_target(true)
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init()?;

    Ok(Some(guard))
}
