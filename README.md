# Joycast

> High-performance, low-latency gamepad and udev input device forwarder over **Iroh P2P** and **Direct IP (UDP)** with security authorization and connection history.

Inspired by [warpout](https://github.com/bresilla/warpout), `joycast` captures physical Linux input devices (gamepads, joysticks, controllers, keyboards, mice) on a client machine and forwards their events in real time to a remote server. The server synthesizes a matching virtual `uinput` device on the host system.

---

## Key Features

- **Dual Transport Support**:
  - **Iroh P2P**: Connects via encrypted, NAT-traversing P2P tunnels using an Iroh `Node ID`. Works across firewalls and the Internet without port forwarding.
  - **Direct IP (UDP)**: Ultra-low-latency direct UDP forwarding for local LAN networks.
- **Client Authorization & Security**:
  - First-time connections are untrusted by default to prevent unauthorized access.
  - Server maintains a pending list and requires explicit authorization via `joycast server approve <CLIENT_ID>`.
- **Client's Local Connection History**:
  - The client locally remembers servers it has successfully connected to (storing server hostname, target address, and transport).
  - Running `joycast client` without arguments displays an interactive menu of previously saved servers on the client.
- **Dynamic Device Mirroring**:
  - Reads client input capabilities (buttons, axes, fuzz, flat, resolution, vendor/product IDs).
  - Synthesizes an identical virtual `uinput` device on the server.
- **Modular Library & Single Binary**:
  - Full Rust library (`joycast`) exposing server, client, trust manager, device scanner, and history modules with configurable paths.
  - Single CLI binary (`joycast`).

---

## Building

```bash
cargo build --release
```

The resulting binary will be placed at `./target/release/joycast`.

---

## Security & Authorization Workflow

When a client connects to a server for the first time, its connection is placed in **Pending** state until explicitly approved on the server.

### 1. View Pending Client Requests (on Server)

```bash
joycast server pending
```

**Output:**
```
Pending Client Authorization Requests:

  [1] Client ID: 8f3a9b1c...
      Hostname: my-laptop
      Device: Wireless Controller
      Transport: Iroh P2P
      First Seen: 2026-07-27T23:00:00Z
```

### 2. Approve a Client (on Server)

Approve using the full Client ID or a prefix:

```bash
joycast server approve 8f3a9b1c
```

### 3. List Authorized Clients (on Server)

```bash
joycast server approved
```

### 4. Revoke Client Access (on Server)

```bash
joycast server revoke 8f3a9b1c
```

---

## Usage (CLI)

### 1. Start the Server

```bash
joycast server
```

**Output:**
```
============================================================
Joycast Server is up and listening!
  Direct UDP Socket : 0.0.0.0:12398
  Connect via IP    : joycast client <SERVER_IP>:12398
  Iroh Node ID      : 0a2c73...  (STATIC)
  Connect via Iroh  : joycast client 0a2c73...
============================================================
```

#### Server Subcommands & Options:
- `joycast server` or `joycast server start`: Start server daemon.
- `joycast server pending`: List pending client connection requests.
- `joycast server approve <CLIENT_ID>`: Authorize a client ID or prefix.
- `joycast server approved`: List authorized clients.
- `joycast server revoke <CLIENT_ID>`: Revoke client authorization.
- `--bind <ADDR>`: Specify Direct UDP socket address (default: `0.0.0.0:12398`).
- `--secret-key <KEY>`: Hex secret key for static Iroh Node ID.
- `--key-file <PATH>`: Custom key file path (default: `~/.config/joycast/server.key` or `/etc/joycast/server.key`).
- `--no-udp`: Disable Direct UDP listener.
- `--no-iroh`: Disable Iroh P2P listener.

---

### 2. List Local Input Devices

```bash
joycast list
```

---

### 3. Start the Client

#### Connect via Iroh P2P:
```bash
joycast client 0a2c73... --device /dev/input/event3
```

#### Connect via Direct IP:
```bash
joycast client 192.168.1.50:12398 --device /dev/input/event3
```

#### Connect from Client's Local Saved History (Interactive Menu):
If no target argument is specified, `joycast client` shows an interactive menu of servers previously saved on the client machine:

```bash
joycast client
```

**Output:**
```
Select a previously connected server:
  [1] Hostname: gaming-desktop
      Target: 0a2c73...
      Transport: Iroh P2P
      Last Connected: 2026-07-27T23:00:00Z

Enter selection [1-1]: 1
```

#### View Client's Local Connection History:
```bash
joycast client --history
```

---

## Library Usage

Add `joycast` to your `Cargo.toml`:

```toml
[dependencies]
joycast = "0.2"
tokio = { version = "1", features = ["full"] }
```

### 1. Server-Side Library Example

Run a custom Joycast server with custom storage paths for the static server key and trusted clients file:

```rust
use anyhow::Result;
use joycast::server::{JoycastServer, ServerConfig};
use joycast::trust::TrustManager;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let custom_config_dir = PathBuf::from("/etc/myapp/joycast");

    // Initialize trust manager with a custom storage path for trusted clients
    let trust_manager = TrustManager::new(Some(custom_config_dir.join("trusted_clients.json")))?;

    // Programmatically authorize a specific client ID if desired
    // trust_manager.approve("8f3a9b1c")?;

    let config = ServerConfig {
        bind_addr: Some("0.0.0.0:12398".parse()?),
        secret_key: None,
        key_file: Some(custom_config_dir.join("server.key")),
        disable_udp: false,
        disable_iroh: false,
        trust_manager: Some(trust_manager),
    };

    let server = JoycastServer::bind(config).await?;
    if let Some(node_id) = server.iroh_node_id() {
        println!("Server listening with static Iroh Node ID: {}", node_id);
    }

    // Run server loop
    server.run().await?;
    Ok(())
}
```

---

### 2. Client-Side Library Example

Connect a client to a server using custom configuration directories for the client's local server history and client identity:

```rust
use anyhow::Result;
use joycast::client::{ClientConfig, JoycastClient};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let custom_data_dir = PathBuf::from("/var/lib/myapp");

    let config = ClientConfig {
        // Specify Iroh Node ID or Direct IP address string
        target: Some("192.168.1.50:12398".to_string()),
        // Path to physical input device (None for auto-detecting first gamepad)
        device_path: Some(PathBuf::from("/dev/input/event3")),
        // Custom path to store client's local server history
        history_file: Some(custom_data_dir.join("known_servers.json")),
        // Custom path to store persistent client identity ID
        client_id_file: Some(custom_data_dir.join("client_id")),
    };

    let client = JoycastClient::new(config)?;
    client.run().await?;
    Ok(())
}
```

---

### 3. Programmatic Device Scanning & Security Management

Query input devices, inspect pending client connection requests on the server, and query the client's local connection history:

```rust
use anyhow::Result;
use joycast::device::DeviceScanner;
use joycast::history::HistoryStore;
use joycast::trust::TrustManager;

fn main() -> Result<()> {
    // List available Linux input devices (/dev/input/event*)
    let devices = DeviceScanner::list_devices();
    for dev in &devices {
        println!("Found input device: {} at {}", dev.name, dev.path.display());
    }

    // Inspect pending connection requests on the server
    let trust = TrustManager::new(None)?;
    for pending in trust.list_pending() {
        println!("Pending client: {} ({})", pending.client_id, pending.hostname);
        // Approve client programmatically:
        // trust.approve(&pending.client_id)?;
    }

    // Inspect the client's local connection history (saved on client disk)
    let history = HistoryStore::new()?;
    for server in history.list_servers() {
        println!("Previously connected server: {} ({})", server.server_hostname, server.target);
    }

    Ok(())
}
```

---

## Permissions & uinput Setup

Creating virtual devices requires permissions to access `/dev/uinput` and read `/dev/input/event*`.

```bash
# Add current user to input and uinput groups
sudo usermod -aG input $USER

# Configure udev rule for uinput
echo 'KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"' | sudo tee /etc/udev/rules.d/99-uinput.rules

# Reload udev
sudo udevadm control --reload-rules && sudo udevadm trigger
```

---

## License

MIT License.
