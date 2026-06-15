//! herdr-simple-mcp — a thin, stateless MCP stdio server that attaches directly to
//! herdr's Unix-socket API. herdr's daemon is the authority; this binary just maps MCP
//! tool calls onto socket requests.

use anyhow::Result;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

mod config;
mod herdr;
mod manifest;
mod server;

#[tokio::main]
async fn main() -> Result<()> {
    // stdout is reserved for MCP JSON-RPC framing — all logs go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("herdr_simple_mcp=info".parse()?),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("starting herdr-simple-mcp");

    warn_on_version_skew().await;

    let service = server::HerdrMcpServer::new().serve(stdio()).await?;

    tracing::info!("herdr-simple-mcp initialized, waiting for requests");

    service.waiting().await?;

    tracing::info!("herdr-simple-mcp stopped");
    Ok(())
}

/// Best-effort startup check: warn if the running herdr's minor version differs from the
/// contract's target, since the strict tool schemas could then be stale. Non-fatal —
/// herdr may not be running yet when the host launches us.
async fn warn_on_version_skew() {
    match herdr::request("ping", serde_json::json!({})).await {
        Ok(pong) => {
            let daemon = pong.get("version").and_then(|v| v.as_str()).unwrap_or("");
            let target = manifest::herdr_version();
            if !daemon.is_empty() && minor(daemon) != minor(&target) {
                tracing::warn!(
                    "herdr daemon version {daemon} differs from contract target {target}; \
                     tool schemas may be stale (run `cargo test -- --ignored` for the drift check)"
                );
            }
        }
        Err(e) => tracing::warn!("could not verify herdr version (is it running?): {e}"),
    }
}

fn minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}
