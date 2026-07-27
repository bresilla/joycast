use anyhow::Result;
use clap::{Parser, Subcommand};
use joycast::{
    client::{ClientConfig, JoycastClient},
    device::DeviceScanner,
    history::HistoryStore,
    server::{JoycastServer, ServerConfig},
    trust::TrustManager,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "joycast",
    author,
    version,
    about = "Low-latency P2P & Direct IP gamepad forwarder with authorization security and connection history",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Server management and execution
    Server {
        #[command(subcommand)]
        subcommand: Option<ServerSubcommand>,

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
    /// Connect to a Joycast server and stream gamepad inputs
    Client {
        /// Server target: Direct IP (e.g. 192.168.1.50:12398) OR Iroh Node ID. Omit to pick from history.
        target: Option<String>,

        /// Path to input device (e.g. /dev/input/event3). If omitted, auto-detects first gamepad.
        #[arg(short, long)]
        device: Option<PathBuf>,

        /// Show list of previously connected servers
        #[arg(long)]
        history: bool,
    },
    /// List available local input devices (/dev/input/event*)
    List,
}

#[derive(Subcommand, Debug)]
enum ServerSubcommand {
    /// Start the server daemon (default action)
    Start,
    /// List pending client authorization requests
    Pending,
    /// Approve a client ID or prefix to grant access
    Approve {
        /// Client ID or prefix to approve
        client_id: String,
    },
    /// List all approved clients
    Approved,
    /// Revoke trust for a client ID or prefix
    Revoke {
        /// Client ID or prefix to revoke
        client_id: String,
    },
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
            subcommand,
            bind,
            secret_key,
            key_file,
            no_udp,
            no_iroh,
        } => {
            let tm = TrustManager::new(None)?;

            match subcommand {
                Some(ServerSubcommand::Pending) => {
                    let pending = tm.list_pending();
                    println!("Pending Client Authorization Requests:\n");
                    if pending.is_empty() {
                        println!("  No pending client requests.");
                    } else {
                        for (idx, p) in pending.iter().enumerate() {
                            println!(
                                "  [{}] Client ID: {}\n      Hostname: {}\n      Device: {}\n      Transport: {}\n      First Seen: {}\n",
                                idx + 1,
                                p.client_id,
                                p.hostname,
                                p.device_name,
                                p.transport,
                                p.first_seen
                            );
                        }
                        println!(
                            "To approve a client, run:\n  joycast server approve <CLIENT_ID>\n"
                        );
                    }
                }
                Some(ServerSubcommand::Approve { client_id }) => {
                    let approved = tm.approve(&client_id)?;
                    println!(
                        "Successfully authorized client '{}' ({})!",
                        approved.client_id, approved.hostname
                    );
                }
                Some(ServerSubcommand::Approved) => {
                    let list = tm.list_approved();
                    println!("Authorized Trusted Clients:\n");
                    if list.is_empty() {
                        println!("  No trusted clients.");
                    } else {
                        for (idx, a) in list.iter().enumerate() {
                            println!(
                                "  [{}] Client ID: {}\n      Hostname: {}\n      Device: {}\n      Approved At: {}\n",
                                idx + 1,
                                a.client_id,
                                a.hostname,
                                a.device_name,
                                a.approved_at
                            );
                        }
                    }
                }
                Some(ServerSubcommand::Revoke { client_id }) => {
                    tm.revoke(&client_id)?;
                    println!("Revoked trust for client matching '{}'.", client_id);
                }
                Some(ServerSubcommand::Start) | None => {
                    let config = ServerConfig {
                        bind_addr: bind.or_else(|| Some("0.0.0.0:12398".parse().unwrap())),
                        secret_key,
                        key_file,
                        disable_udp: no_udp,
                        disable_iroh: no_iroh,
                        trust_manager: Some(tm),
                    };
                    let server = JoycastServer::bind(config).await?;
                    server.run().await?;
                }
            }
        }
        Commands::Client {
            target,
            device,
            history,
        } => {
            if history {
                let store = HistoryStore::new()?;
                let list = store.list_servers();
                println!("Previously Connected Servers:\n");
                if list.is_empty() {
                    println!("  No server history found.");
                } else {
                    for (idx, s) in list.iter().enumerate() {
                        println!(
                            "  [{}] Hostname: {}\n      Target: {}\n      Transport: {}\n      Last Connected: {}\n",
                            idx + 1,
                            s.server_hostname,
                            s.target,
                            s.transport_type,
                            s.last_connected
                        );
                    }
                }
                return Ok(());
            }

            let client = JoycastClient::new(ClientConfig {
                target,
                device_path: device,
                ..Default::default()
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
