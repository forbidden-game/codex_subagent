use clap::Parser;
use tracing_subscriber::EnvFilter;

mod agent;
mod app_server;
mod mcp;
mod prompts;
mod subagent;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// Path to the codex binary (default: codex)
    #[arg(long)]
    codex_bin: Option<String>,
    /// Max parallel subagents (default: 10)
    #[arg(long, default_value_t = 10)]
    max_workers: usize,
    /// Log level (e.g. info, debug)
    #[arg(long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let log_level = cli
        .log_level
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(log_level))
        .with_target(false)
        .init();

    let codex_bin = cli
        .codex_bin
        .or_else(|| std::env::var("CODEX_BIN").ok())
        .unwrap_or_else(|| "codex".to_string());
    let max_workers = cli.max_workers;

    let executor = subagent::SubagentExecutor::new(codex_bin, max_workers);
    let server = mcp::McpServer::new(executor);
    server.run().await?;
    Ok(())
}
