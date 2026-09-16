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
        for (name, entries) in &self.servers {
            if entries.is_empty() {
                return Err(format!("server group '{}' has no URLs", name).into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_servers_table_is_valid() {
        let config: Config = toml::from_str(
            r#"
                listen = "127.0.0.1:8080"
                api_tokens = ["secret"]

                [servers]
            "#,
        )
        .unwrap();

        assert!(config.validate().is_ok());
    }

    #[test]
    fn configured_server_group_must_not_be_empty() {
        let config: Config = toml::from_str(
            r#"
                listen = "127.0.0.1:8080"
                api_tokens = ["secret"]

                [servers]
                local = []
            "#,
        )
        .unwrap();

        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "server group 'local' has no URLs"
        );
    }
}
