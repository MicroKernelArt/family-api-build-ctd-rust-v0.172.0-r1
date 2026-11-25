//! codex-transportd：Codex transport sidecar（TLS-CTD-001）。
//!
//! UDS-only 封闭 operation server，基于官方 rust-v0.169.1 transport。

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("codex_transportd=info")),
        )
        .init();

    let config = match codex_transportd::config::Config::from_env() {
        Ok(c) => c,
        Err(e) if e == "help" => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::from(2);
        }
    };

    match codex_transportd::server::serve(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("server error: {e}");
            ExitCode::from(1)
        }
    }
}
