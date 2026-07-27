use anyhow::{Result, bail};
use iroh::PublicKey;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

pub const DEFAULT_PORT: u16 = 12398;

/// Represents either a Direct IP socket address or an Iroh Node ID (PublicKey).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddress {
    Ip(SocketAddr),
    Iroh(PublicKey),
}

impl TargetAddress {
    /// Returns a human-readable display of the target.
    pub fn display_type(&self) -> &'static str {
        match self {
            TargetAddress::Ip(_) => "Direct IP",
            TargetAddress::Iroh(_) => "Iroh P2P",
        }
    }
}

impl std::fmt::Display for TargetAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetAddress::Ip(addr) => write!(f, "{}", addr),
            TargetAddress::Iroh(pk) => write!(f, "{}", pk),
        }
    }
}

impl FromStr for TargetAddress {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();

        // 1. Try parsing full SocketAddr (e.g., 192.168.1.50:12398 or [::1]:12398)
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(TargetAddress::Ip(addr));
        }

        // 2. Try parsing plain IpAddr with default port 12398 (e.g., 192.168.1.50)
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(TargetAddress::Ip(SocketAddr::new(ip, DEFAULT_PORT)));
        }

        // 3. Try parsing as Iroh Node ID (PublicKey)
        if let Ok(pk) = PublicKey::from_str(s) {
            return Ok(TargetAddress::Iroh(pk));
        }

        bail!(
            "Invalid target address '{}'. Must be either a valid IP address (e.g., 192.168.1.50:12398) or a valid iroh Node ID.",
            s
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_address_ip_parsing() {
        let ip_target: TargetAddress = "192.168.1.50:12398".parse().unwrap();
        assert_eq!(ip_target.display_type(), "Direct IP");

        let plain_ip: TargetAddress = "127.0.0.1".parse().unwrap();
        assert_eq!(
            plain_ip,
            TargetAddress::Ip("127.0.0.1:12398".parse().unwrap())
        );
    }
}
