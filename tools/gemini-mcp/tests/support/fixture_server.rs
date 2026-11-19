const FIXTURE_NATIVE: bool = true;
#[path = "../../src/artifact.rs"]
mod artifact;
#[path = "../../src/ops.rs"]
mod ops;
#[path = "../../src/server.rs"]
mod server;
use rmcp::ServiceExt;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<_> = std::env::args().collect();
    if a.get(1).is_some_and(|x| x == "--worker") {
        if a.len() != 5 {
            anyhow::bail!("invalid worker arguments")
        }
        return ops::worker(&a[2], &a[3], &a[4]);
    }
    server::GeminiServer
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
