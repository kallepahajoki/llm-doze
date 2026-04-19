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
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{error, info};

use config::Config;
use server::ManagedServer;

const SOCK_PATH: &str = "/run/llm-doze.sock";

#[derive(Parser)]
#[command(name = "llm-doze", about = "LLM reverse proxy with auto start/stop")]
#[command(subcommand_required = true, arg_required_else_help = true)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "/etc/llm-doze/config.yaml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the proxy server
    Serve {
        /// Bind address (0.0.0.0 for all interfaces)
        #[arg(short, long, default_value = "0.0.0.0")]
        bind: String,
    },
    /// Check status of all configured backends
    Status,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        Commands::Status => {
            run_status().await;
            return Ok(());
        }
        Commands::Serve { ref bind } => {
            run_serve(&config, bind).await?;
        }
    }

    Ok(())
}

async fn run_serve(config: &Config, bind: &str) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!(
        servers = config.servers.len(),
        "loaded configuration"
    );

    let mut servers: Vec<Arc<ManagedServer>> = Vec::new();
    let mut handles = Vec::new();

    // Probe backends to detect already-running services
    let probe_client =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http::<http_body_util::Empty<bytes::Bytes>>();

    for server_config in &config.servers {
        let auth_token = config
            .effective_token(server_config)
            .map(|s| s.to_string());
        let managed = ManagedServer::new(server_config.clone(), auth_token);

        // Check if backend is already running
        let url = server_config.backend_url(&server_config.health);
        if let Ok(resp) = probe_client
            .get(url.parse::<hyper::Uri>().unwrap())
            .await
        {
            if resp.status().is_success() {
                info!(
                    server = %server_config.name,
                    "backend already running, tracking for idle shutdown"
                );
                managed.set_state(server::ServerState::Running).await;
                managed.touch().await;
            }
        }

        servers.push(Arc::clone(&managed));

        let addr: SocketAddr = format!("{}:{}", bind, server_config.listen).parse()?;

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

    // Spawn management socket
    let mgmt_servers = servers.clone();
    tokio::spawn(async move {
        if let Err(e) = run_management_socket(mgmt_servers).await {
            error!(error = %e, "management socket failed");
        }
    });

    // Wait for all listeners (they run forever unless error)
    for handle in handles {
        handle.await?;
    }

    Ok(())
}


async fn run_management_socket(
    servers: Vec<Arc<ManagedServer>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Clean up stale socket
    let _ = std::fs::remove_file(SOCK_PATH);

    let listener = tokio::net::UnixListener::bind(SOCK_PATH)?;
    info!(path = SOCK_PATH, "management socket listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let servers = servers.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |_req| {
                let servers = servers.clone();
                async move { build_status_response(&servers).await }
            });

            let _ = http1::Builder::new().serve_connection(io, svc).await;
        });
    }
}

async fn build_status_response(
    servers: &[Arc<ManagedServer>],
) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, hyper::Error> {
    let mut entries = Vec::new();
    for s in servers {
        let state = s.get_state().await;
        let idle = s.idle_seconds().await;
        entries.push(format!(
            "{{\"name\":\"{}\",\"port\":{},\"backend\":\"{}\",\"state\":\"{}\",\"idle_seconds\":{},\"idle_timeout\":{}}}",
            s.config.name, s.config.listen, s.config.backend, state, idle, s.config.idle_timeout
        ));
    }
    let body = format!("[{}]", entries.join(","));

    Ok(hyper::Response::builder()
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))
        .unwrap())
}

async fn run_status() {
    // Connect to management socket
    let stream = match tokio::net::UnixStream::connect(SOCK_PATH).await {
        Ok(s) => s,
        Err(_) => {
            eprintln!("Cannot connect to llm-doze (is it running?)");
            eprintln!("Socket: {}", SOCK_PATH);
            std::process::exit(1);
        }
    };

    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(conn);

    let req = hyper::Request::builder()
        .uri("/status")
        .header("host", "localhost")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();

    let resp = sender.send_request(req).await.unwrap();
    let body = resp.into_body();

    use http_body_util::BodyExt;
    let bytes = body.collect().await.unwrap().to_bytes();
    let json: Vec<StatusEntry> = serde_json::from_slice(&bytes).unwrap();

    // Print table
    let name_width = json.iter().map(|e| e.name.len()).max().unwrap_or(4).max(4);
    let backend_width = json
        .iter()
        .map(|e| e.backend.len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<name_width$}  {:>6}  {:<backend_width$}  {:<12}  {:>10}  {:>7}",
        "NAME", "PORT", "BACKEND", "STATUS", "IDLE", "TIMEOUT"
    );
    println!(
        "{:<name_width$}  {:>6}  {:<backend_width$}  {:<12}  {:>10}  {:>7}",
        "─".repeat(name_width),
        "──────",
        "─".repeat(backend_width),
        "────────────",
        "──────────",
        "───────"
    );

    for entry in &json {
        let (indicator, idle_str) = match entry.state.as_str() {
            "running" => ("● running", format_duration(entry.idle_seconds)),
            "starting" => ("▶ starting", "-".to_string()),
            "stopping" => ("◼ stopping", "-".to_string()),
            _ => ("○ stopped", "-".to_string()),
        };
        println!(
            "{:<name_width$}  {:>6}  {:<backend_width$}  {:<12}  {:>10}  {:>6}s",
            entry.name,
            entry.port,
            entry.backend,
            indicator,
            idle_str,
            entry.idle_timeout
        );
    }
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[derive(serde::Deserialize)]
struct StatusEntry {
    name: String,
    port: u16,
    backend: String,
    state: String,
    idle_seconds: u64,
    idle_timeout: u64,
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
