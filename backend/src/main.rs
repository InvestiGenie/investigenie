mod ai;
mod api;
mod config;
mod integrations;
mod search;
mod storage;

use std::error::Error;

use config::Settings;
use storage::Storage;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let settings = Settings::from_env()?;
    match std::env::args().nth(1).as_deref() {
        Some("migrate") => {
            Storage::migrate(&settings.storage).await?;
            tracing::info!("Configuration schema migrated");
            return Ok(());
        }
        Some(_) => return Err("Unsupported command; use migrate or no arguments".into()),
        None => {}
    }
    let web_origin = settings.web_origin.parse()?;
    if settings.quickstart {
        if let storage::PersistenceOptions::Filesystem { path, .. } = &settings.storage {
            tracing::warn!(
                directory = %path.display(),
                "No storage settings found; using local quickstart storage with a generated encryption key. Set CONFIG_ENCRYPTION_KEY and PERSISTENCE_MODE for production"
            );
        }
    }
    let storage = Storage::open(settings.storage).await?;
    let mut app = api::router(web_origin, storage.clone());
    if let Some(directory) = &settings.web_dir {
        app = api::with_web(app, directory);
    }
    let app = app.layer(TraceLayer::new_for_http());
    let listener = TcpListener::bind(settings.bind).await?;

    tracing::info!(address = %settings.bind, "InvestiGenie API listening");
    match &settings.web_dir {
        Some(directory) => tracing::info!(
            ui = %directory.display(),
            "Open http://{} in your browser",
            settings.bind
        ),
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
