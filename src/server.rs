use anyhow::{Context, Result, bail};
use iroh::endpoint::presets::N0;
use iroh::{Endpoint, PublicKey, SecretKey};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::device::VirtualOutput;
use crate::protocol::{ALPN, AckStatus, Message};
use crate::transport::DEFAULT_PORT;
use crate::trust::TrustManager;

pub struct ServerConfig {
    pub bind_addr: Option<SocketAddr>,
    pub secret_key: Option<String>,
    pub key_file: Option<PathBuf>,
    pub disable_udp: bool,
    pub disable_iroh: bool,
    pub trust_manager: Option<TrustManager>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: Some(SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT))),
            secret_key: None,
            key_file: None,
            disable_udp: false,
            disable_iroh: false,
            trust_manager: None,
        }
    }
}

pub struct JoycastServer {
    udp_socket: Option<Arc<UdpSocket>>,
    iroh_endpoint: Option<Endpoint>,
    trust_manager: TrustManager,
    active_sessions: Arc<Mutex<HashMap<String, VirtualOutput>>>,
}

impl JoycastServer {
    /// Helper to parse a hex string into a SecretKey.
    fn parse_secret_key_hex(hex_str: &str) -> Result<SecretKey> {
        let bytes = hex::decode(hex_str.trim()).context("Invalid hex string for secret key")?;
        if bytes.len() != 32 {
            bail!(
                "Secret key must be 32 bytes (64 hex characters), got {} bytes",
                bytes.len()
            );
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(SecretKey::from(arr))
    }

    /// Helper to load or generate a persistent SecretKey so the Node ID remains static across restarts.
    fn load_or_create_secret_key(config: &ServerConfig) -> Result<SecretKey> {
        if let Some(ref key_str) = config.secret_key {
            return Self::parse_secret_key_hex(key_str);
        }

        if let Some(ref path) = config.key_file {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read key file at {}", path.display()))?;
            return Self::parse_secret_key_hex(&content);
        }

        let mut candidates = Vec::new();
        let etc_path = PathBuf::from("/etc/joycast/server.key");
        candidates.push(etc_path.clone());

        if let Ok(sudo_user) = std::env::var("SUDO_USER")
            && !sudo_user.is_empty()
            && sudo_user != "root"
        {
            candidates.push(PathBuf::from(format!(
                "/home/{}/.config/joycast/server.key",
                sudo_user
            )));
        }

        if let Some(user_config) = dirs_next::config_dir() {
            candidates.push(user_config.join("joycast").join("server.key"));
        }

        for path in &candidates {
            if path.exists()
                && let Ok(content) = fs::read_to_string(path)
                && let Ok(key) = Self::parse_secret_key_hex(&content)
            {
                info!("Loaded static server key from {}", path.display());
                return Ok(key);
            }
        }

        let target_path = if etc_path.exists() || TrustManager::is_root_context() {
            etc_path
        } else if let Some(user_config) = dirs_next::config_dir() {
            user_config.join("joycast").join("server.key")
        } else {
            PathBuf::from("/etc/joycast/server.key")
        };

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).ok();
        }

        let key = SecretKey::generate();
        let hex_key = hex::encode(key.to_bytes());
        if let Err(e) = fs::write(&target_path, &hex_key) {
            warn!(
                "Could not save persistent key file to {}: {}",
                target_path.display(),
                e
            );
        } else {
            info!(
                "Generated and saved new static server key to {}",
                target_path.display()
            );
        }
        Ok(key)
    }

    /// Bind and initialize the Joycast server over Direct UDP and/or Iroh.
    pub async fn bind(config: ServerConfig) -> Result<Self> {
        let trust_manager = match config.trust_manager.clone() {
            Some(tm) => tm,
            None => TrustManager::new(None)?,
        };

        let udp_socket = if !config.disable_udp {
            let addr = config
                .bind_addr
                .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)));
            let socket = UdpSocket::bind(addr)
                .await
                .with_context(|| format!("Failed to bind Direct UDP socket at {}", addr))?;
            Some(Arc::new(socket))
        } else {
            None
        };

        let iroh_endpoint = if !config.disable_iroh {
            let secret_key = Self::load_or_create_secret_key(&config)?;

            let endpoint = Endpoint::builder(N0)
                .secret_key(secret_key)
                .alpns(vec![ALPN.to_vec()])
                .bind()
                .await
                .context("Failed to bind iroh endpoint")?;
            Some(endpoint)
        } else {
            None
        };

        info!("============================================================");
        info!("Joycast Server is up and listening!");
        if let Some(ref socket) = udp_socket
            && let Ok(local_addr) = socket.local_addr()
        {
            info!("  Direct UDP Socket : {}", local_addr);
            info!(
                "  Connect via IP    : joycast client <SERVER_IP>:{}",
                local_addr.port()
            );
        }
        if let Some(ref ep) = iroh_endpoint {
            let node_id = ep.secret_key().public();
            info!("  Iroh Node ID      : {}  (STATIC)", node_id);
            info!("  Connect via Iroh  : joycast client {}", node_id);
        }
        info!("============================================================");

        Ok(Self {
            udp_socket,
            iroh_endpoint,
            trust_manager,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Return the server's Iroh Node ID (if Iroh is enabled).
    pub fn iroh_node_id(&self) -> Option<PublicKey> {
        self.iroh_endpoint
            .as_ref()
            .map(|ep| ep.secret_key().public())
    }

    /// Return reference to TrustManager.
    pub fn trust_manager(&self) -> &TrustManager {
        &self.trust_manager
    }

    /// Run the server, listening concurrently on Direct UDP and Iroh P2P.
    pub async fn run(self) -> Result<()> {
        let mut handles = Vec::new();
        let tm = self.trust_manager.clone();
        let active_sessions = Arc::clone(&self.active_sessions);

        // 1. Direct UDP Listener
        if let Some(socket) = self.udp_socket {
            let tm_clone = tm.clone();
            handles.push(tokio::spawn(async move {
                if let Err(e) = Self::run_udp_server(socket, tm_clone).await {
                    error!("Direct UDP server loop error: {}", e);
                }
            }));
        }

        // 2. Iroh P2P Listener
        if let Some(endpoint) = self.iroh_endpoint {
            let tm_clone = tm.clone();
            let sessions_clone = Arc::clone(&active_sessions);
            handles.push(tokio::spawn(async move {
                if let Err(e) = Self::run_iroh_server(endpoint, tm_clone, sessions_clone).await {
                    error!("Iroh server loop error: {}", e);
                }
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Helper to get current host name.
    fn server_hostname() -> String {
        gethostname::gethostname().to_string_lossy().into_owned()
    }

    /// Direct UDP packet handler loop.
    async fn run_udp_server(socket: Arc<UdpSocket>, tm: TrustManager) -> Result<()> {
        struct UdpClientSession {
            client_id: String,
            vdev: VirtualOutput,
            last_seen: Instant,
        }

        let clients: Arc<Mutex<HashMap<SocketAddr, UdpClientSession>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut buf = vec![0u8; 65535];

        let clients_reaper = Arc::clone(&clients);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let mut lock = clients_reaper.lock().await;
                let now = Instant::now();
                lock.retain(|addr, session| {
                    let active = now.duration_since(session.last_seen) < Duration::from_secs(10);
                    if !active {
                        info!(client_addr = %addr, client_id = %session.client_id, "UDP client inactive timeout, removing virtual device");
                    }
                    active
                });
            }
        });

        loop {
            let (len, src_addr) = match socket.recv_from(&mut buf).await {
                Ok(res) => res,
                Err(e) => {
                    warn!("UDP recv error: {}", e);
                    continue;
                }
            };

            let payload = &buf[..len];
            let msg = match Message::decode(payload) {
                Ok(m) => m,
                Err(e) => {
                    warn!(client_addr = %src_addr, error = %e, "Invalid UDP message received");
                    continue;
                }
            };

            let clients_clone = Arc::clone(&clients);
            let socket_clone = Arc::clone(&socket);

            match msg {
                Message::Handshake(payload) => {
                    info!(
                        client_addr = %src_addr,
                        client_id = %payload.client_id,
                        hostname = %payload.client_hostname,
                        device = %payload.metadata.name,
                        "Direct UDP Handshake received"
                    );

                    if tm.is_approved(&payload.client_id) {
                        match VirtualOutput::new(&payload.metadata) {
                            Ok(vdev) => {
                                clients_clone.lock().await.insert(
                                    src_addr,
                                    UdpClientSession {
                                        client_id: payload.client_id,
                                        vdev,
                                        last_seen: Instant::now(),
                                    },
                                );
                                let ack = Message::HandshakeAck {
                                    status: AckStatus::Approved,
                                    server_hostname: Self::server_hostname(),
                                    message: "Connection authorized".into(),
                                };
                                if let Ok(ack_bytes) = ack.encode() {
                                    let _ = socket_clone.send_to(&ack_bytes, src_addr).await;
                                }
                            }
                            Err(e) => {
                                let ack = Message::HandshakeAck {
                                    status: AckStatus::Rejected,
                                    server_hostname: Self::server_hostname(),
                                    message: format!("Failed to create virtual device: {}", e),
                                };
                                if let Ok(ack_bytes) = ack.encode() {
                                    let _ = socket_clone.send_to(&ack_bytes, src_addr).await;
                                }
                            }
                        }
                    } else {
                        info!(client_id = %payload.client_id, "Client connection requires approval");
                        tm.register_pending(
                            payload.client_id.clone(),
                            payload.client_hostname,
                            payload.metadata.name,
                            "Direct UDP".into(),
                        );
                        let ack = Message::HandshakeAck {
                            status: AckStatus::PendingApproval,
                            server_hostname: Self::server_hostname(),
                            message: format!(
                                "Connection pending authorization on server. Run 'joycast server approve {}' to authorize.",
                                payload.client_id
                            ),
                        };
                        if let Ok(ack_bytes) = ack.encode() {
                            let _ = socket_clone.send_to(&ack_bytes, src_addr).await;
                        }
                    }
                }
                Message::Events(events) => {
                    let mut lock = clients_clone.lock().await;
                    if let Some(session) = lock.get_mut(&src_addr) {
                        if !tm.is_approved(&session.client_id) {
                            warn!(client_addr = %src_addr, "Ignoring events from non-approved client");
                            continue;
                        }
                        session.last_seen = Instant::now();
                        if let Err(e) = session.vdev.emit(&events) {
                            warn!(client_addr = %src_addr, error = %e, "Failed to emit UDP events to virtual device");
                        }
                    }
                }
                Message::Ping => {
                    if let Ok(pong_bytes) = Message::Pong.encode() {
                        let _ = socket_clone.send_to(&pong_bytes, src_addr).await;
                    }
                }
                _ => {}
            }
        }
    }

    /// Iroh P2P connection listener loop.
    async fn run_iroh_server(
        endpoint: Endpoint,
        tm: TrustManager,
        active_sessions: Arc<Mutex<HashMap<String, VirtualOutput>>>,
    ) -> Result<()> {
        while let Some(incoming) = endpoint.accept().await {
            let tm_clone = tm.clone();
            let sessions_clone = Arc::clone(&active_sessions);
            tokio::spawn(async move {
                let accepting = match incoming.accept() {
                    Ok(a) => a,
                    Err(e) => {
                        warn!("Failed to accept incoming Iroh connection: {}", e);
                        return;
                    }
                };

                let conn = match accepting.await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Handshake failed with incoming Iroh client: {}", e);
                        return;
                    }
                };

                let remote_node = conn.remote_id().to_string();
                info!(client_id = %remote_node, "Iroh client connected");

                if let Err(e) = Self::handle_iroh_client(conn, tm_clone, sessions_clone).await {
                    warn!(client_id = %remote_node, error = %e, "Iroh client disconnected");
                } else {
                    info!(client_id = %remote_node, "Iroh client disconnected gracefully");
                }
            });
        }
        Ok(())
    }

    /// Handle connection from a single Iroh client.
    async fn handle_iroh_client(
        conn: iroh::endpoint::Connection,
        tm: TrustManager,
        active_sessions: Arc<Mutex<HashMap<String, VirtualOutput>>>,
    ) -> Result<()> {
        let (mut send, mut recv) = conn
            .accept_bi()
            .await
            .context("Failed to accept bi-directional stream")?;

        // 1. Expect Handshake
        let handshake_msg = Self::read_frame(&mut recv).await?;
        let payload = match handshake_msg {
            Message::Handshake(p) => p,
            other => bail!("Expected Handshake message, got: {:?}", other),
        };

        info!(
            client_id = %payload.client_id,
            hostname = %payload.client_hostname,
            device = %payload.metadata.name,
            "Received device handshake from Iroh client"
        );

        // 2. Security Approval Check
        if !tm.is_approved(&payload.client_id) {
            info!(client_id = %payload.client_id, "Iroh client requires server approval");
            tm.register_pending(
                payload.client_id.clone(),
                payload.client_hostname,
                payload.metadata.name,
                "Iroh P2P".into(),
            );
            let ack = Message::HandshakeAck {
                status: AckStatus::PendingApproval,
                server_hostname: Self::server_hostname(),
                message: format!(
                    "Connection pending authorization on server. Run 'joycast server approve {}' to authorize.",
                    payload.client_id
                ),
            };
            Self::write_frame(&mut send, &ack).await?;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(1000)).await;
            return Ok(());
        }

        // 3. Check for existing active session for this client_id and replace it IMMEDIATELY
        {
            let mut lock = active_sessions.lock().await;
            if let Some(old_vdev) = lock.remove(&payload.client_id) {
                info!(client_id = %payload.client_id, "Replacing existing active session for client ID");
                drop(old_vdev);
            }
        }

        // 4. Create new Virtual Device for approved client
        let new_vdev = match VirtualOutput::new(&payload.metadata) {
            Ok(dev) => {
                let ack = Message::HandshakeAck {
                    status: AckStatus::Approved,
                    server_hostname: Self::server_hostname(),
                    message: "Connection authorized".to_string(),
                };
                Self::write_frame(&mut send, &ack).await?;
                dev
            }
            Err(e) => {
                let ack = Message::HandshakeAck {
                    status: AckStatus::Rejected,
                    server_hostname: Self::server_hostname(),
                    message: format!("Failed to create virtual device: {}", e),
                };
                let _ = Self::write_frame(&mut send, &ack).await;
                let _ = send.finish();
                tokio::time::sleep(Duration::from_millis(500)).await;
                return Err(e);
            }
        };

        // Insert into active_sessions map
        let client_id = payload.client_id.clone();
        active_sessions
            .lock()
            .await
            .insert(client_id.clone(), new_vdev);

        // 5. Main event loop
        let loop_result = async {
            loop {
                let msg = match Self::read_frame(&mut recv).await {
                    Ok(m) => m,
                    Err(_e) => break,
                };

                match msg {
                    Message::Events(events) => {
                        let mut lock = active_sessions.lock().await;
                        if let Some(vdev) = lock.get_mut(&client_id) {
                            vdev.emit(&events)
                                .context("Failed to emit events to virtual device")?;
                        } else {
                            break;
                        }
                    }
                    Message::Ping => {
                        Self::write_frame(&mut send, &Message::Pong).await?;
                    }
                    _ => {}
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        // Clean up active session on disconnect
        {
            let mut lock = active_sessions.lock().await;
            if let Some(vdev) = lock.remove(&client_id) {
                drop(vdev);
            }
        }

        loop_result
    }

    pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Message> {
        let len = reader
            .read_u32()
            .await
            .context("Failed to read frame length")?;
        if len > 10 * 1024 * 1024 {
            bail!("Frame size too large: {} bytes", len);
        }
        let mut buf = vec![0u8; len as usize];
        reader
            .read_exact(&mut buf)
            .await
            .context("Failed to read frame payload")?;
        Message::decode(&buf).context("Failed to decode message payload")
    }

    pub async fn write_frame<W: AsyncWriteExt + Unpin>(
        writer: &mut W,
        msg: &Message,
    ) -> Result<()> {
        let bytes = msg.encode().context("Failed to encode message")?;
        writer
            .write_u32(bytes.len() as u32)
            .await
            .context("Failed to write frame length")?;
        writer
            .write_all(&bytes)
            .await
            .context("Failed to write frame payload")?;
        writer.flush().await.context("Failed to flush writer")?;
        Ok(())
    }
}
