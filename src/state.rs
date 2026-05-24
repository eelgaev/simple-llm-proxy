use arc_swap::ArcSwap;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::config::{Config, ServerEntry};

pub struct GpuSet {
    pub name: String,
    pub servers: Vec<ServerEntry>,
    pub semaphore: Arc<Semaphore>,
    pub next_server: AtomicUsize,
}

pub struct AppState {
    pub api_tokens: HashSet<String>,
    pub gpu_sets: Vec<Arc<GpuSet>>,
    pub model_map: ArcSwap<HashMap<String, Vec<Arc<GpuSet>>>>,
    pub http_client: reqwest::Client,
}

impl AppState {
    pub fn from_config(config: Config) -> Self {
        let gpu_sets: Vec<Arc<GpuSet>> = config
            .servers
            .into_iter()
            .map(|(name, servers)| {
                Arc::new(GpuSet {
                    name,
                    servers,
                    semaphore: Arc::new(Semaphore::new(1)),
                    next_server: AtomicUsize::new(0),
                })
            })
            .collect();

        Self {
            api_tokens: config.api_tokens.into_iter().collect(),
            gpu_sets,
            model_map: ArcSwap::new(Arc::new(HashMap::new())),
            http_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
        }
    }

    pub async fn discover_models(&self) {
        let mut new_map: HashMap<String, Vec<Arc<GpuSet>>> = HashMap::new();

        for gpu_set in &self.gpu_sets {
            let url = format!(
                "{}/v1/models",
                gpu_set.servers[0].url().trim_end_matches('/')
            );

            let mut req = self.http_client.get(&url);
            if let Some(token) = gpu_set.servers[0].token() {
                req = req.bearer_auth(token);
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(gpu_set = %gpu_set.name, "failed to query models: {e}");
                    continue;
                }
            };

            let body: serde_json::Value = match resp.json().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(gpu_set = %gpu_set.name, "failed to parse models response: {e}");
                    continue;
                }
            };

            if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
                for model in data {
                    if let Some(id) = model.get("id").and_then(|i| i.as_str()) {
                        new_map
                            .entry(id.to_string())
                            .or_default()
                            .push(Arc::clone(gpu_set));
                    }
                }
                tracing::info!(gpu_set = %gpu_set.name, "discovered models");
            }
        }

        tracing::info!("model map refreshed: {} model(s) across {} GPU set(s)",
            new_map.len(), self.gpu_sets.len());

        self.model_map.store(Arc::new(new_map));
    }
}
