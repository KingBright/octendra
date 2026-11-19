use gemini_desktop_mcp::client::GeminiClient;
use serde_json::{Value, json};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        anyhow::bail!("usage: gemini-desktop-client SERVER_BINARY ACTION [JSON_OBJECT]")
    }
    let c = GeminiClient::connect(&args[1]).await?;
    let params: Value = if args.len() == 4 {
        serde_json::from_str(&args[3])?
    } else {
        json!({})
    };
    let result = c.call(&args[2], params).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    c.close().await?;
    Ok(())
}
