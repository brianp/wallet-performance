use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use tari_common::configuration::Network;
use tari_common_types::tari_address::TariAddress;
use tari_common_types::types::{CompressedPublicKey, PrivateKey};
use tari_crypto::keys::SecretKey;
use tari_crypto::tari_utilities::hex::Hex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressPoolEntry {
    pub index: usize,
    pub address_base58: String,
    pub view_key_hex: String,
    pub spend_key_hex: String,
}

pub struct AddressPool {
    addresses: Vec<String>,
    counter: AtomicUsize,
}

impl AddressPool {
    /// Generate `count` random Tari addresses for the given network.
    pub fn generate(
        count: usize,
        network: Network,
    ) -> anyhow::Result<(Self, Vec<AddressPoolEntry>)> {
        let mut entries = Vec::with_capacity(count);
        let mut addresses = Vec::with_capacity(count);

        for i in 0..count {
            let view_key = PrivateKey::random(&mut OsRng);
            let spend_key = PrivateKey::random(&mut OsRng);

            let view_pub = CompressedPublicKey::from_secret_key(&view_key);
            let spend_pub = CompressedPublicKey::from_secret_key(&spend_key);

            let address =
                TariAddress::new_dual_address_with_default_features(view_pub, spend_pub, network)
                    .context("Failed to create TariAddress")?;

            let base58 = address.to_base58();
            addresses.push(base58.clone());

            entries.push(AddressPoolEntry {
                index: i,
                address_base58: base58,
                view_key_hex: view_key.to_hex(),
                spend_key_hex: spend_key.to_hex(),
            });
        }

        Ok((
            Self {
                addresses,
                counter: AtomicUsize::new(0),
            },
            entries,
        ))
    }

    pub fn save_to_file(entries: &[AddressPoolEntry], path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(entries)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load_from_file(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        let entries: Vec<AddressPoolEntry> = serde_json::from_str(&data)?;
        let addresses = entries.into_iter().map(|e| e.address_base58).collect();
        Ok(Self {
            addresses,
            counter: AtomicUsize::new(0),
        })
    }

    /// Round-robin address selection.
    pub fn next_address(&self) -> &str {
        let idx = self.counter.fetch_add(1, Ordering::Relaxed) % self.addresses.len();
        &self.addresses[idx]
    }

    pub fn len(&self) -> usize {
        self.addresses.len()
    }
}
