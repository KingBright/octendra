//! Exact, repeatable client configuration. No credentials are created or copied.
use crate::{GatewayConfig, write_private};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum ClientProfile {
    Chatgpt,
    Gemini,
    Grok,
    GrokBot,
    Codex,
    Antigravity,
}
impl ClientProfile {
    fn values(
        self,
    ) -> (
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    ) {
        match self {
            Self::Chatgpt => (
                "chatgpt",
                &["https://chatgpt.com"],
                &["https://chatgpt.com/connector_platform_oauth_redirect"],
            ),
            Self::Gemini => ("gemini", &["https://gemini.google.com"], &[]),
            Self::Grok => (
                "grok",
                &["https://grok.com"],
                &["https://grok.com/connectors-oauth-exchange-code/"],
            ),
            Self::GrokBot => (
                "grok-bot",
                &["https://cursor.com", "https://www.cursor.com"],
                &[
                    "cursor://anysphere.cursor-mcp/oauth/callback",
                    "http://localhost:8787/callback",
                    "https://www.cursor.com/agents/mcp/oauth/callback",
                    "https://cursor.com/agents/mcp/oauth/callback",
                ],
            ),
            Self::Codex => ("codex", &[], &[]),
            Self::Antigravity => ("antigravity", &[], &[]),
        }
    }
}

pub fn configure(path: &Path, profiles: &[ClientProfile], check: bool) -> Result<Value> {
    let original = std::fs::read(path)?;
    let mut document: Value = serde_json::from_slice(&original)?;
    let mut config: GatewayConfig = serde_json::from_value(document.clone())?;
    let mut clients = Vec::new();
    let mut changed = false;
    for profile in profiles {
        let (name, origins, callbacks) = profile.values();
        let missing_origins: Vec<_> = origins
            .iter()
            .filter(|s| !config.allowed_origins.iter().any(|v| v == **s))
            .copied()
            .collect();
        let missing_callbacks: Vec<_> = callbacks
            .iter()
            .filter(|s| !config.redirect_uris.iter().any(|v| v == **s))
            .copied()
            .collect();
        let ready = missing_origins.is_empty() && missing_callbacks.is_empty();
        changed |= !ready;
        config
            .allowed_origins
            .extend(missing_origins.iter().map(|s| (*s).to_owned()));
        config
            .redirect_uris
            .extend(missing_callbacks.iter().map(|s| (*s).to_owned()));
        clients.push(json!({"client":name,"configuration":if check && !ready {"needs_configuration"} else {"ready"},
            "missing_origins":missing_origins,"missing_callbacks":missing_callbacks,
            "authorization":"not_observed","tool_call":"not_observed"}));
    }
    config.validate_oauth_policy()?;
    if changed && !check {
        document["allowed_origins"] = json!(config.allowed_origins);
        document["redirect_uris"] = json!(config.redirect_uris);
        anyhow::ensure!(
            std::fs::read(path)? == original,
            "configuration_changed; inspect before retrying"
        );
        write_private(path, &serde_json::to_vec_pretty(&document)?)?;
    }
    Ok(
        json!({"mcp_url":format!("{}/mcp",config.public_url),"changed":changed && !check,"clients":clients,
        "next_action":if check && changed {"run_configure_clients_without_check"} else {"complete_client_login_then_call_devices_list"}}),
    )
}

pub async fn probe(origin: &str) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let mut checks = Vec::new();
    for (stage, path, expected) in [
        (
            "resource_discovery",
            "/.well-known/oauth-protected-resource",
            200,
        ),
        (
            "authorization_discovery",
            "/.well-known/oauth-authorization-server",
            200,
        ),
        ("mcp_authentication_challenge", "/mcp", 401),
    ] {
        let response = client
            .get(format!("{origin}{path}"))
            .header("user-agent", "Octendra-Connection-Check")
            .send()
            .await;
        checks.push(match response {
            Ok(response) => json!({"stage":stage,"http_status":response.status().as_u16(),"passed":response.status().as_u16()==expected}),
            Err(_) => json!({"stage":stage,"passed":false,"error_code":"endpoint_unreachable"}),
        });
    }
    Ok(json!({"checks":checks,"authorization":"not_observed","tool_call":"not_observed"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_are_repeatable_keep_settings_and_cover_cursor_redirect_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.json");
        let config = json!({"public_url":"https://example.test","bind":"127.0.0.1:18787","state_dir":dir.path(),
            "owner":"fixture","password_hash":"private-hash","devices":[],"redirect_uris":[],"allowed_origins":[],"public_url_aliases":["https://alias.example.test"]});
        write_private(&path, &serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(
            configure(&path, &[ClientProfile::GrokBot], true).unwrap()["clients"][0]["configuration"],
            "needs_configuration"
        );
        assert_eq!(
            configure(&path, &[ClientProfile::GrokBot], false).unwrap()["changed"],
            true
        );
        assert_eq!(
            configure(&path, &[ClientProfile::GrokBot], false).unwrap()["changed"],
            false
        );
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["public_url_aliases"], config["public_url_aliases"]);
        let saved: GatewayConfig = serde_json::from_value(saved).unwrap();
        assert!(saved.authorization_csp().contains("https://cursor.com"));
        assert_eq!(saved.redirect_uris.len(), 4);
    }
}
