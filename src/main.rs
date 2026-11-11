use std::process::ExitCode;

use tokio::net::TcpListener;
use tokio::signal;
use tracing::Level;

use trout::build_app;
use trout::config::{Config, LogFormat};

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("trout: {error}");

            return ExitCode::from(2);
        }
    };

    init_logging(&config);

    let listen = config.listen;
    let app = match build_app(config) {
        Ok(app) => app,
        Err(error) => {
            tracing::error!(%error, "cannot start");

            return ExitCode::FAILURE;
        }
    };
    let listener = match TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, %listen, "cannot bind");

            return ExitCode::FAILURE;
        }
    };

    tracing::info!(%listen, "trout listening");

    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!(%error, "server stopped with an error");

        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn init_logging(config: &Config) {
    let level = config.log.level.parse::<Level>().unwrap_or(Level::INFO);
    let builder = tracing_subscriber::fmt().with_max_level(level);

    match config.log.format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
    }
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
}
