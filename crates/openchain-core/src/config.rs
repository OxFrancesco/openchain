use eyre::{eyre, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub clickhouse: ClickHouseConfig,
    #[serde(default)]
    pub chains: BTreeMap<String, ChainConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClickHouseConfig {
    pub url: String,
    pub database: String,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    pub name: String,
    pub rpc: String,
    /// Optional WebSocket endpoint for head subscriptions in follow mode.
    /// Defaults to the rpc URL with the scheme swapped (https->wss, http->ws).
    #[serde(default)]
    pub ws: Option<String>,
}

fn default_user() -> String {
    "default".into()
}

impl ChainConfig {
    /// WebSocket endpoint for follow-mode head subscriptions: the explicit
    /// `ws` field if set, otherwise the rpc URL with the scheme swapped.
    pub fn ws_url(&self) -> String {
        if let Some(ws) = &self.ws {
            return ws.clone();
        }
        if let Some(rest) = self.rpc.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.rpc.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.rpc.clone()
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("cannot read config at {}", path.display()))?;
        toml::from_str(&raw).wrap_err("invalid config file")
    }

    pub fn chain(&self, chain_id: u64) -> Result<&ChainConfig> {
        self.chains
            .get(&chain_id.to_string())
            .ok_or_else(|| eyre!("chain {chain_id} not found in config; add a [chains.{chain_id}] section"))
    }

    pub fn default_template() -> Self {
        let mut chains = BTreeMap::new();
        chains.insert(
            "1".to_string(),
            ChainConfig { name: "ethereum".into(), rpc: "https://ethereum-rpc.publicnode.com".into(), ws: None },
        );
        Config {
            clickhouse: ClickHouseConfig {
                url: "http://localhost:8123".into(),
                database: "openchain".into(),
                user: "default".into(),
                password: String::new(),
            },
            chains,
        }
    }
}
