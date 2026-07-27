use anyhow::{Context, Result, bail};
use evdev::Device;
use futures::StreamExt;
use iroh::endpoint::presets::N0;
use iroh::{Endpoint, SecretKey};
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

use crate::device::{DeviceScanner, extract_metadata};
use crate::history::HistoryStore;
use crate::protocol::{ALPN, AckStatus, EventWire, HandshakePayload, Message};
use crate::server::JoycastServer;
use crate::transport::TargetAddress;

#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    pub target: Option<String>,
    pub device_path: Option<PathBuf>,
    pub keyboard: bool,
    pub mouse: bool,
    pub all: bool,
    pub history_file: Option<PathBuf>,
    pub client_id_file: Option<PathBuf>,
}

pub struct JoycastClient {
    target: TargetAddress,
    target_raw: String,
    device_path: PathBuf,
    client_id: String,
    history_file: Option<PathBuf>,
}

impl JoycastClient {
    /// Load or generate a persistent client identity ID.
    fn load_or_create_client_id(custom_path: Option<PathBuf>) -> String {
        let id_file = if let Some(p) = custom_path {
            p
        } else {
            let base_dir = dirs_next::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("joycast");
            fs::create_dir_all(&base_dir).ok();
            base_dir.join("client_id")
        };

        if id_file.exists()
            && let Ok(id) = fs::read_to_string(&id_file)
            && !id.trim().is_empty()
        {
            return id.trim().to_string();
        }

        let new_id = hex::encode(SecretKey::generate().to_bytes());
        if let Some(parent) = id_file.parent() {
            fs::create_dir_all(parent).ok();
        }
        let _ = fs::write(&id_file, &new_id);
        new_id
    }

    /// Select target server interactively from history if not specified, or resolve hostname from history.
    pub fn resolve_target(
        specified: Option<String>,
        history_file: Option<PathBuf>,
    ) -> Result<(TargetAddress, String)> {
        if let Some(target_str) = specified {
            if let Ok(target) = TargetAddress::from_str(&target_str) {
                return Ok((target, target_str));
            }

            // Target is not a direct IP or valid Iroh Node ID.
            // Search known server history for a matching server hostname!
            if let Ok(history) = HistoryStore::with_path(history_file.clone())
                && let Some(matched) = history.find_by_hostname(&target_str)
            {
                info!(
                    "Resolved server hostname '{}' -> target '{}' ({})",
                    matched.server_hostname, matched.target, matched.transport_type
                );
                let target = TargetAddress::from_str(&matched.target)?;
                return Ok((target, matched.target.clone()));
            }

            bail!(
                "Could not resolve target '{}'. Must be an IP address (e.g. 192.168.1.50:12398), a 64-character Iroh Node ID, or a known server hostname in history.",
                target_str
            );
        }

        let history = HistoryStore::with_path(history_file)?;
        let servers = history.list_servers();

        if servers.is_empty() {
            bail!(
                "No target specified and no previously connected servers found. Specify a target with 'joycast client <TARGET>'"
            );
        }

        println!("\nSelect a previously connected server:");
        for (idx, s) in servers.iter().enumerate() {
            println!(
                "  [{}] Hostname: {}\n      Target: {}\n      Transport: {}\n      Last Connected: {}\n",
                idx + 1,
                s.server_hostname,
                s.target,
                s.transport_type,
                s.last_connected
            );
        }

        print!("Enter selection [1-{}]: ", servers.len());
        use std::io::Write;
        std::io::stdout().flush().ok();

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .context("Failed to read input selection")?;

        let choice: usize = input.trim().parse().context("Invalid selection index")?;
        let selected = history
            .get_by_index(choice)
            .context("Selection out of bounds")?;

        info!(
            "Selected server: {} ({})",
            selected.server_hostname, selected.target
        );
        let target = TargetAddress::from_str(&selected.target)?;
        Ok((target, selected.target))
    }

    /// Initialize a Joycast client.
    pub fn new(config: ClientConfig) -> Result<Self> {
        let (target, target_raw) =
            Self::resolve_target(config.target, config.history_file.clone())?;
        let client_id = Self::load_or_create_client_id(config.client_id_file);

        let device_path = match config.device_path {
            Some(path) => path,
            None => {
                let devices =
                    DeviceScanner::list_devices_filtered(config.keyboard, config.mouse, config.all);
                if devices.is_empty() {
                    bail!(
                        "No matching gamepad or joystick found. Pass '--keyboard' (-k), '--mouse' (-m), or '--all' (-a) to include other device types, or specify '--device <PATH>'."
                    );
                } else if devices.len() == 1 {
                    let path = devices[0].path.clone();
                    info!(
                        "Auto-detected input device: {} ({})",
                        path.display(),
                        devices[0].name
                    );
                    path
                } else {
                    println!("\nSelect input device to stream:");
                    for (idx, dev) in devices.iter().enumerate() {
                        println!(
                            "  [{}] Path: {}\n      Name: {}\n      Type: {}\n",
                            idx + 1,
                            dev.path.display(),
                            dev.name,
                            dev.device_type
                        );
                    }
                    print!("Enter selection [1-{}]: ", devices.len());
                    use std::io::Write;
                    std::io::stdout().flush().ok();

                    let mut input = String::new();
                    std::io::stdin()
                        .read_line(&mut input)
                        .context("Failed to read device selection")?;

                    let choice: usize = input.trim().parse().context("Invalid selection index")?;
                    if choice == 0 || choice > devices.len() {
                        bail!("Selection index out of bounds");
                    }
                    let selected_path = devices[choice - 1].path.clone();
                    info!(
                        "Selected input device: {} ({})",
                        selected_path.display(),
                        devices[choice - 1].name
                    );
                    selected_path
                }
            }
        };

        Ok(Self {
            target,
            target_raw,
            device_path,
            client_id,
            history_file: config.history_file,
        })
    }

    /// Helper to get current host name.
    fn client_hostname() -> String {
        gethostname::gethostname().to_string_lossy().into_owned()
    }

    /// Run the client, streaming input events to the target server.
    pub async fn run(self) -> Result<()> {
        let device = Device::open(&self.device_path)
            .with_context(|| format!("Failed to open device at {}", self.device_path.display()))?;

        let metadata = extract_metadata(&device);
        info!(
            device_path = %self.device_path.display(),
            name = %metadata.name,
            keys = metadata.keys.len(),
            abs_axes = metadata.abs_axes.len(),
            transport = %self.target.display_type(),
            target = %self.target,
            client_id = %self.client_id,
            "Opened input device"
        );

        let dev_id_str = self.device_path.to_string_lossy().into_owned();

        match self.target {
            TargetAddress::Ip(addr) => {
                Self::run_udp_client(
                    device,
                    metadata,
                    addr,
                    self.client_id,
                    dev_id_str,
                    self.target_raw,
                    self.history_file,
                )
                .await
            }
            TargetAddress::Iroh(node_id) => {
                Self::run_iroh_client(
                    device,
                    metadata,
                    node_id,
                    self.client_id,
                    dev_id_str,
                    self.target_raw,
                    self.history_file,
                )
                .await
            }
        }
    }

    /// Direct UDP Client loop.
    async fn run_udp_client(
        device: Device,
        metadata: crate::protocol::DeviceMetadata,
        server_addr: std::net::SocketAddr,
        client_id: String,
        device_id: String,
        target_raw: String,
        history_file: Option<PathBuf>,
    ) -> Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind local UDP socket")?;
        socket
            .connect(server_addr)
            .await
            .with_context(|| format!("Failed to connect UDP socket to {}", server_addr))?;

        info!(
            "Sending Handshake to Direct UDP server at {}...",
            server_addr
        );
        let payload = HandshakePayload {
            client_id: client_id.clone(),
            client_hostname: Self::client_hostname(),
            metadata,
            device_id: Some(device_id),
        };

        let handshake = Message::Handshake(payload);
        let bytes = handshake.encode().context("Failed to encode handshake")?;
        socket
            .send(&bytes)
            .await
            .context("Failed to send handshake UDP packet")?;

        // Await HandshakeAck
        let mut buf = vec![0u8; 65535];
        let ack_res = tokio::time::timeout(Duration::from_secs(4), socket.recv(&mut buf)).await;
        match ack_res {
            Ok(Ok(len)) => {
                let ack_msg =
                    Message::decode(&buf[..len]).context("Failed to decode HandshakeAck")?;
                match ack_msg {
                    Message::HandshakeAck {
                        status,
                        server_hostname,
                        message,
                    } => match status {
                        AckStatus::Approved => {
                            info!("Server authorized connection: {}", message);
                            if let Ok(mut history) = HistoryStore::with_path(history_file) {
                                let _ = history.record_connection(
                                    server_hostname,
                                    target_raw,
                                    "Direct UDP".into(),
                                );
                            }
                        }
                        AckStatus::PendingApproval => {
                            warn!("------------------------------------------------------------");
                            warn!("Server Authorization Required!");
                            warn!("Your Client ID: {}", client_id);
                            warn!("Message: {}", message);
                            warn!("Please ask the server admin to run:");
                            warn!("  joycast server approve {}", client_id);
                            warn!("------------------------------------------------------------");
                            bail!("Connection pending server authorization.");
                        }
                        AckStatus::Rejected => {
                            bail!("Server rejected connection: {}", message);
                        }
                    },
                    other => bail!("Expected HandshakeAck from server, got: {:?}", other),
                }
            }
            Ok(Err(e)) => bail!("Error receiving HandshakeAck: {}", e),
            Err(_) => {
                warn!("HandshakeAck timed out (server might not respond to ACK)");
            }
        }

        info!("Forwarding input events over Direct UDP. Press Ctrl+C to stop.");

        let mut event_stream = device
            .into_event_stream()
            .context("Failed to create async event stream for device")?;

        let mut event_batch = Vec::new();

        while let Some(ev_res) = event_stream.next().await {
            match ev_res {
                Ok(ev) => {
                    event_batch.push(EventWire {
                        type_: ev.event_type().0,
                        code: ev.code(),
                        value: ev.value(),
                    });

                    if !event_batch.is_empty() {
                        let msg = Message::Events(std::mem::take(&mut event_batch));
                        if let Ok(msg_bytes) = msg.encode()
                            && let Err(e) = socket.send(&msg_bytes).await
                        {
                            warn!("Failed to send UDP event batch: {}", e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading evdev event: {}", e);
                    break;
                }
            }
        }

        info!("Joycast UDP client shutting down...");
        Ok(())
    }

    /// Iroh P2P Client loop.
    async fn run_iroh_client(
        device: Device,
        metadata: crate::protocol::DeviceMetadata,
        node_id: iroh::PublicKey,
        client_id: String,
        device_id: String,
        target_raw: String,
        history_file: Option<PathBuf>,
    ) -> Result<()> {
        let endpoint = Endpoint::builder(N0)
            .secret_key(SecretKey::generate())
            .bind()
            .await
            .context("Failed to bind client iroh endpoint")?;

        info!("Connecting to Joycast server Iroh Node ID: {}...", node_id);
        let conn = endpoint
            .connect(node_id, ALPN)
            .await
            .context("Failed to connect to server via iroh")?;

        info!("Connected to server! Opening stream...");
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .context("Failed to open bi-directional stream")?;

        let run_res = async {
            // 1. Send Handshake
            let payload = HandshakePayload {
                client_id: client_id.clone(),
                client_hostname: Self::client_hostname(),
                metadata,
                device_id: Some(device_id),
            };

            let handshake = Message::Handshake(payload);
            JoycastServer::write_frame(&mut send, &handshake).await?;
            info!("Handshake sent to server, awaiting acknowledgment...");

            // 2. Receive HandshakeAck
            let ack_msg = JoycastServer::read_frame(&mut recv).await?;
            match ack_msg {
                Message::HandshakeAck {
                    status,
                    server_hostname,
                    message,
                } => match status {
                    AckStatus::Approved => {
                        info!("Server authorized connection: {}", message);
                        if let Ok(mut history) = HistoryStore::with_path(history_file) {
                            let _ = history.record_connection(
                                server_hostname,
                                target_raw,
                                "Iroh P2P".into(),
                            );
                        }
                    }
                    AckStatus::PendingApproval => {
                        warn!("------------------------------------------------------------");
                        warn!("Server Authorization Required!");
                        warn!("Your Client ID: {}", client_id);
                        warn!("Message: {}", message);
                        warn!("Please ask the server admin to run:");
                        warn!("  joycast server approve {}", client_id);
                        warn!("------------------------------------------------------------");
                        bail!("Connection pending server authorization.");
                    }
                    AckStatus::Rejected => {
                        bail!("Server rejected connection: {}", message);
                    }
                },
                other => bail!("Expected HandshakeAck from server, got: {:?}", other),
            }

            info!("Forwarding input events over Iroh P2P. Press Ctrl+C to stop.");

            let mut event_stream = device
                .into_event_stream()
                .context("Failed to create async event stream for device")?;

            let mut event_batch = Vec::new();

            while let Some(ev_res) = event_stream.next().await {
                match ev_res {
                    Ok(ev) => {
                        event_batch.push(EventWire {
                            type_: ev.event_type().0,
                            code: ev.code(),
                            value: ev.value(),
                        });

                        if !event_batch.is_empty() {
                            let msg = Message::Events(std::mem::take(&mut event_batch));
                            if let Err(e) = JoycastServer::write_frame(&mut send, &msg).await {
                                warn!("Failed to send event batch to server: {}", e);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading evdev event: {}", e);
                        break;
                    }
                }
            }

            info!("Joycast Iroh client shutting down...");
            Ok(())
        }
        .await;

        endpoint.close().await;
        run_res
    }
}
