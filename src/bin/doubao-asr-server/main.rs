mod app;
mod audio_input;
mod protocol;

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use app::{AppConfig, DoubaoTranscriber};
use clap::Parser;
use doubao_asr::Client;
use reqwest::redirect::Policy;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "doubao-asr-server",
    version = env!("CARGO_PKG_VERSION"),
    about = "OpenAI-compatible Chat Completions server backed by Doubao IME ASR."
)]
struct Args {
    /// Address on which the HTTP server listens
    #[arg(long, env = "DOUBAO_ASR_LISTEN", default_value = "127.0.0.1:8000")]
    listen: SocketAddr,
    /// Credential file path (default: OS user config directory)
    #[arg(long, env = "DOUBAO_ASR_CREDENTIALS")]
    credentials: Option<PathBuf>,
    /// Model ID advertised through the OpenAI-compatible API
    #[arg(long, env = "DOUBAO_ASR_MODEL", default_value = "doubao-asr")]
    model: String,
    /// Doubao upload pacing multiplier (0 < speed <= 4)
    #[arg(long, env = "DOUBAO_ASR_UPLOAD_SPEED", default_value_t = 1.0)]
    upload_speed: f64,
    /// Maximum number of simultaneous requests
    #[arg(long, env = "DOUBAO_ASR_MAX_CONCURRENCY", default_value_t = 2)]
    max_concurrency: usize,
    /// End-to-end request timeout in seconds
    #[arg(long, env = "DOUBAO_ASR_REQUEST_TIMEOUT_SECS", default_value_t = 1800)]
    request_timeout_secs: u64,
    /// Proactive credential refresh interval in seconds (0 disables it)
    #[arg(
        long,
        env = "DOUBAO_ASR_CREDENTIAL_REFRESH_SECS",
        default_value_t = 21600
    )]
    credential_refresh_secs: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "server stopped");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.model.trim().is_empty() {
        return Err("model must not be empty".into());
    }
    if args.max_concurrency == 0 {
        return Err("max concurrency must be greater than zero".into());
    }
    if args.request_timeout_secs == 0 {
        return Err("request timeout must be greater than zero".into());
    }

    let mut client = Client::builder().upload_speed(args.upload_speed);
    if let Some(path) = args.credentials {
        client = client.credentials_path(path);
    }
    if args.credential_refresh_secs > 0 {
        client =
            client.credential_refresh_interval(Duration::from_secs(args.credential_refresh_secs));
    }
    let client = client.build()?;
    let maintenance = (args.credential_refresh_secs > 0).then(|| {
        let client = client.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(60));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                match client.refresh_credentials_if_due().await {
                    Ok(true) => tracing::info!("Doubao credentials refreshed"),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "Doubao credential refresh failed; retaining current credentials")
                    }
                }
            }
        })
    });
    let transcriber = DoubaoTranscriber::new(client);
    let downloader = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(Policy::limited(5))
        .build()?;
    let config = AppConfig {
        model: Arc::from(args.model),
        api_key: std::env::var("DOUBAO_ASR_API_KEY")
            .ok()
            .filter(|value| !value.is_empty())
            .map(Arc::from),
        max_concurrency: args.max_concurrency,
        request_timeout: Duration::from_secs(args.request_timeout_secs),
    };
    let router = app::router(config, transcriber, downloader);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(
        address = %listener.local_addr()?,
        credential_refresh_secs = args.credential_refresh_secs,
        "doubao ASR server listening"
    );
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Some(maintenance) = maintenance {
        maintenance.abort();
        let _ = maintenance.await;
    }
    result?;
    Ok(())
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
    tracing::info!("shutdown signal received");
}
