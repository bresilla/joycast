use anyhow::{Context, Result, bail};
use iroh::endpoint::presets::N0;
use iroh::{Endpoint, PublicKey, SecretKey};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::device::VirtualOutput;
use crate::protocol::{ALPN, Message};
use crate::transport::DEFAULT_PORT;

pub struct ServerConfig {
    pub bind_addr: Option<SocketAddr>,
    pub secret_key: Option<String>,
    pub key_file: Option<PathBuf>,
    pub disable_udp: bool,
    pub disable_iroh: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: Some(SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT))),
            secret_key: None,
            key_file: None,
            disable_udp: false,
            disable_iroh: false,
        }
    }
}

pub struct JoycastServer {
    udp_socket: Option<Arc<UdpSocket>>,
    iroh_endpoint: Option<Endpoint>,
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
        // 1. Explicit secret key string passed
        if let Some(ref key_str) = config.secret_key {
            return Self::parse_secret_key_hex(key_str);
        }

        // 2. Explicit key file path passed
        if let Some(ref path) = config.key_file {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read key file at {}", path.display()))?;
            return Self::parse_secret_key_hex(&content);
        }

        // 3. System-wide & Sudo fallback locations:
        let mut candidates = Vec::new();

        if let Ok(sudo_user) = std::env::var("SUDO_USER")
            && !sudo_user.is_empty()
            && sudo_user != "root"
        {
            let sudo_path =
                PathBuf::from(format!("/home/{}/.config/joycast/server.key", sudo_user));
            candidates.push(sudo_path);
        }

        let etc_path = PathBuf::from("/etc/joycast/server.key");
        candidates.push(etc_path);

        if let Some(user_config) = dirs_next::config_dir() {
            candidates.push(user_config.join("joycast").join("server.key"));
        }

        // Search for existing key in candidate paths
        for path in &candidates {
            if path.exists()
                && let Ok(content) = fs::read_to_string(path)
                && let Ok(key) = Self::parse_secret_key_hex(&content)
            {
                info!("Loaded static server key from {}", path.display());
                return Ok(key);
            }
        }

        // If no existing key file found, pick target write path based on context:
        let target_path = if let Ok(sudo_user) = std::env::var("SUDO_USER")
            && !sudo_user.is_empty()
            && sudo_user != "root"
        {
            // Interactive sudo execution -> save to regular user's config
            PathBuf::from(format!("/home/{}/.config/joycast/server.key", sudo_user))
        } else if std::env::var("USER").unwrap_or_default() == "root"
            || std::env::var("JOURNAL_STREAM").is_ok()
            || std::env::var("SYSTEMD_EXEC_PID").is_ok()
        {
            // System service or direct root daemon -> save to /etc/joycast/server.key
            PathBuf::from("/etc/joycast/server.key")
        } else if let Some(user_config) = dirs_next::config_dir() {
            // Regular user -> save to ~/.config/joycast/server.key
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
        })
    }

    /// Return the server's Iroh Node ID (if Iroh is enabled).
    pub fn iroh_node_id(&self) -> Option<PublicKey> {
        self.iroh_endpoint
            .as_ref()
            .map(|ep| ep.secret_key().public())
    }

    /// Run the server, listening concurrently on Direct UDP and Iroh P2P.
    pub async fn run(self) -> Result<()> {
        let mut handles = Vec::new();

        // 1. Direct UDP Listener
        if let Some(socket) = self.udp_socket {
            handles.push(tokio::spawn(async move {
                if let Err(e) = Self::run_udp_server(socket).await {
                    error!("Direct UDP server loop error: {}", e);
                }
            }));
        }

        // 2. Iroh P2P Listener
        if let Some(endpoint) = self.iroh_endpoint {
            handles.push(tokio::spawn(async move {
                if let Err(e) = Self::run_iroh_server(endpoint).await {
                    error!("Iroh server loop error: {}", e);
                }
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Direct UDP packet handler loop.
    async fn run_udp_server(socket: Arc<UdpSocket>) -> Result<()> {
        let clients: Arc<Mutex<HashMap<SocketAddr, VirtualOutput>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut buf = vec![0u8; 65535];

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
                Message::Handshake(meta) => {
                    info!(client_addr = %src_addr, device = %meta.name, "Direct UDP Handshake received");
                    match VirtualOutput::new(&meta) {
                        Ok(vdev) => {
                            clients_clone.lock().await.insert(src_addr, vdev);
                            let ack = Message::HandshakeAck {
                                success: true,
                                message: "Virtual uinput device initialized (Direct UDP)".into(),
                            };
                            if let Ok(ack_bytes) = ack.encode() {
                                let _ = socket_clone.send_to(&ack_bytes, src_addr).await;
                            }
                        }
                        Err(e) => {
                            let ack = Message::HandshakeAck {
                                success: false,
                                message: format!("Failed to create virtual device: {}", e),
                            };
                            if let Ok(ack_bytes) = ack.encode() {
                                let _ = socket_clone.send_to(&ack_bytes, src_addr).await;
                            }
                        }
                    }
                }
                Message::Events(events) => {
                    let mut lock = clients_clone.lock().await;
                    if let Some(vdev) = lock.get_mut(&src_addr)
                        && let Err(e) = vdev.emit(&events)
                    {
                        warn!(client_addr = %src_addr, error = %e, "Failed to emit UDP events to virtual device");
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
    async fn run_iroh_server(endpoint: Endpoint) -> Result<()> {
        while let Some(incoming) = endpoint.accept().await {
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

                if let Err(e) = Self::handle_iroh_client(conn).await {
                    warn!(client_id = %remote_node, error = %e, "Iroh client disconnected with error");
                } else {
                    info!(client_id = %remote_node, "Iroh client disconnected gracefully");
                }
            });
        }
        Ok(())
    }

    /// Handle connection from a single Iroh client.
    async fn handle_iroh_client(conn: iroh::endpoint::Connection) -> Result<()> {
        let (mut send, mut recv) = conn
            .accept_bi()
            .await
            .context("Failed to accept bi-directional stream")?;

        // 1. Expect Handshake
        let handshake_msg = Self::read_frame(&mut recv).await?;
        let meta = match handshake_msg {
            Message::Handshake(meta) => meta,
            other => bail!("Expected Handshake message, got: {:?}", other),
        };

        info!(device = %meta.name, "Received device handshake from Iroh client");

        // 2. Create Virtual Device
        let mut virtual_device = match VirtualOutput::new(&meta) {
            Ok(dev) => {
                let ack = Message::HandshakeAck {
                    success: true,
                    message: "Virtual uinput device initialized".to_string(),
                };
                Self::write_frame(&mut send, &ack).await?;
                dev
            }
            Err(e) => {
                let ack = Message::HandshakeAck {
                    success: false,
                    message: format!("Failed to create virtual device: {}", e),
                };
                let _ = Self::write_frame(&mut send, &ack).await;
                return Err(e);
            }
        };

        // 3. Main event loop
        loop {
            let msg = match Self::read_frame(&mut recv).await {
                Ok(m) => m,
                Err(_e) => break,
            };

            match msg {
                Message::Events(events) => {
                    virtual_device
                        .emit(&events)
                        .context("Failed to emit events to virtual device")?;
                }
                Message::Ping => {
                    Self::write_frame(&mut send, &Message::Pong).await?;
                }
                _ => {}
            }
        }

        Ok(())
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
