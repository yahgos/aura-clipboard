use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Stable identifier of a device: the first 16 bytes of `blake3(noise static public key)`.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 16]);

impl DeviceId {
    pub fn from_public_key(public_key: &[u8]) -> Self {
        let hash = blake3::hash(public_key);
        let mut id = [0u8; 16];
        id.copy_from_slice(&hash.as_bytes()[..16]);
        Self(id)
    }

    /// Short form for logs and UI.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

/// Random identifier of a clipboard group. Also used as the Argon2 salt for the passphrase.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GroupId(pub [u8; 16]);

macro_rules! hex_id {
    ($t:ty) => {
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }

        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($t), hex::encode(&self.0[..4]))
            }
        }

        impl FromStr for $t {
            type Err = hex::FromHexError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let mut out = [0u8; 16];
                hex::decode_to_slice(s, &mut out)?;
                Ok(Self(out))
            }
        }
    };
}

hex_id!(DeviceId);
hex_id!(GroupId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let id = DeviceId::from_public_key(b"some key");
        let parsed: DeviceId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
        assert_eq!(id.short().len(), 8);
    }
}
