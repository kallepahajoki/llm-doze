mod auth;
mod config;
mod lifecycle;
mod proxy;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{error, info};

use config::Config;
use server::ManagedServer;

#[derive(Parser)]
#[command(name = "llm-doze", about = "LLM reverse proxy with auto start/stop")]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/llm-doze/config.yaml")]
    config: PathBuf,

    /// Bind address (0.0.0.0 for all interfaces)
    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Check health status of all configured backends
    Status,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        Some(Commands::Status) => {
            run_status(&config).await;
            return Ok(());
        }
        None => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!(
        servers = config.servers.len(),
        config = %cli.config.display(),
        "loaded configuration"
    );

    let mut handles = Vec::new();

    for server_config in &config.servers {
        let auth_token = config
            .effective_token(server_config)
            .map(|s| s.to_string());
        let managed = ManagedServer::new(server_config.clone(), auth_token);

        let addr: SocketAddr = format!("{}:{}", cli.bind, server_config.listen).parse()?;

        // Spawn idle monitor
        let monitor_server = Arc::clone(&managed);
        tokio::spawn(lifecycle::idle_monitor(monitor_server));

        // Spawn listener
        let listener_server = Arc::clone(&managed);
        let handle = tokio::spawn(async move {
            if let Err(e) = run_listener(addr, listener_server).await {
                error!(error = %e, "listener failed");
            }
        });

        info!(
            name = %server_config.name,
            listen = %addr,
            backend = %server_config.backend,
            managed_subprocess = server_config.is_managed_subprocess(),
            "registered server"
        );

        handles.push(handle);
    }

    // Wait for all listeners (they run forever unless error)
    for handle in handles {
        handle.await?;
    }

    Ok(())
}

async fn run_status(config: &Config) {
    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
        .build_http::<http_body_util::Empty<bytes::Bytes>>();

    let mut results = Vec::new();

    for server_config in &config.servers {
        let url = server_config.backend_url(&server_config.health);
        let status = match client.get(url.parse::<hyper::Uri>().unwrap()).await {
            Ok(resp) if resp.status().is_success() => "running",
            Ok(resp) => {
                // Store the status string to avoid temp value issues
                results.push((server_config, format!("unhealthy ({})", resp.status())));
                continue;
            }
            Err(_) => "stopped",
        };
        results.push((server_config, status.to_string()));
    }

    // Print table
    let name_width = results
        .iter()
        .map(|(s, _)| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let backend_width = results
        .iter()
        .map(|(s, _)| s.backend.len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<name_width$}  {:>6}  {:<backend_width$}  {}",
        "NAME", "PORT", "BACKEND", "STATUS"
    );
    println!(
        "{:<name_width$}  {:>6}  {:<backend_width$}  {}",
        "─".repeat(name_width),
        "──────",
        "─".repeat(backend_width),
        "───────"
    );

    for (server_config, status) in &results {
        let indicator = match status.as_str() {
            "running" => "● running",
            "stopped" => "○ stopped",
            other => other,
        };
        println!(
            "{:<name_width$}  {:>6}  {:<backend_width$}  {}",
            server_config.name, server_config.listen, server_config.backend, indicator
        );
    }
}

async fn run_listener(
    addr: SocketAddr,
    server: Arc<ManagedServer>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    info!(addr = %addr, "listening");

    loop {
        let (stream, remote) = listener.accept().await?;
        let server = Arc::clone(&server);

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req| {
                let server = Arc::clone(&server);
                async move { proxy::handle_request(server, req).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(io, svc)
                .with_upgrades()
                .await
            {
                if !e.to_string().contains("connection closed") {
                    tracing::debug!(remote = %remote, error = %e, "connection error");
                }
            }
        });
    }
}
