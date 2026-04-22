use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use scriptorium::{
    config::Config, pb::sandbox_server::SandboxServer, runtime::DockerRuntime,
    service::SandboxService, workspace::WorkspaceManager,
};
use tonic::transport::Server;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Parser, Debug)]
#[command(name = "scriptorium", version)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, default_value = "deploy/config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("load config from {}", cli.config.display()))?;

    tracing::info!(
        listen = %cfg.server.listen,
        workspace_root = %cfg.workspace.root.display(),
        "starting scriptorium"
    );

    let workspaces = WorkspaceManager::new(cfg.workspace.clone());
    workspaces.ensure_root().await?;

    let runtime = DockerRuntime::connect(cfg.docker.clone(), cfg.sandbox.clone()).await?;

    let svc = SandboxService::new(runtime, workspaces, &cfg.concurrency);

    let addr = cfg.server.listen;
    tracing::info!(%addr, "gRPC server listening");
    Server::builder()
        .add_service(SandboxServer::new(svc))
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;

    tracing::info!("scriptorium stopped");
    Ok(())
}

fn init_tracing() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,bollard=warn"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_target(true))
        .init();
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv()  => tracing::info!("received SIGINT"),
    }
}
