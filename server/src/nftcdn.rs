use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use oura::framework::ChainConfig;
use sha2::Sha256;
use tracing::info;

use crate::chain::Chain;

type HmacSha256 = Hmac<Sha256>;

/// Public test key for preprod (from https://nftcdn.io/doc)
const PREPROD_KEY: &str = "7FoxfBgV2k+RSz6UUts3/fG1edG7oIGXxdtIVCdalaI=";

#[derive(Clone)]
pub struct NftcdnConfig {
    pub subdomain: &'static str,
    key: Option<Vec<u8>>,
}

fn decode_key(b64: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("invalid base64 in NFTCDN_KEY")
}

impl NftcdnConfig {
    pub fn new(network: &Chain) -> Self {
        let (subdomain, key) = match network.config() {
            ChainConfig::Mainnet => {
                let raw = std::env::var("NFTCDN_KEY").expect("NFTCDN_KEY must be set for mainnet");
                ("poolpm.nftcdn.io", Some(decode_key(&raw)))
            }
            ChainConfig::PreProd => ("preprod.nftcdn.io", Some(decode_key(PREPROD_KEY))),
            ChainConfig::Preview => ("preview.nftcdn.io", None),
            _ => ("preview.nftcdn.io", None),
        };

        info!(subdomain, signed = key.is_some(), "nftcdn config");

        Self { subdomain, key }
    }

    pub fn compute_tk(&self, fingerprint: &str, path: &str, size: u16) -> Option<String> {
        let key = self.key.as_ref()?;
        let url = format!(
            "https://{}.{}/{}?tk=&size={}",
            fingerprint, self.subdomain, path, size
        );
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
        mac.update(url.as_bytes());
        Some(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_tk_preprod_image_example() {
        // Example from https://nftcdn.io/doc
        let config = NftcdnConfig {
            subdomain: "preprod.nftcdn.io",
            key: Some(decode_key(PREPROD_KEY)),
        };
        let tk = config
            .compute_tk("asset1cpfcfxay6s73xez8srvhf0pydtd9yqs8hyfawv", "image", 128)
            .unwrap();
        assert_eq!(tk, "ZZ388CZwJhhLzm2djfRwaaPb8I_w7luNh5hOHJ2Ev4I");
    }

    #[test]
    fn test_compute_tk_none_without_key() {
        let config = NftcdnConfig {
            subdomain: "preview.nftcdn.io",
            key: None,
        };
        assert!(config.compute_tk("asset1xyz", "preview", 32).is_none());
    }
}
