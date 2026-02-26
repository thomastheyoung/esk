use esk::mcp::EskMcpServer;
use rmcp::ServiceExt;
use tokio::io::{stdin, stdout};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let server = EskMcpServer::new().serve((stdin(), stdout())).await?;
    server.waiting().await?;
    Ok(())
}
