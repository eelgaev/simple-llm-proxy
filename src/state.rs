use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tokio::sync::{Mutex, Semaphore};

use crate::config::{Config, ServerEntry};

pub struct GpuSet {
    pub name: String,
    pub servers: Vec<ServerEntry>,
    pub models_path: String,
    pub semaphore: Arc<Semaphore>,
    pub next_server: AtomicUsize,
    /// Dynamically registered nodes are commonly addressed by IP and use a
    /// locally issued/self-signed certificate.
    pub trust_invalid_tls: bool,
    /// Prefix applied to model ids exposed by dynamically registered hosts.
    /// The backend still receives its original, unprefixed id.
    pub model_prefix: Option<String>,
}

pub struct ModelEntry {
    pub gpu_sets: Vec<Arc<GpuSet>>,
    /// The `data[]` object exactly as the backend reported it, so backend-specific
    /// fields (llama.cpp's `meta`, vLLM/sglang's `max_model_len`, `root`, ...)
    /// survive the trip through the proxy.
    pub info: serde_json::Value,
    /// The matching entry from llama.cpp's second top-level block, `models[]`
    /// (the Ollama-style listing). `None` for backends that don't emit one.
    pub listing: Option<serde_json::Value>,
    /// Model id understood by the selected backend.
    pub backend_id: String,
}

pub struct AppState {
    pub api_tokens: HashSet<String>,
    pub gpu_sets: ArcSwap<Vec<Arc<GpuSet>>>,
    pub model_map: ArcSwap<HashMap<String, ModelEntry>>,
    pub http_client: reqwest::Client,
    pub registered_host_client: reqwest::Client,
    pub discovered_hosts_path: PathBuf,
    registration_lock: Mutex<()>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiscoveredHost {
    pub host: IpAddr,
    pub port: u16,
    pub api_key: String,
}

impl AppState {
    pub fn from_config(config: Config, discovered_hosts_path: PathBuf) -> Self {
        let gpu_sets: Vec<Arc<GpuSet>> = config
            .servers
            .into_iter()
            .map(|(name, servers)| {
                Arc::new(GpuSet {
                    name,
                    servers,
                    models_path: "/v1/models".into(),
                    semaphore: Arc::new(Semaphore::new(1)),
                    next_server: AtomicUsize::new(0),
                    trust_invalid_tls: false,
                    model_prefix: None,
                })
            })
            .collect();

        Self {
            api_tokens: config.api_tokens.into_iter().collect(),
            gpu_sets: ArcSwap::new(Arc::new(gpu_sets)),
            model_map: ArcSwap::new(Arc::new(HashMap::new())),
            http_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            registered_host_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .danger_accept_invalid_certs(true)
                .build()
                .unwrap(),
            discovered_hosts_path,
            registration_lock: Mutex::new(()),
        }
    }

    pub async fn discover_models(&self) {
        let mut new_map: HashMap<String, ModelEntry> = HashMap::new();

        let gpu_sets = self.gpu_sets.load_full();
        for gpu_set in gpu_sets.iter() {
            let url = format!(
                "{}{}",
                gpu_set.servers[0].url().trim_end_matches('/'),
                gpu_set.models_path,
            );

            let mut req = self.client_for(gpu_set).get(&url);
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

            let listings = body.get("models").and_then(|m| m.as_array());

            let data = body
                .get("data")
                .and_then(|d| d.as_array())
                .map(Vec::as_slice);
            let synthesized;
            let data = if data.is_some() {
                data
            } else {
                synthesized = listings.map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            let id = item
                                .get("id")
                                .or_else(|| item.get("model"))
                                .and_then(|value| value.as_str())?;
                            Some(serde_json::json!({ "id": id, "object": "model" }))
                        })
                        .collect::<Vec<_>>()
                });
                synthesized.as_deref()
            };

            if let Some(data) = data {
                for model in data {
                    if let Some(id) = model.get("id").and_then(|i| i.as_str()) {
                        let exposed_id = gpu_set
                            .model_prefix
                            .as_ref()
                            .map(|prefix| format!("{prefix}/{id}"))
                            .unwrap_or_else(|| id.to_string());
                        let mut info = model.clone();
                        info["id"] = serde_json::Value::String(exposed_id.clone());
                        let mut listing = listings.and_then(|l| {
                            l.iter()
                                .find(|m| m.get("model").and_then(|n| n.as_str()) == Some(id))
                                .cloned()
                        });
                        if let Some(listing) = listing.as_mut() {
                            listing["model"] = serde_json::Value::String(exposed_id.clone());
                        }
                        new_map
                            .entry(exposed_id)
                            .or_insert_with(|| ModelEntry {
                                gpu_sets: Vec::new(),
                                info,
                                listing,
                                backend_id: id.to_string(),
                            })
                            .gpu_sets
                            .push(Arc::clone(gpu_set));
                    }
                }
                tracing::info!(gpu_set = %gpu_set.name, "discovered models");
            }
        }

        tracing::info!(
            "model map refreshed: {} model(s) across {} GPU set(s)",
            new_map.len(),
            gpu_sets.len()
        );

        self.model_map.store(Arc::new(new_map));
    }

    pub async fn add_discovered_host(
        &self,
        host: DiscoveredHost,
        base_url: String,
    ) -> Result<(), std::io::Error> {
        let _guard = self.registration_lock.lock().await;
        let mut hosts = self.read_discovered_hosts().await?;
        hosts.retain(|entry| entry.host != host.host || entry.port != host.port);
        hosts.push(host.clone());

        let json = serde_json::to_vec_pretty(&hosts)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let temporary_path = self.discovered_hosts_path.with_extension("json.tmp");
        tokio::fs::write(&temporary_path, json).await?;
        tokio::fs::rename(&temporary_path, &self.discovered_hosts_path).await?;

        let name = discovered_group_name(host.host, host.port);
        let mut gpu_sets = self.gpu_sets.load_full().as_ref().clone();
        gpu_sets.retain(|gpu_set| gpu_set.name != name);
        gpu_sets.push(Arc::new(GpuSet {
            name,
            servers: vec![ServerEntry::WithToken {
                url: base_url,
                token: host.api_key,
            }],
            models_path: "/models".into(),
            semaphore: Arc::new(Semaphore::new(1)),
            next_server: AtomicUsize::new(0),
            trust_invalid_tls: true,
            model_prefix: Some(host.host.to_string()),
        }));
        self.gpu_sets.store(Arc::new(gpu_sets));
        Ok(())
    }

    pub async fn read_discovered_hosts(&self) -> Result<Vec<DiscoveredHost>, std::io::Error> {
        match tokio::fs::read(&self.discovered_hosts_path).await {
            Ok(contents) => serde_json::from_slice(&contents)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn client_for(&self, gpu_set: &GpuSet) -> &reqwest::Client {
        if gpu_set.trust_invalid_tls {
            &self.registered_host_client
        } else {
            &self.http_client
        }
    }
}

fn discovered_group_name(host: IpAddr, port: u16) -> String {
    format!("discovered-{host}-{port}")
}
