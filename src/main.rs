use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crumb::apns::ApnsClient;
use crumb::config::Config;
use crumb::dedupe::Dedupe;
use crumb::dispatch::Gateway;
use crumb::server::router;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Longer than any sensible homeserver retry backoff.
const DEDUPE_TTL: Duration = Duration::from_secs(15 * 60);
const DEDUPE_CAP: usize = 100_000;
/// The most a homeserver waits for an answer.
const DEADLINE: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("crumb=info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::var_os("CRUMB_CONFIG").ok_or("CRUMB_CONFIG is not set")?;
    let config = Config::load(path.as_ref())?;

    let apps: HashMap<String, ApnsClient> = config
        .apps
        .into_iter()
        .map(|(app_id, app)| {
            info!(%app_id, endpoint = ?app.endpoint, "serving app");
            (app_id, ApnsClient::from(app))
        })
        .collect();
    let gateway = Arc::new(Gateway::new(
        apps,
        Dedupe::new(DEDUPE_TTL, DEDUPE_CAP),
        DEADLINE,
    ));

    let listener = TcpListener::bind(config.listen).await?;
    info!(listen = %config.listen, "crumb is up");
    axum::serve(listener, router(gateway))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Podman stops a container with SIGTERM; a terminal sends SIGINT.
async fn shutdown() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => term,
                Err(e) => {
                    error!("cannot listen for SIGTERM: {e}");
                    let _ = interrupt.await;
                    return;
                }
            };
        tokio::select! {
            _ = interrupt => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
    info!("shutting down");
}
