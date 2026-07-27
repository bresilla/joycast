use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"joycast/v1";

/// Detailed information about an absolute axis (e.g. analog sticks, triggers).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AbsAxisWire {
    pub code: u16,
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// Metadata describing a physical input device to be replicated as a uinput device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceMetadata {
    pub name: String,
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub keys: Vec<u16>,
    pub abs_axes: Vec<AbsAxisWire>,
    pub rel_axes: Vec<u16>,
}

/// Serialized input event (type, code, value).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventWire {
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

/// Status of the handshake acknowledgment from server.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AckStatus {
    Approved,
    PendingApproval,
    Rejected,
}

/// Payload sent by client during handshake.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandshakePayload {
    pub client_id: String,
    pub client_hostname: String,
    pub metadata: DeviceMetadata,
}

/// Messages sent between Joycast client and server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Message {
    /// Sent by client upon connection to describe the client and input device.
    Handshake(HandshakePayload),
    /// Sent by server in response to handshake.
    HandshakeAck {
        status: AckStatus,
        server_hostname: String,
        message: String,
    },
    /// Batch of input events forwarded from client to server.
    Events(Vec<EventWire>),
    /// Keep-alive ping.
    Ping,
    /// Keep-alive pong.
    Pong,
}

impl Message {
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_roundtrip() {
        let meta = DeviceMetadata {
            name: "Xbox Controller".into(),
            bustype: 3,
            vendor: 0x045e,
            product: 0x028e,
            version: 1,
            keys: vec![304, 305, 307, 308],
            abs_axes: vec![AbsAxisWire {
                code: 0,
                value: 128,
                minimum: 0,
                maximum: 255,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            }],
            rel_axes: vec![],
        };

        let payload = HandshakePayload {
            client_id: "client_123".into(),
            client_hostname: "my-laptop".into(),
            metadata: meta,
        };

        let msg = Message::Handshake(payload);
        let bytes = msg.encode().unwrap();
        let decoded = Message::decode(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }
}
