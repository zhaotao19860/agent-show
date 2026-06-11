use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, sync::Arc};

#[derive(Parser)]
#[command(name = "agent-show", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Serve {
        #[arg(long, default_value = "127.0.0.1:7777")]
        bind: String,
        #[arg(long, default_value_t = false)]
        no_open: bool,
        #[arg(long, default_value_t = false)]
        unsafe_public: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            bind,
            no_open,
            unsafe_public,
        } => {
            let addr = validate_bind_addr(&bind, unsafe_public)?;
            let mut adapters: Vec<Arc<dyn pawscope_core::AgentAdapter>> = Vec::new();
            match pawscope_copilot::CopilotAdapter::new() {
                Ok(a) => adapters.push(Arc::new(a)),
                Err(e) => tracing::warn!("copilot adapter disabled: {e}"),
            }
            match pawscope_claude::ClaudeAdapter::new() {
                Ok(a) => adapters.push(Arc::new(a)),
                Err(e) => tracing::warn!("claude adapter disabled: {e}"),
            }
            match pawscope_codex::CodexAdapter::new() {
                Ok(a) => adapters.push(Arc::new(a)),
                Err(e) => tracing::warn!("codex adapter disabled: {e}"),
            }
            match pawscope_opencode::OpenCodeAdapter::new() {
                Ok(a) => adapters.push(Arc::new(a)),
                Err(e) => tracing::warn!("opencode adapter disabled: {e}"),
            }
            match pawscope_gemini::GeminiAdapter::new() {
                Ok(a) => adapters.push(Arc::new(a)),
                Err(e) => tracing::warn!("gemini adapter disabled: {e}"),
            }
            match pawscope_aider::AiderAdapter::new() {
                Ok(a) => adapters.push(Arc::new(a)),
                Err(e) => tracing::warn!("aider adapter disabled: {e}"),
            }
            match pawscope_comate::ComateAdapter::new() {
                Ok(a) => adapters.push(Arc::new(a)),
                Err(e) => tracing::warn!("comate adapter disabled: {e}"),
            }
            tracing::info!("active adapters: {}", adapters.len());
            let adapter: Arc<dyn pawscope_core::AgentAdapter> =
                Arc::new(pawscope_server::MultiAdapter::new(adapters));
            let (router, state) = pawscope_server::build_app(adapter);
            pawscope_server::spawn_watcher(state);
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tracing::info!("listening on http://{addr}");
            if !no_open {
                let _ = open::that(format!("http://{addr}"));
            }
            axum::serve(listener, router).await?;
        }
    }
    Ok(())
}

fn validate_bind_addr(bind: &str, unsafe_public: bool) -> Result<SocketAddr> {
    let addr: SocketAddr = bind.parse()?;
    if !unsafe_public && !addr.ip().is_loopback() {
        bail!(
            "refusing to bind {addr}: Agent Show exposes local files and mutating APIs; use --unsafe-public to bind a non-loopback address"
        );
    }
    Ok(addr)
}
