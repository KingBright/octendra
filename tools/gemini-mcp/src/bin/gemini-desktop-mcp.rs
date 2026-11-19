const FIXTURE_NATIVE: bool = false;
#[path = "../artifact.rs"]
mod artifact;
#[path = "../ops.rs"]
mod ops;
#[path = "../server.rs"]
mod server;
use rmcp::ServiceExt;
use server::GeminiServer;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|x| x == "--worker") {
        if args.len() != 5 {
            anyhow::bail!("invalid worker arguments")
        }
        return ops::worker(&args[2], &args[3], &args[4]);
    }
    if args.get(1).is_some_and(|x| x == "--check-accessibility") {
        let out = ops::native("accessibility", None)?;
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if args.get(1).is_some_and(|x| x == "--prompt-accessibility") {
        let out = ops::native("prompt_accessibility", None)?;
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if args.len() != 1 {
        anyhow::bail!("unknown argument")
    }
    GeminiServer
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
