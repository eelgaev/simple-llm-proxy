use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ServerEntry {
    Url(String),
    WithToken { url: String, token: String },
}

impl ServerEntry {
    pub fn url(&self) -> &str {
        match self {
            ServerEntry::Url(u) => u,
            ServerEntry::WithToken { url, .. } => url,
        }
    }

    pub fn token(&self) -> Option<&str> {
        match self {
            ServerEntry::Url(_) => None,
            ServerEntry::WithToken { token, .. } => Some(token),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub listen: String,
    pub api_tokens: Vec<String>,
    pub servers: HashMap<String, Vec<ServerEntry>>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.api_tokens.is_empty() {
            return Err("at least one api_token is required".into());
        }
        if self.servers.is_empty() {
            return Err("at least one server group is required".into());
        }
        for (name, entries) in &self.servers {
            if entries.is_empty() {
                return Err(format!("server group '{}' has no URLs", name).into());
            }
        }
        Ok(())
    }
}
