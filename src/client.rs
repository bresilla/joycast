use anyhow::{Context, Result, bail};
use evdev::Device;
use futures::StreamExt;
use iroh::endpoint::presets::N0;
use iroh::{Endpoint, SecretKey};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

use crate::device::{DeviceScanner, extract_metadata};
use crate::protocol::{ALPN, EventWire, Message};
use crate::server::JoycastServer;
use crate::transport::TargetAddress;

pub struct ClientConfig {
    pub target: String,
    pub device_path: Option<PathBuf>,
}

pub struct JoycastClient {
    target: TargetAddress,
    device_path: PathBuf,
}

impl JoycastClient {
    /// Initialize a Joycast client from target string (IP address or Iroh Node ID).
    pub fn new(config: ClientConfig) -> Result<Self> {
        let target = TargetAddress::from_str(&config.target)?;

        let device_path = match config.device_path {
            Some(path) => path,
            None => {
                info!("No device specified, searching for gamepad/joystick...");
                match DeviceScanner::find_first_gamepad() {
                    Some(path) => {
                        info!("Auto-detected input device: {}", path.display());
                        path
                    }
                    None => {
                        bail!(
                            "No gamepad or joystick found. Use `joycast list` to see available devices and specify one with --device <PATH>"
                        );
                    }
                }
            }
        };

        Ok(Self {
            target,
            device_path,
        })
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
            "Opened input device"
        );

        match self.target {
            TargetAddress::Ip(addr) => Self::run_udp_client(device, metadata, addr).await,
            TargetAddress::Iroh(node_id) => Self::run_iroh_client(device, metadata, node_id).await,
        }
    }

    /// Direct UDP Client loop.
    async fn run_udp_client(
        device: Device,
        metadata: crate::protocol::DeviceMetadata,
        server_addr: std::net::SocketAddr,
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
        let handshake = Message::Handshake(metadata);
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
                    Message::HandshakeAck { success, message } => {
                        if success {
                            info!("Server acknowledged device creation: {}", message);
                        } else {
                            bail!("Server rejected device creation: {}", message);
                        }
                    }
                    other => bail!("Expected HandshakeAck from server, got: {:?}", other),
                }
            }
            Ok(Err(e)) => bail!("Error receiving HandshakeAck: {}", e),
            Err(_) => {
                warn!(
                    "HandshakeAck timed out (server might not respond to ACK, continuing event forwarding...)"
                );
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

        // 1. Send Handshake
        let handshake = Message::Handshake(metadata);
        JoycastServer::write_frame(&mut send, &handshake).await?;
        info!("Handshake sent to server, awaiting acknowledgment...");

        // 2. Receive HandshakeAck
        let ack_msg = JoycastServer::read_frame(&mut recv).await?;
        match ack_msg {
            Message::HandshakeAck { success, message } => {
                if success {
                    info!("Server acknowledged device creation: {}", message);
                } else {
                    bail!("Server rejected device creation: {}", message);
                }
            }
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
}
