use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Record of a previously connected server that accepted our connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnownServerInfo {
    pub server_hostname: String,
    pub target: String,
    pub transport_type: String,
    pub last_connected: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HistoryData {
    pub servers: Vec<KnownServerInfo>,
}

/// Store for managing client's known server history.
pub struct HistoryStore {
    store_path: PathBuf,
    data: HistoryData,
}

impl HistoryStore {
    /// Load or create HistoryStore.
    pub fn new() -> Result<Self> {
        let base_dir = dirs_next::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("joycast");
        fs::create_dir_all(&base_dir).ok();
        let store_path = base_dir.join("known_servers.json");

        let data = if store_path.exists() {
            match fs::read_to_string(&store_path) {
                Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
                Err(_) => HistoryData::default(),
            }
        } else {
            HistoryData::default()
        };

        Ok(Self { store_path, data })
    }

    /// Save state to disk.
    fn save(&self) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let json =
            serde_json::to_string_pretty(&self.data).context("Failed to serialize history data")?;
        fs::write(&self.store_path, json).context("Failed to write history file")?;
        Ok(())
    }

    /// Add or update a server entry upon successful handshake.
    pub fn record_connection(
        &mut self,
        server_hostname: String,
        target: String,
        transport_type: String,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();

        // Remove existing matching entry by target or hostname
        self.data
            .servers
            .retain(|s| s.target != target && s.server_hostname != server_hostname);

        self.data.servers.insert(
            0,
            KnownServerInfo {
                server_hostname,
                target,
                transport_type,
                last_connected: now,
            },
        );

        self.save()
    }

    /// List all known servers.
    pub fn list_servers(&self) -> &[KnownServerInfo] {
        &self.data.servers
    }

    /// Get a server by 1-based index.
    pub fn get_by_index(&self, index: usize) -> Option<KnownServerInfo> {
        if index == 0 || index > self.data.servers.len() {
            None
        } else {
            Some(self.data.servers[index - 1].clone())
        }
    }
}
