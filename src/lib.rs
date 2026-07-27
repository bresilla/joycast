pub mod client;
pub mod device;
pub mod history;
pub mod protocol;
pub mod server;
pub mod service;
pub mod transport;
pub mod trust;

pub const ALPN: &[u8] = b"joycast/v1";

/// Returns this crate's display name.
pub fn name() -> &'static str {
    "joycast"
}
