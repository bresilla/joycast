pub mod client;
pub mod device;
pub mod protocol;
pub mod server;
pub mod transport;

pub const ALPN: &[u8] = b"joycast/v1";

/// Returns this crate's display name.
pub fn name() -> &'static str {
    "joycast"
}
