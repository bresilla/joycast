use anyhow::{Context, Result, bail};
use evdev::Device;
use futures::StreamExt;
use iroh::endpoint::presets::N0;
use iroh::{Endpoint, SecretKey};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at};
use tracing::{info, warn};

use crate::device::{DeviceScanner, extract_metadata};
use crate::history::HistoryStore;
use crate::protocol::{ALPN, AckStatus, DeviceMetadata, EventWire, HandshakePayload, Message};
use crate::server::JoycastServer;
use crate::transport::TargetAddress;

const DEVICE_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

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
    device_selection: DeviceSelection,
    client_id: String,
    history_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct DeviceSelection {
    requested_path: Option<PathBuf>,
    keyboard: bool,
    mouse: bool,
    all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceIdentity {
    name: String,
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

impl DeviceIdentity {
    fn from_metadata(metadata: &DeviceMetadata) -> Self {
        Self {
            name: metadata.name.clone(),
            bustype: metadata.bustype,
            vendor: metadata.vendor,
            product: metadata.product,
            version: metadata.version,
        }
    }

    fn matches(&self, metadata: &DeviceMetadata) -> bool {
        self == &Self::from_metadata(metadata)
    }
}

struct ReconnectingDevice {
    preferred_path: PathBuf,
    identity: DeviceIdentity,
}

impl ReconnectingDevice {
    fn new(preferred_path: PathBuf, metadata: &DeviceMetadata) -> Self {
        Self {
            preferred_path,
            identity: DeviceIdentity::from_metadata(metadata),
        }
    }

    /// Try the previous event node first, then locate the same physical device if udev
    /// assigned it a different `/dev/input/eventN` path after reconnection.
    fn try_reopen(&mut self) -> Option<Device> {
        let mut fallback = None;

        for (path, device) in evdev::enumerate() {
            let metadata = extract_metadata(&device);
            if !self.identity.matches(&metadata) {
                continue;
            }

            if path == self.preferred_path {
                return Some(device);
            }

            if fallback.is_none() {
                fallback = Some((path, device));
            }
        }

        if let Some((path, device)) = fallback {
            info!(
                old_path = %self.preferred_path.display(),
                new_path = %path.display(),
                "Reconnected controller was assigned a new input path"
            );
            self.preferred_path = path;
            Some(device)
        } else {
            None
        }
    }
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

        let requested_path = match config.device_path {
            Some(path) => Some(path),
            None => {
                let devices =
                    DeviceScanner::list_devices_filtered(config.keyboard, config.mouse, config.all);
                if devices.is_empty() {
                    info!(
                        "No matching input device is connected yet; the client will connect to the server and wait"
                    );
                    None
                } else if devices.len() == 1 {
                    let path = devices[0].path.clone();
                    info!(
                        "Auto-detected input device: {} ({})",
                        path.display(),
                        devices[0].name
                    );
                    Some(path)
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
                    Some(selected_path)
                }
            }
        };

        Ok(Self {
            target,
            target_raw,
            device_selection: DeviceSelection {
                requested_path,
                keyboard: config.keyboard,
                mouse: config.mouse,
                all: config.all,
            },
            client_id,
            history_file: config.history_file,
        })
    }

    /// Helper to get current host name.
    fn client_hostname() -> String {
        gethostname::gethostname().to_string_lossy().into_owned()
    }

    fn try_open_initial_device(selection: &mut DeviceSelection) -> Option<(Device, PathBuf)> {
        if let Some(path) = &selection.requested_path {
            return Device::open(path).ok().map(|device| (device, path.clone()));
        }

        let selected = DeviceScanner::list_devices_filtered(
            selection.keyboard,
            selection.mouse,
            selection.all,
        )
        .into_iter()
        .next()?;

        match Device::open(&selected.path) {
            Ok(device) => {
                selection.requested_path = Some(selected.path.clone());
                Some((device, selected.path))
            }
            Err(_) => None,
        }
    }

    fn log_opened_device(
        path: &Path,
        metadata: &DeviceMetadata,
        target: &TargetAddress,
        client_id: &str,
    ) {
        info!(
            device_path = %path.display(),
            name = %metadata.name,
            keys = metadata.keys.len(),
            abs_axes = metadata.abs_axes.len(),
            transport = %target.display_type(),
            target = %target,
            client_id,
            "Opened input device"
        );
    }

    fn reconnect_timer() -> tokio::time::Interval {
        let mut timer = interval(DEVICE_RETRY_INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        timer
    }

    fn keepalive_timer() -> tokio::time::Interval {
        let mut timer = interval_at(Instant::now() + KEEPALIVE_INTERVAL, KEEPALIVE_INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        timer
    }

    fn event_message(event: evdev::InputEvent) -> Message {
        Message::Events(vec![EventWire {
            type_: event.event_type().0,
            code: event.code(),
            value: event.value(),
        }])
    }

    /// Run the client, streaming input events to the target server.
    pub async fn run(self) -> Result<()> {
        let Self {
            target,
            target_raw,
            device_selection,
            client_id,
            history_file,
        } = self;

        match target {
            TargetAddress::Ip(addr) => {
                Self::run_udp_client(addr, client_id, target_raw, history_file, device_selection)
                    .await
            }
            TargetAddress::Iroh(node_id) => {
                Self::run_iroh_client(
                    node_id,
                    client_id,
                    target_raw,
                    history_file,
                    device_selection,
                )
                .await
            }
        }
    }

    /// Direct UDP Client loop.
    async fn run_udp_client(
        server_addr: std::net::SocketAddr,
        client_id: String,
        target_raw: String,
        history_file: Option<PathBuf>,
        mut device_selection: DeviceSelection,
    ) -> Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind local UDP socket")?;
        socket
            .connect(server_addr)
            .await
            .with_context(|| format!("Failed to connect UDP socket to {}", server_addr))?;

        info!(%server_addr, "Direct UDP transport ready");
        if let Some(path) = &device_selection.requested_path {
            info!(device_path = %path.display(), "Waiting for input device");
        } else {
            info!("Waiting for a matching input device");
        }

        let mut buf = vec![0u8; 65535];
        let mut reconnect_timer = Self::reconnect_timer();
        let mut keepalive_timer = Self::keepalive_timer();

        // Establish the transport first. This lets the client be started before a
        // Bluetooth/USB controller is powered on.
        let (device, device_path) = loop {
            tokio::select! {
                _ = reconnect_timer.tick() => {
                    if let Some(opened) = Self::try_open_initial_device(&mut device_selection) {
                        break opened;
                    }
                }
                _ = keepalive_timer.tick() => {
                    let ping = Message::Ping.encode().context("Failed to encode keepalive")?;
                    socket.send(&ping).await.context("Failed to send UDP keepalive")?;
                }
                recv_result = socket.recv(&mut buf) => {
                    if let Err(error) = recv_result {
                        warn!(%error, "UDP receive error while waiting for controller");
                    }
                }
            }
        };

        let metadata = extract_metadata(&device);
        Self::log_opened_device(
            &device_path,
            &metadata,
            &TargetAddress::Ip(server_addr),
            &client_id,
        );
        let device_id = device_path.to_string_lossy().into_owned();

        info!(%server_addr, "Sending handshake to Direct UDP server");
        let payload = HandshakePayload {
            client_id: client_id.clone(),
            client_hostname: Self::client_hostname(),
            metadata: metadata.clone(),
            device_id: Some(device_id),
        };

        let handshake = Message::Handshake(payload);
        let bytes = handshake.encode().context("Failed to encode handshake")?;
        socket
            .send(&bytes)
            .await
            .context("Failed to send handshake UDP packet")?;

        // Await HandshakeAck
        let ack_res = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let len = socket.recv(&mut buf).await?;
                let message = Message::decode(&buf[..len]);
                if matches!(message, Ok(Message::HandshakeAck { .. })) {
                    return Ok::<Message, anyhow::Error>(message?);
                }
            }
        })
        .await;
        match ack_res {
            Ok(Ok(ack_msg)) => match ack_msg {
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
            },
            Ok(Err(error)) => bail!("Error receiving HandshakeAck: {}", error),
            Err(_) => {
                warn!("HandshakeAck timed out (server might not respond to ACK)");
            }
        }

        info!("Forwarding input events over Direct UDP. Press Ctrl+C to stop.");
        let mut reconnecting_device = ReconnectingDevice::new(device_path, &metadata);
        let mut event_stream = Some(
            device
                .into_event_stream()
                .context("Failed to create async event stream for device")?,
        );

        loop {
            if let Some(stream) = event_stream.as_mut() {
                tokio::select! {
                    event = stream.next() => {
                        match event {
                            Some(Ok(event)) => {
                                let bytes = Self::event_message(event)
                                    .encode()
                                    .context("Failed to encode input event")?;
                                socket.send(&bytes).await.context("Failed to send UDP input event")?;
                            }
                            Some(Err(error)) => {
                                warn!(%error, "Controller disconnected; keeping server connection alive and waiting for it to return");
                                event_stream = None;
                            }
                            None => {
                                info!("Controller disconnected; keeping server connection alive and waiting for it to return");
                                event_stream = None;
                            }
                        }
                    }
                    _ = keepalive_timer.tick() => {
                        let ping = Message::Ping.encode().context("Failed to encode keepalive")?;
                        socket.send(&ping).await.context("Failed to send UDP keepalive")?;
                    }
                    recv_result = socket.recv(&mut buf) => {
                        if let Err(error) = recv_result {
                            warn!(%error, "UDP receive error");
                        }
                    }
                }
            } else {
                tokio::select! {
                    _ = reconnect_timer.tick() => {
                        if let Some(device) = reconnecting_device.try_reopen() {
                            match device.into_event_stream() {
                                Ok(stream) => {
                                    info!(
                                        device_path = %reconnecting_device.preferred_path.display(),
                                        device = %reconnecting_device.identity.name,
                                        "Controller reconnected; resuming input forwarding"
                                    );
                                    event_stream = Some(stream);
                                }
                                Err(error) => {
                                    warn!(%error, "Controller returned but its event stream could not be opened; retrying");
                                }
                            }
                        }
                    }
                    _ = keepalive_timer.tick() => {
                        let ping = Message::Ping.encode().context("Failed to encode keepalive")?;
                        socket.send(&ping).await.context("Failed to send UDP keepalive")?;
                    }
                    recv_result = socket.recv(&mut buf) => {
                        if let Err(error) = recv_result {
                            warn!(%error, "UDP receive error while waiting for controller");
                        }
                    }
                }
            }
        }
    }

    /// Iroh P2P Client loop.
    async fn run_iroh_client(
        node_id: iroh::PublicKey,
        client_id: String,
        target_raw: String,
        history_file: Option<PathBuf>,
        mut device_selection: DeviceSelection,
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
            if let Some(path) = &device_selection.requested_path {
                info!(device_path = %path.display(), "Waiting for input device");
            } else {
                info!("Waiting for a matching input device");
            }

            let mut reconnect_timer = Self::reconnect_timer();
            let mut keepalive_timer = Self::keepalive_timer();

            // The server accepts Ping before the device handshake so this Iroh
            // stream can stay established while the controller is powered off.
            let (device, device_path) = loop {
                tokio::select! {
                    _ = reconnect_timer.tick() => {
                        if let Some(opened) = Self::try_open_initial_device(&mut device_selection) {
                            break opened;
                        }
                    }
                    _ = keepalive_timer.tick() => {
                        JoycastServer::write_frame(&mut send, &Message::Ping).await?;
                        let response = tokio::time::timeout(
                            KEEPALIVE_TIMEOUT,
                            JoycastServer::read_frame(&mut recv),
                        )
                        .await
                        .context("Server did not answer controller-wait keepalive")??;
                        if response != Message::Pong {
                            bail!("Expected Pong while waiting for controller, got: {:?}", response);
                        }
                    }
                }
            };

            let metadata = extract_metadata(&device);
            Self::log_opened_device(
                &device_path,
                &metadata,
                &TargetAddress::Iroh(node_id),
                &client_id,
            );
            let device_id = device_path.to_string_lossy().into_owned();

            // 1. Send Handshake
            let payload = HandshakePayload {
                client_id: client_id.clone(),
                client_hostname: Self::client_hostname(),
                metadata: metadata.clone(),
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
            let mut reconnecting_device = ReconnectingDevice::new(device_path, &metadata);
            let mut event_stream = Some(
                device
                    .into_event_stream()
                    .context("Failed to create async event stream for device")?,
            );

            // Keep one persistent read future so frame reads are never cancelled
            // halfway through when another select branch wins.
            let server_messages = async {
                loop {
                    let message = JoycastServer::read_frame(&mut recv).await?;
                    if message != Message::Pong {
                        warn!(?message, "Unexpected message from server");
                    }
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            };
            tokio::pin!(server_messages);

            loop {
                if let Some(stream) = event_stream.as_mut() {
                    tokio::select! {
                        event = stream.next() => {
                            match event {
                                Some(Ok(event)) => {
                                    JoycastServer::write_frame(&mut send, &Self::event_message(event)).await?;
                                }
                                Some(Err(error)) => {
                                    warn!(%error, "Controller disconnected; keeping server connection alive and waiting for it to return");
                                    event_stream = None;
                                }
                                None => {
                                    info!("Controller disconnected; keeping server connection alive and waiting for it to return");
                                    event_stream = None;
                                }
                            }
                        }
                        _ = keepalive_timer.tick() => {
                            JoycastServer::write_frame(&mut send, &Message::Ping).await?;
                        }
                        server_result = &mut server_messages => {
                            server_result.context("Server connection closed")?;
                            bail!("Server connection closed");
                        }
                    }
                } else {
                    tokio::select! {
                        _ = reconnect_timer.tick() => {
                            if let Some(device) = reconnecting_device.try_reopen() {
                                match device.into_event_stream() {
                                    Ok(stream) => {
                                        info!(
                                            device_path = %reconnecting_device.preferred_path.display(),
                                            device = %reconnecting_device.identity.name,
                                            "Controller reconnected; resuming input forwarding"
                                        );
                                        event_stream = Some(stream);
                                    }
                                    Err(error) => {
                                        warn!(%error, "Controller returned but its event stream could not be opened; retrying");
                                    }
                                }
                            }
                        }
                        _ = keepalive_timer.tick() => {
                            JoycastServer::write_frame(&mut send, &Message::Ping).await?;
                        }
                        server_result = &mut server_messages => {
                            server_result.context("Server connection closed")?;
                            bail!("Server connection closed");
                        }
                    }
                }
            }
        }
        .await;

        endpoint.close().await;
        run_res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(name: &str, vendor: u16, product: u16) -> DeviceMetadata {
        DeviceMetadata {
            name: name.into(),
            bustype: 3,
            vendor,
            product,
            version: 1,
            keys: vec![304, 305],
            abs_axes: Vec::new(),
            rel_axes: Vec::new(),
        }
    }

    #[test]
    fn reconnect_identity_matches_same_physical_device_model() {
        let original = metadata("Wireless Controller", 0x054c, 0x09cc);
        let identity = DeviceIdentity::from_metadata(&original);

        let mut reconnected = original.clone();
        reconnected.keys.push(307);

        assert!(identity.matches(&reconnected));
    }

    #[test]
    fn reconnect_identity_rejects_a_different_device() {
        let identity = DeviceIdentity::from_metadata(&metadata("Controller A", 1, 2));

        assert!(!identity.matches(&metadata("Controller B", 1, 2)));
        assert!(!identity.matches(&metadata("Controller A", 1, 3)));
    }

    #[test]
    fn input_event_is_preserved_on_the_wire() {
        let message = JoycastClient::event_message(evdev::InputEvent::new(1, 304, 1));

        assert_eq!(
            message,
            Message::Events(vec![EventWire {
                type_: 1,
                code: 304,
                value: 1,
            }])
        );
    }
}
