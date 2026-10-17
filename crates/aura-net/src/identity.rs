use anyhow::{Result, anyhow};
use aura_core::DeviceId;
use serde::{Deserialize, Serialize};

pub(crate) const NOISE_XX: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
pub(crate) const NOISE_XX_PSK3: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

/// A device's long-term X25519 static keypair, used as its Noise identity.
#[derive(Clone, Serialize, Deserialize)]
pub struct Identity {
    pub private_key: [u8; 32],
    pub public_key: [u8; 32],
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let kp = snow::Builder::new(NOISE_XX.parse()?).generate_keypair()?;
        Ok(Self {
            private_key: kp
                .private
                .try_into()
                .map_err(|_| anyhow!("bad private key length"))?,
            public_key: kp
                .public
                .try_into()
                .map_err(|_| anyhow!("bad public key length"))?,
        })
    }

    pub fn device_id(&self) -> DeviceId {
        DeviceId::from_public_key(&self.public_key)
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("device_id", &self.device_id())
            .finish_non_exhaustive()
    }
}
