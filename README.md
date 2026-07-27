# Joycast

> High-performance, low-latency gamepad and udev input device forwarder over **Iroh P2P** and **Direct IP (UDP)**.

`joycast` lets you capture physical Linux input devices (gamepads, joysticks, controllers, keyboards, mice) on a local machine and forward their events in real time to a remote server. The server automatically creates a matching virtual `uinput` device on the host system, enabling seamless remote gaming and input redirection.

---

## Key Features

- **Dual Transport Support**:
  - **Iroh P2P**: Connects via encrypted, NAT-traversing hole-punching P2P tunnels using an Iroh `Node ID`. Works seamlessly across firewalls and the Internet without port forwarding.
  - **Direct IP (UDP)**: High-speed, ultra-low latency direct UDP forwarding for local LAN networks.
- **Dynamic Device Mirroring**:
  - Reads client input capabilities (buttons, axes, fuzz, flat, resolution, vendor/product IDs).
  - Automatically synthesizes an identical virtual `uinput` device on the server.
- **Library & CLI**:
  - Modular Rust library (`joycast`) for embedding into custom applications.
  - Single, full-fledged CLI binary (`joycast`).
- **Device Auto-Discovery**:
  - Built-in device scanner to list and classify `/dev/input/event*` devices (Gamepads, Joysticks, Keyboards, Mice).
  - Auto-selects active gamepad if no device path is specified.

---

## Building

```bash
cargo build --release
```

The resulting binary will be placed at `./target/release/joycast`.

---

## Usage

### 1. Start the Server

Run the `joycast server` command on the machine receiving input:

```bash
joycast server
```

**Output:**
```
============================================================
Joycast Server is up and listening!
  Direct UDP Socket : 0.0.0.0:12398
  Connect via IP    : joycast client <SERVER_IP>:12398
  Iroh Node ID      : 5x7ab...
  Connect via Iroh  : joycast client 5x7ab...
============================================================
```

#### Server Options:
- `--bind <ADDR>`: Specify Direct UDP socket address (default: `0.0.0.0:12398`).
- `--secret-key <KEY>`: Hex-encoded secret key to persist the server's Iroh Node ID.
- `--no-udp`: Disable Direct UDP listener.
- `--no-iroh`: Disable Iroh P2P listener.

---

### 2. List Local Input Devices

Run `joycast list` to inspect available input devices on your client machine:

```bash
joycast list
```

**Example Output:**
```
Available Linux input devices:

  [0] Path: /dev/input/event3
      Name: Wireless Controller (Xbox Wireless Controller)
      Type: Gamepad
      Vendor: 0x045e, Product: 0x028e
```

---

### 3. Start the Client

You can connect using **either** a Direct IP address or an Iroh `Node ID`:

#### Connect via Iroh P2P:
```bash
joycast client 5x7ab... --device /dev/input/event3
```

#### Connect via Direct IP (LAN):
```bash
joycast client 192.168.1.50:12398 --device /dev/input/event3
```

*(If `--device` is omitted, `joycast` automatically detects and picks the first available gamepad/joystick).*

---

## Permissions / uinput Setup

Creating virtual devices requires permissions to access `/dev/uinput` and read `/dev/input/event*`.

You can run with `sudo` or configure udev rules for non-root execution:

```bash
# Add current user to input and uinput groups
sudo usermod -aG input $USER

# Configure udev rule for uinput
echo 'KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"' | sudo tee /etc/udev/rules.d/99-uinput.rules

# Reload udev
sudo udevadm control --reload-rules && sudo udevadm trigger
```

---

## Library Usage

Add `joycast` to your `Cargo.toml`:

```toml
[dependencies]
joycast = { path = "../joycast" }
```

```rust
use joycast::client::{ClientConfig, JoycastClient};
use joycast::server::{JoycastServer, ServerConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Start Server
    let server = JoycastServer::bind(ServerConfig::default()).await?;
    tokio::spawn(async move { server.run().await });

    Ok(())
}
```

---

## License

MIT License.
