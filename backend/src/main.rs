mod ai;
mod ai_connectors;
mod api;
mod config;
mod data_connectors;
mod logging;
mod search;
mod storage;

use std::error::Error;

use config::{Command, Configuration, Settings};
use storage::Storage;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("InvestiGenie failed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let configuration = Configuration::load(std::env::args_os().skip(1))?;
    if configuration.command == Command::Healthcheck {
        return healthcheck(&configuration).await;
    }
    let logging_settings = logging::LoggingSettings::from_configuration(&configuration)?;
    let _log_guard = logging::initialize(&logging_settings)?;
    tracing::info!(
        log_level = %logging_settings.level,
        log_mode = logging_settings.mode.as_str(),
        "Application logging initialized"
    );
    let settings = Settings::from_configuration(&configuration)?;
    match configuration.command {
        Command::Migrate => {
            Storage::migrate(&settings.storage).await?;
            tracing::info!("Configuration schema migrated");
            return Ok(());
        }
        Command::Serve => {}
        Command::Healthcheck => {
            unreachable!("healthcheck is handled before application initialization")
        }
    }
    let web_origin = settings.web_origin.parse()?;
    if settings.quickstart {
        if let storage::PersistenceOptions::Filesystem { path, .. } = &settings.storage {
            tracing::warn!(
                directory = %path.display(),
                "No explicit persistence mode or key configured; using local quickstart storage. Configure storage.mode and an encryption key for production"
            );
        }
    }
    let mut storage_operation = logging::Operation::new("storage", "open");
    let storage = Storage::open(settings.storage).await?;
    storage_operation.complete(200, None, None);
    let mut app = api::router(web_origin, storage.clone(), settings.ai_probe_timeout);
    if let Some(directory) = &settings.web_dir {
        app = api::with_web(app, directory);
    }
    let app = app.layer(axum::middleware::from_fn(logging::http_request));
    let listener = TcpListener::bind(settings.bind).await?;

    tracing::info!(address = %settings.bind, "InvestiGenie API listening");
    match &settings.web_dir {
        Some(directory) => {
            if settings.bind.ip().is_unspecified() {
                tracing::info!(
                    ui = %directory.display(),
                    "Open http://127.0.0.1:{} in your browser (in Docker, use the mapped host port)",
                    settings.bind.port()
                )
            } else {
                tracing::info!(
                    ui = %directory.display(),
                    "Open http://{} in your browser",
                    settings.bind
                )
            }
        }
        None => tracing::info!("No bundled web UI found; serve the frontend separately"),
    }
    tracing::info!("Datasource connections are managed from the Connections UI");
    tracing::warn!(
        "API authentication is not configured; restrict access through an authenticated gateway"
    );

    let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_receiver.await;
            })
            .await
    });
    let result = tokio::select! {
        result = &mut server => Some(result),
        _ = shutdown_signal() => {
            let _ = shutdown_sender.send(());
            match tokio::time::timeout(std::time::Duration::from_secs(30), &mut server).await {
                Ok(result) => Some(result),
                Err(_) => { server.abort(); None }
            }
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(5), storage.close())
        .await
        .is_err()
    {
        tracing::warn!("Configuration pool drain timed out");
    }
    if result.is_some_and(|result| !matches!(result, Ok(Ok(())))) {
        return Err("API server stopped unexpectedly".into());
    }

    tracing::info!("Application shutdown complete");
    Ok(())
}

/// Probes the local readiness endpoint; used by container health checks.
async fn healthcheck(configuration: &Configuration) -> Result<(), Box<dyn Error>> {
    let mut address: std::net::SocketAddr = configuration
        .value("RUST_API_BIND")
        .unwrap_or_else(|| "127.0.0.1:3000".to_owned())
        .parse()?;
    if address.ip().is_unspecified() {
        address.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?
        .get(format!("http://{address}/api/health/ready"))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("Readiness check returned {}", response.status()).into());
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "Failed to listen for Ctrl-C");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "Failed to listen for termination signal"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
