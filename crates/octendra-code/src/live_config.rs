//! Reload device/client configuration without replacing queues, rate limits or listeners.
use crate::GatewayConfig;
use anyhow::{Result, ensure};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub(crate) fn validate(config: &GatewayConfig) -> Result<()> {
    config.validate_oauth_policy()?;
    ensure!(
        !config.owner.is_empty() && !config.password_hash.is_empty(),
        "owner and password required"
    );
    let mut ids = std::collections::HashSet::new();
    for device in &config.devices {
        uuid::Uuid::parse_str(&device.id)?;
        ensure!(
            ids.insert(&device.id) && device.token_hash.len() == 64,
            "invalid or duplicate device identity"
        );
        ensure!(
            device
                .scopes
                .iter()
                .all(|s| crate::SUPPORTED_SCOPES.split_whitespace().any(|x| x == s)),
            "invalid device scope"
        );
    }
    if let Some(browser) = &config.forge_browser {
        browser.validate()?;
    }
    Ok(())
}

pub(crate) struct LiveConfig {
    path: PathBuf,
    state: Mutex<(String, Arc<GatewayConfig>)>,
}
impl LiveConfig {
    pub fn new(path: PathBuf, config: Arc<GatewayConfig>) -> Self {
        Self {
            path,
            state: Mutex::new((String::new(), config)),
        }
    }
    pub fn snapshot(&self) -> Arc<GatewayConfig> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Ok(bytes) = std::fs::read(&self.path) else {
            return state.1.clone();
        };
        let digest = crate::hash(&bytes);
        if digest == state.0 {
            return state.1.clone();
        }
        state.0 = digest;
        let candidate = (|| -> Result<GatewayConfig> {
            let config: GatewayConfig = serde_json::from_slice(&bytes)?;
            validate(&config)?;
            let old = serde_json::to_value(&*state.1)?;
            let new = serde_json::to_value(&config)?;
            for field in [
                "bind",
                "public_url",
                "public_url_aliases",
                "owner",
                "state_dir",
                "forge_browser",
            ] {
                ensure!(
                    old.get(field) == new.get(field),
                    "listener or storage configuration requires restart"
                );
            }
            Ok(config)
        })();
        match candidate {
            Ok(config) => {
                state.1 = Arc::new(config);
                tracing::info!("device and client configuration reloaded");
            }
            Err(_) => tracing::warn!(
                "configuration reload rejected; retaining last valid configuration; run check-gateway-config"
            ),
        }
        state.1.clone()
    }
}
