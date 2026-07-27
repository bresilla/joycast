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
- **Client Connection History & Auto-Selection**:
  - Remembers previously authorized servers (storing server hostname, address, and transport).
  - Running `joycast client` without arguments displays an interactive menu of saved servers.
- **Dynamic Device Mirroring**:
  - Reads client input capabilities (buttons, axes, fuzz, flat, resolution, vendor/product IDs).
  - Synthesizes an identical virtual `uinput` device on the server.
- **Modular Library & Single Binary**:
  - Library (`joycast`) exposing full server, client, trust manager, and history modules.
  - CLI binary (`joycast`).

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

#### Connect from Saved History (Interactive Menu):
If no target argument is specified, `joycast client` shows a menu of previously authorized servers:

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

#### View Connection History:
```bash
joycast client --history
```

---

## Library Usage

Add `joycast` to your `Cargo.toml`:

```toml
[dependencies]
joycast = "0.1"
tokio = { version = "1", features = ["full"] }
```

### Server-Side Library Example

```rust
use anyhow::Result;
use joycast::server::{JoycastServer, ServerConfig};
use joycast::trust::TrustManager;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize custom trust manager for client authorization
    let trust_manager = TrustManager::new(None)?;

    // Manually approve a trusted client ID programmatically if desired
    // trust_manager.approve("client_123")?;

    let config = ServerConfig {
        bind_addr: Some("0.0.0.0:12398".parse()?),
        secret_key: None,
        key_file: None,
        disable_udp: false,
        disable_iroh: false,
        trust_manager: Some(trust_manager),
    };

    let server = JoycastServer::bind(config).await?;
    println!("Server bound to Iroh Node ID: {:?}", server.iroh_node_id());

    // Run server event loop
    server.run().await?;
    Ok(())
}
```

### Client-Side Library Example

```rust
use anyhow::Result;
use joycast::client::{ClientConfig, JoycastClient};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let config = ClientConfig {
        // Pass Iroh Node ID or Direct IP address string
        target: Some("192.168.1.50:12398".to_string()),
        // Specify input device path or None for auto-detection
        device_path: Some(PathBuf::from("/dev/input/event3")),
    };

    let client = JoycastClient::new(config)?;
    client.run().await?;
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
