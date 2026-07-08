mod config;
mod lsp;
mod mcp;
mod text;

use anyhow::Result;
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use std::path::PathBuf;
use std::sync::Arc;

/// A config-driven MCP server that bridges any LSP language server to MCP clients.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Workspace directory to activate on startup (repeatable).
    #[arg(long)]
    workspace: Vec<PathBuf>,
    /// Path to a config TOML overriding the built-in language-server registry.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // MCP runs over stdout; all logs must go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let config = config::Config::load_or_builtin(cli.config.as_deref());
    let manager = Arc::new(lsp::manager::Manager::new(config));

    for ws in &cli.workspace {
        match manager.activate_workspace(ws.clone()).await {
            Ok(p) => tracing::info!("activated workspace {}", p.display()),
            Err(e) => tracing::warn!("could not activate {ws:?}: {e:#}"),
        }
    }

    let server = mcp::server::LsprantoServer::new(manager);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
