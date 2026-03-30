use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use oura::framework::ChainConfig;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

use crate::chain::Chain;

type HmacSha256 = Hmac<Sha256>;

/// Public test key for preprod (from https://nftcdn.io/doc)
const PREPROD_KEY: &str = "7FoxfBgV2k+RSz6UUts3/fG1edG7oIGXxdtIVCdalaI=";

#[derive(Clone)]
pub struct NftcdnConfig {
    pub subdomain: &'static str,
    key: Option<Vec<u8>>,
    client: reqwest::Client,
    decimals: Arc<DashMap<String, u8>>,
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

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build reqwest client");

        Self {
            subdomain,
            key,
            client,
            decimals: Arc::new(DashMap::new()),
        }
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

    fn compute_metadata_tk(&self, fingerprint: &str) -> Option<String> {
        let key = self.key.as_ref()?;
        let url = format!("https://{}.{}/metadata?tk=", fingerprint, self.subdomain);
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
        mac.update(url.as_bytes());
        Some(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
    }

    /// Get the number of decimals for an asset. Skips fetch for quantity 1
    /// (NFTs). Only caches non-zero results to keep the map tiny.
    pub async fn get_decimals(&self, fingerprint: &str, quantity: u64) -> u8 {
        if quantity == 1 {
            // Check cache in case this token was previously seen with quantity != 1
            return self.decimals.get(fingerprint).map(|e| *e).unwrap_or(0);
        }

        if let Some(entry) = self.decimals.get(fingerprint) {
            return *entry;
        }

        let mut url = format!("https://{}.{}/metadata", fingerprint, self.subdomain);
        if let Some(tk) = self.compute_metadata_tk(fingerprint) {
            url.push_str(&format!("?tk={}", tk));
        }

        let start = std::time::Instant::now();
        let decimals = match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let d = resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("decimals")?.as_u64())
                    .unwrap_or(0) as u8;
                info!(
                    fingerprint,
                    decimals = d,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "nftcdn metadata"
                );
                d
            }
            Ok(resp) if resp.status().is_server_error() => {
                error!(fingerprint, status = %resp.status(), "nftcdn metadata fetch failed");
                0
            }
            Ok(_) => 0,
            Err(e) => {
                error!(fingerprint, error = %e, "nftcdn metadata fetch error");
                0
            }
        };

        if decimals > 0 {
            self.decimals.insert(fingerprint.to_string(), decimals);
        }
        decimals
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
            client: reqwest::Client::new(),
            decimals: Arc::new(DashMap::new()),
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
            client: reqwest::Client::new(),
            decimals: Arc::new(DashMap::new()),
        };
        assert!(config.compute_tk("asset1xyz", "preview", 32).is_none());
    }
}
