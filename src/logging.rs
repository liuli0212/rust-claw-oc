use crate::config::AppConfig;
use opentelemetry::global;
use opentelemetry::trace::TracerProvider;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use std::fs;
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub fn init_logging(
    _config: &AppConfig,
) -> Result<Option<WorkerGuard>, Box<dyn std::error::Error + Send + Sync>> {
    let base_filter = with_common_directives(
        std::env::var("CLAW_LOG_LEVEL")
            .ok()
            .or_else(|| std::env::var("RUST_LOG").ok())
            .and_then(|v| EnvFilter::try_new(v).ok())
            .unwrap_or_else(|| EnvFilter::new("info")),
    );
    let console_filter = std::env::var("CLAW_CONSOLE_LOG_LEVEL")
        .ok()
        .map(EnvFilter::try_new)
        .transpose()?
        .map(with_common_directives);

    let enable_file = std::env::var("CLAW_FILE_LOG")
        .map(|v| v != "0")
        .unwrap_or(true);
    let console_enabled = console_filter.is_some();
    let json_logs = std::env::var("CLAW_LOG_JSON")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let log_dir = std::env::var("CLAW_LOG_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("logs"));

    if enable_file && !log_dir.exists() {
        fs::create_dir_all(&log_dir)?;
    }

    let file_appender = tracing_appender::rolling::daily(&log_dir, "claw.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let otlp_tracer = build_otlp_tracer()?;

    match (json_logs, enable_file, console_enabled, otlp_tracer) {
        (true, true, true, Some(tracer)) => tracing_subscriber::registry()
            .with(base_filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_filter(console_filter.clone().unwrap()),
            )
            .with(
                fmt::layer()
                    .json()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_target(true),
            )
            .try_init()?,
        (true, true, false, Some(tracer)) => tracing_subscriber::registry()
            .with(base_filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .with(
                fmt::layer()
                    .json()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_target(true),
            )
            .try_init()?,
        (true, false, true, Some(tracer)) => tracing_subscriber::registry()
            .with(base_filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_filter(console_filter.clone().unwrap()),
            )
            .try_init()?,
        (true, false, false, Some(tracer)) => tracing_subscriber::registry()
            .with(base_filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .try_init()?,
        (false, true, true, Some(tracer)) => tracing_subscriber::registry()
            .with(base_filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_filter(console_filter.clone().unwrap()),
            )
            .with(
                fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_target(true),
            )
            .try_init()?,
        (false, true, false, Some(tracer)) => tracing_subscriber::registry()
            .with(base_filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .with(
                fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_target(true),
            )
            .try_init()?,
        (false, false, true, Some(tracer)) => tracing_subscriber::registry()
            .with(base_filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_filter(console_filter.clone().unwrap()),
            )
            .try_init()?,
        (false, false, false, Some(tracer)) => tracing_subscriber::registry()
            .with(base_filter)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .try_init()?,
        (true, true, true, None) => tracing_subscriber::registry()
            .with(base_filter)
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_filter(console_filter.clone().unwrap()),
            )
            .with(
                fmt::layer()
                    .json()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_target(true),
            )
            .try_init()?,
        (true, true, false, None) => tracing_subscriber::registry()
            .with(base_filter)
            .with(
                fmt::layer()
                    .json()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_current_span(true)
                    .with_span_list(true)
                    .with_target(true),
            )
            .try_init()?,
        (true, false, true, None) => tracing_subscriber::registry()
            .with(base_filter)
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_filter(console_filter.clone().unwrap()),
            )
            .try_init()?,
        (true, false, false, None) => tracing_subscriber::registry()
            .with(base_filter)
            .try_init()?,
        (false, true, true, None) => tracing_subscriber::registry()
            .with(base_filter)
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_filter(console_filter.clone().unwrap()),
            )
            .with(
                fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_target(true),
            )
            .try_init()?,
        (false, true, false, None) => tracing_subscriber::registry()
            .with(base_filter)
            .with(
                fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_target(true),
            )
            .try_init()?,
        (false, false, true, None) => tracing_subscriber::registry()
            .with(base_filter)
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_filter(console_filter.unwrap()),
            )
            .try_init()?,
        (false, false, false, None) => tracing_subscriber::registry()
            .with(base_filter)
            .try_init()?,
    };

    Ok(enable_file.then_some(guard))
}

fn with_common_directives(filter: EnvFilter) -> EnvFilter {
    filter.add_directive("rustyline=off".parse().unwrap())
}

fn build_otlp_tracer() -> Result<Option<SdkTracer>, Box<dyn std::error::Error + Send + Sync>> {
    let endpoint = match std::env::var("CLAW_OTLP_ENDPOINT") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return Ok(None),
    };

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_attributes([KeyValue::new("service.name", "rusty-claw")])
                .build(),
        )
        .build();

    let tracer = provider.tracer("rusty-claw");
    global::set_tracer_provider(provider);
    Ok(Some(tracer))
}
