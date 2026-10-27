use std::{error::Error, path::PathBuf, time::Instant};

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use tracing::Instrument;
use tracing_appender::{non_blocking::WorkerGuard, rolling::Rotation};
use tracing_subscriber::{
    filter::{filter_fn, LevelFilter},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    Layer,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogMode {
    Console,
    File,
    Both,
}

impl LogMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::File => "file",
            Self::Both => "both",
        }
    }

    fn writes_file(self) -> bool {
        matches!(self, Self::File | Self::Both)
    }
    fn writes_console(self) -> bool {
        matches!(self, Self::Console | Self::Both)
    }
}

pub struct LoggingSettings {
    pub level: LevelFilter,
    pub mode: LogMode,
    pub path: PathBuf,
    pub max_files: usize,
}

impl LoggingSettings {
    pub fn from_configuration(
        configuration: &crate::config::Configuration,
    ) -> Result<Self, &'static str> {
        Self::from_values(|name| configuration.value(name))
    }

    fn from_values(get: impl Fn(&str) -> Option<String>) -> Result<Self, &'static str> {
        let level = match get("LOG_LEVEL")
            .unwrap_or_else(|| "info".to_owned())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "error" => LevelFilter::ERROR,
            "warn" => LevelFilter::WARN,
            "info" => LevelFilter::INFO,
            "debug" => LevelFilter::DEBUG,
            _ => return Err("LOG_LEVEL must be error, warn, info, or debug"),
        };
        let mode = match get("LOG_MODE").as_deref().map(str::trim) {
            None | Some("both") => LogMode::Both,
            Some("console") => LogMode::Console,
            Some("file") => LogMode::File,
            _ => return Err("logging.mode / LOG_MODE must be console, file, or both"),
        };
        if !mode.writes_file() {
            return Ok(Self {
                level,
                mode,
                path: PathBuf::from("logs"),
                max_files: 7,
            });
        }
        let path = get("LOG_PATH").unwrap_or_else(|| "logs".to_owned());
        if path.trim().is_empty() {
            return Err("logging.path / LOG_PATH must be nonempty when file logging is enabled");
        }
        let max_files = get("LOG_MAX_FILES")
            .unwrap_or_else(|| "7".to_owned())
            .parse::<usize>()
            .ok()
            .filter(|count| (1..=365).contains(count))
            .ok_or("LOG_MAX_FILES must be between 1 and 365")?;
        Ok(Self {
            level,
            mode,
            path: path.into(),
            max_files,
        })
    }
}

pub fn initialize(settings: &LoggingSettings) -> Result<Option<WorkerGuard>, Box<dyn Error>> {
    let (writer, guard) = if settings.mode.writes_file() {
        std::fs::create_dir_all(&settings.path)
            .map_err(|_| "Could not initialize the log directory")?;
        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("investigenie")
            .filename_suffix("log")
            .max_log_files(settings.max_files)
            .build(&settings.path)
            .map_err(|_| "Could not initialize the log directory")?;
        let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .buffered_lines_limit(1024)
            .lossy(true)
            .finish(appender);
        (Some(writer), Some(guard))
    } else {
        (None, None)
    };
    let console = settings
        .mode
        .writes_console()
        .then(|| tracing_subscriber::fmt::layer().with_filter(filter_fn(application_event)));
    let file = writer.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(filter_fn(application_event))
    });
    tracing_subscriber::registry()
        .with(settings.level)
        .with(console)
        .with(file)
        .try_init()?;
    Ok(guard)
}

fn application_event(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.target() == "investigenie_api" || metadata.target().starts_with("investigenie_api::")
}

pub struct Operation {
    category: &'static str,
    operation: &'static str,
    started: Instant,
    completed: bool,
}

impl Operation {
    pub fn new(category: &'static str, operation: &'static str) -> Self {
        tracing::debug!(category, operation, "Operation started");
        Self {
            category,
            operation,
            started: Instant::now(),
            completed: false,
        }
    }

    pub fn complete(
        &mut self,
        status: u16,
        error_code: Option<&'static str>,
        records: Option<usize>,
    ) {
        self.completed = true;
        macro_rules! completed {
            ($level:expr) => {
                tracing::event!(
                    $level,
                    category = self.category,
                    operation = self.operation,
                    elapsed_ms = self.started.elapsed().as_millis() as u64,
                    status,
                    error_code,
                    records,
                    "Operation completed"
                );
            };
        }
        if status >= 500 {
            completed!(tracing::Level::ERROR);
        } else if status >= 400 || error_code.is_some() {
            completed!(tracing::Level::WARN);
        } else {
            completed!(tracing::Level::DEBUG);
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if !self.completed {
            tracing::debug!(
                category = self.category,
                operation = self.operation,
                elapsed_ms = self.started.elapsed().as_millis() as u64,
                "Operation cancelled or interrupted"
            );
        }
    }
}

pub async fn http_request(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched_or_static");
    let span = tracing::error_span!("http_request", request_id = %uuid::Uuid::new_v4(), method = %request.method(), route);
    async move {
        let mut operation = Operation::new("http", "request");
        let response = next.run(request).await;
        operation.complete(response.status().as_u16(), None, None);
        response
    }
    .instrument(span)
    .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    pub(crate) struct CapturedLogs {
        pub subscriber: tracing::Dispatch,
        pub bytes: Arc<Mutex<Vec<u8>>>,
    }

    #[derive(Clone)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub(crate) fn capture() -> CapturedLogs {
        capture_at(LevelFilter::TRACE)
    }

    fn capture_at(level: LevelFilter) -> CapturedLogs {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturedWriter(bytes.clone());
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .with_filter(filter_fn(move |metadata| {
                    application_event(metadata) && level >= *metadata.level()
                })),
        );
        CapturedLogs {
            subscriber: tracing::Dispatch::new(subscriber),
            bytes,
        }
    }

    #[tokio::test]
    async fn request_logs_use_route_templates_and_never_bodies_headers_or_query_strings() {
        use tower::ServiceExt;
        use tracing::instrument::WithSubscriber;
        let captured = capture();
        let app = axum::Router::new()
            .route(
                "/api/connections/{id}/search",
                axum::routing::post(|| async { "ok" }),
            )
            .layer(axum::middleware::from_fn(http_request));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/connections/private-id/search?token=private-query")
            .header("authorization", "Bearer private-header")
            .body(axum::body::Body::from("private-body"))
            .unwrap();
        let response = app
            .oneshot(request)
            .with_subscriber(captured.subscriber.clone())
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        tracing::dispatcher::with_default(&captured.subscriber, || {
            tracing::debug!(target: "reqwest", "private-wire-body");
            drop(Operation::new("datasource", "cancelled_test"));
        });
        let text = String::from_utf8(captured.bytes.lock().unwrap().clone()).unwrap();
        assert!(text.contains("/api/connections/{id}/search"));
        assert!(text.contains("request_id"));
        assert!(text.contains("status=200"));
        assert!(text.contains("cancelled or interrupted"));
        for secret in [
            "private-id",
            "private-query",
            "private-header",
            "private-body",
            "private-wire-body",
        ] {
            assert!(
                !text.contains(secret),
                "Unexpected private content in request logs"
            );
        }
    }

    fn settings(values: &[(&str, &str)]) -> Result<LoggingSettings, &'static str> {
        LoggingSettings::from_values(|name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn logging_level_and_file_output_are_independent_and_bounded() {
        let defaults = settings(&[]).unwrap();
        assert_eq!(defaults.level, LevelFilter::INFO);
        assert_eq!(defaults.mode, LogMode::Both);
        assert_eq!(defaults.max_files, 7);
        assert!(settings(&[("LOG_MODE", "console"), ("LOG_MAX_FILES", "invalid")]).is_ok());
        let enabled = settings(&[
            ("LOG_LEVEL", "debug"),
            ("LOG_MODE", "file"),
            ("LOG_PATH", "custom/logs"),
            ("LOG_MAX_FILES", "3"),
        ])
        .unwrap();
        assert_eq!(enabled.level, LevelFilter::DEBUG);
        assert_eq!(enabled.mode, LogMode::File);
        assert_eq!(enabled.path, PathBuf::from("custom/logs"));
        assert_eq!(enabled.max_files, 3);
        for (name, level) in [
            ("error", LevelFilter::ERROR),
            ("warn", LevelFilter::WARN),
            ("info", LevelFilter::INFO),
            (" DEBUG ", LevelFilter::DEBUG),
        ] {
            for mode in ["console", "file", "both"] {
                let parsed = settings(&[("LOG_LEVEL", name), ("LOG_MODE", mode)]).unwrap();
                assert_eq!(parsed.level, level);
                assert_eq!(parsed.mode.as_str(), mode);
            }
        }
        for values in [
            vec![("LOG_LEVEL", "trace")],
            vec![("LOG_LEVEL", "invalid")],
            vec![("LOG_MODE", "invalid")],
            vec![("LOG_PATH", " ")],
            vec![("LOG_MAX_FILES", "0")],
            vec![("LOG_MAX_FILES", "366")],
        ] {
            assert!(settings(&values).is_err());
        }
    }

    #[test]
    fn operation_failures_remain_visible_at_warn_and_error_levels() {
        for level in [
            LevelFilter::ERROR,
            LevelFilter::WARN,
            LevelFilter::INFO,
            LevelFilter::DEBUG,
        ] {
            let captured = capture_at(level);
            tracing::dispatcher::with_default(&captured.subscriber, || {
                tracing::info!("Lifecycle marker");
                Operation::new("datasource", "success_marker").complete(200, None, Some(50));
                Operation::new("datasource", "warning_marker").complete(
                    400,
                    Some("invalid_query"),
                    None,
                );
                Operation::new("datasource", "failure_marker").complete(
                    502,
                    Some("source_unavailable"),
                    None,
                );
            });
            let text = String::from_utf8(captured.bytes.lock().unwrap().clone()).unwrap();
            assert!(text.contains("failure_marker"));
            assert_eq!(text.contains("warning_marker"), level >= LevelFilter::WARN);
            assert_eq!(
                text.contains("Lifecycle marker"),
                level >= LevelFilter::INFO
            );
            assert_eq!(text.contains("success_marker"), level >= LevelFilter::DEBUG);
        }
    }

    #[tokio::test]
    async fn error_level_preserves_request_context_without_private_values() {
        use tower::ServiceExt;
        use tracing::instrument::WithSubscriber;
        let captured = capture_at(LevelFilter::ERROR);
        let app = axum::Router::new()
            .route(
                "/api/connections/{id}/search",
                axum::routing::post(|| async { axum::http::StatusCode::BAD_GATEWAY }),
            )
            .layer(axum::middleware::from_fn(http_request));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/connections/private-id/search?token=private-query")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app
            .oneshot(request)
            .with_subscriber(captured.subscriber.clone())
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
        let text = String::from_utf8(captured.bytes.lock().unwrap().clone()).unwrap();
        for expected in [
            "request_id",
            "/api/connections/{id}/search",
            "status=502",
            "ERROR",
        ] {
            assert!(
                text.contains(expected),
                "Missing failure context: {expected}"
            );
        }
        assert!(!text.contains("private-id"));
        assert!(!text.contains("private-query"));
        assert!(!text.contains("Operation started"));
    }

    #[test]
    fn debug_file_writer_flushes_without_terminal_escape_codes() {
        let directory = tempfile::tempdir().unwrap();
        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("investigenie")
            .filename_suffix("log")
            .max_log_files(2)
            .build(directory.path())
            .unwrap();
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(writer)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(operation = "probe", "Debug log test")
        });
        drop(guard);
        let files = std::fs::read_dir(directory.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(content.contains("Debug log test"));
        assert!(content.contains("probe"));
        assert!(!content.contains('\u{1b}'));
    }
}
