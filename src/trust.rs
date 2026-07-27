use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Information about a client waiting for server authorization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingClientInfo {
    pub client_id: String,
    pub hostname: String,
    pub device_name: String,
    pub transport: String,
    pub first_seen: String,
}

/// Information about an approved, trusted client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovedClientInfo {
    pub client_id: String,
    pub hostname: String,
    pub device_name: String,
    pub approved_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TrustData {
    pub approved: HashMap<String, ApprovedClientInfo>,
    pub pending: HashMap<String, PendingClientInfo>,
}

/// Thread-safe security manager for tracking approved and pending clients.
#[derive(Clone)]
pub struct TrustManager {
    store_path: PathBuf,
    data: Arc<Mutex<TrustData>>,
}

impl TrustManager {
    /// Load or create the TrustManager storing trusted clients at `store_path`.
    pub fn new(store_path: Option<PathBuf>) -> Result<Self> {
        let path = if let Some(p) = store_path {
            p
        } else {
            let base_dir = if let Ok(sudo_user) = std::env::var("SUDO_USER")
                && !sudo_user.is_empty()
                && sudo_user != "root"
            {
                PathBuf::from(format!("/home/{}/.config/joycast", sudo_user))
            } else if std::env::var("USER").unwrap_or_default() == "root"
                || std::env::var("SYSTEMD_EXEC_PID").is_ok()
            {
                PathBuf::from("/etc/joycast")
            } else if let Some(cfg) = dirs_next::config_dir() {
                cfg.join("joycast")
            } else {
                PathBuf::from("/etc/joycast")
            };

            fs::create_dir_all(&base_dir).ok();
            base_dir.join("trusted_clients.json")
        };

        let data = if path.exists() {
            match fs::read_to_string(&path) {
                Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
                Err(e) => {
                    warn!("Failed to read trust store at {}: {}", path.display(), e);
                    TrustData::default()
                }
            }
        } else {
            TrustData::default()
        };

        Ok(Self {
            store_path: path,
            data: Arc::new(Mutex::new(data)),
        })
    }

    /// Save state to disk.
    fn save(&self, data: &TrustData) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(data).context("Failed to serialize trust data")?;
        fs::write(&self.store_path, json).with_context(|| {
            format!(
                "Failed to write trust data to {}",
                self.store_path.display()
            )
        })?;
        Ok(())
    }

    /// Check if a client ID is approved.
    pub fn is_approved(&self, client_id: &str) -> bool {
        let lock = self.data.lock().unwrap();
        lock.approved.contains_key(client_id)
    }

    /// Register a pending connection request.
    pub fn register_pending(
        &self,
        client_id: String,
        hostname: String,
        device_name: String,
        transport: String,
    ) {
        let mut lock = self.data.lock().unwrap();
        if lock.approved.contains_key(&client_id) {
            return;
        }

        let first_seen = Utc::now().to_rfc3339();
        let pending = PendingClientInfo {
            client_id: client_id.clone(),
            hostname,
            device_name,
            transport,
            first_seen,
        };

        lock.pending.insert(client_id, pending);
        let _ = self.save(&lock);
    }

    /// Approve a pending client by client_id or prefix.
    pub fn approve(&self, target_id: &str) -> Result<ApprovedClientInfo> {
        let mut lock = self.data.lock().unwrap();

        // Match exact or prefix
        let matched_id = if lock.pending.contains_key(target_id) {
            Some(target_id.to_string())
        } else {
            lock.pending
                .keys()
                .find(|id| id.starts_with(target_id))
                .cloned()
        };

        let id = match matched_id {
            Some(i) => i,
            None => bail!(
                "No pending client request found matching ID or prefix '{}'",
                target_id
            ),
        };

        let pending = lock.pending.remove(&id).unwrap();
        let approved = ApprovedClientInfo {
            client_id: pending.client_id.clone(),
            hostname: pending.hostname,
            device_name: pending.device_name,
            approved_at: Utc::now().to_rfc3339(),
        };

        lock.approved.insert(id.clone(), approved.clone());
        self.save(&lock)?;
        info!(
            "Approved client identity '{}' ({})",
            approved.client_id, approved.hostname
        );

        Ok(approved)
    }

    /// Revoke / un-trust an approved client.
    pub fn revoke(&self, target_id: &str) -> Result<bool> {
        let mut lock = self.data.lock().unwrap();

        let matched_id = if lock.approved.contains_key(target_id) {
            Some(target_id.to_string())
        } else {
            lock.approved
                .keys()
                .find(|id| id.starts_with(target_id))
                .cloned()
        };

        if let Some(id) = matched_id {
            lock.approved.remove(&id);
            self.save(&lock)?;
            info!("Revoked trust for client ID: {}", id);
            Ok(true)
        } else {
            bail!(
                "No approved client found matching ID or prefix '{}'",
                target_id
            )
        }
    }

    /// List all clients waiting for authorization.
    pub fn list_pending(&self) -> Vec<PendingClientInfo> {
        let lock = self.data.lock().unwrap();
        let mut list: Vec<PendingClientInfo> = lock.pending.values().cloned().collect();
        list.sort_by(|a, b| a.first_seen.cmp(&b.first_seen));
        list
    }

    /// List all approved clients.
    pub fn list_approved(&self) -> Vec<ApprovedClientInfo> {
        let lock = self.data.lock().unwrap();
        let mut list: Vec<ApprovedClientInfo> = lock.approved.values().cloned().collect();
        list.sort_by(|a, b| a.approved_at.cmp(&b.approved_at));
        list
    }
}
