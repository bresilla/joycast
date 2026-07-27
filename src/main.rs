use anyhow::Result;
use clap::{Parser, Subcommand};
use joycast::{
    client::{ClientConfig, JoycastClient},
    device::DeviceScanner,
    server::{JoycastServer, ServerConfig},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "joycast",
    author,
    version,
    about = "Low-latency P2P & Direct IP gamepad and udev input device forwarder over iroh & UDP",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start Joycast server to receive input events over Direct UDP and/or Iroh P2P
    Server {
        /// Direct UDP bind address (e.g. 0.0.0.0:12398)
        #[arg(short, long, env = "JOYCAST_BIND")]
        bind: Option<SocketAddr>,

        /// Hex-encoded secret key to persist the server's Iroh Node ID
        #[arg(short, long, env = "JOYCAST_SECRET_KEY")]
        secret_key: Option<String>,

        /// Custom path to persistent secret key file (defaults to ~/.config/joycast/server.key)
        #[arg(short, long, env = "JOYCAST_KEY_FILE")]
        key_file: Option<PathBuf>,

        /// Disable Direct UDP IP listener
        #[arg(long)]
        no_udp: bool,

        /// Disable Iroh P2P listener
        #[arg(long)]
        no_iroh: bool,
    },
    /// Connect to a Joycast server (via Direct IP or Iroh Node ID) and stream gamepad inputs
    Client {
        /// Server target: Direct IP (e.g. 192.168.1.50:12398 or 127.0.0.1) OR Iroh Node ID
        target: String,

        /// Path to input device (e.g. /dev/input/event3). If omitted, auto-detects first gamepad.
        #[arg(short, long)]
        device: Option<PathBuf>,
    },
    /// List available local input devices (/dev/input/event*)
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing logger
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Server {
            bind,
            secret_key,
            key_file,
            no_udp,
            no_iroh,
        } => {
            let config = ServerConfig {
                bind_addr: bind.or_else(|| Some("0.0.0.0:12398".parse().unwrap())),
                secret_key,
                key_file,
                disable_udp: no_udp,
                disable_iroh: no_iroh,
            };
            let server = JoycastServer::bind(config).await?;
            server.run().await?;
        }
        Commands::Client { target, device } => {
            let client = JoycastClient::new(ClientConfig {
                target,
                device_path: device,
            })?;
            client.run().await?;
        }
        Commands::List => {
            println!("Available Linux input devices:\n");
            let devices = DeviceScanner::list_devices();
            if devices.is_empty() {
                println!(
                    "  No input devices found in /dev/input/event* (check permissions or udev rules)"
                );
            } else {
                for (idx, dev) in devices.iter().enumerate() {
                    println!(
                        "  [{}] Path: {}\n      Name: {}\n      Type: {}\n      Vendor: 0x{:04x}, Product: 0x{:04x}\n",
                        idx,
                        dev.path.display(),
                        dev.name,
                        dev.device_type,
                        dev.vendor,
                        dev.product
                    );
                }
            }
        }
    }

    Ok(())
}
