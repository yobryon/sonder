//! `sonder-relay --bind 0.0.0.0:8788` — see `docs/ROOMS.md`.

use clap::Parser;

#[derive(Parser)]
#[command(name = "sonder-relay", about = "WebSocket relay for shared Sonder rooms")]
struct Cli {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:8788")]
    bind: std::net::SocketAddr,
    /// Log filter (RUST_LOG syntax).
    #[arg(long, default_value = "info")]
    log: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(cli.log)).init();
    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "sonder-relay listening");
    axum::serve(listener, sonder_relay::router(sonder_relay::Relay::new())).await?;
    Ok(())
}
