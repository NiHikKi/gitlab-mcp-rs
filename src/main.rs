//! GitLab MCP server — a Rust reimplementation of the Node reference server.

mod config;
mod exec;
mod gitlab;
mod registry;
mod server;

use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // stdout carries the MCP stream, so every log line goes to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GITLAB_MCP_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = config::parse_cli(std::env::args().skip(1));
    let cfg = config::Config::from_env_and_cli(cli)?;
    let registry = Arc::new(registry::Registry::load(&cfg).context("loading the tool registry")?);
    let exec = Arc::new(exec::Executor::new(&cfg, registry.clone())?);

    tracing::info!(
        api_url = %cfg.api_url,
        tools = registry.visible().len(),
        read_only = cfg.read_only,
        "gitlab-mcp starting"
    );

    let service = server::GitLabMcp::new(registry, exec)
        .serve(stdio())
        .await
        .context("starting the MCP stdio transport")?;

    service.waiting().await?;
    Ok(())
}
