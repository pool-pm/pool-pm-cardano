use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use oura::framework::ChainConfig;
use sha2::Sha256;
use tracing::info;

use crate::chain::Chain;

type HmacSha256 = Hmac<Sha256>;

/// Public test key for preprod (from https://nftcdn.io/doc)
const PREPROD_KEY: &str = "7FoxfBgV2k+RSz6UUts3/fG1edG7oIGXxdtIVCdalaI=";

/// Asset thumbnail display cap in CSS px. Must match the `--thumb-size`
/// default / `thumbSize` base in `web/src/lib/components/Transaction.svelte`.
const THUMB_DISPLAY_MAX_PX: f64 = 96.0;

/// nftcdn.io only serves power-of-two image sizes — this keeps its CDN cache
/// hot (few keys per asset) and yields GPU-friendly POT textures. This ladder
/// covers the realistic `devicePixelRatio` range for a THUMB_DISPLAY_MAX_PX
/// thumbnail: 1x desktop (128), 2x retina / fractional scaling (256),
/// 3x phones / zoom (512). Tokens are precomputed once per asset upstream;
/// each client is served just the rung matching its DPR.
pub const SIZE_LADDER: [u16; 3] = [128, 256, 512];

/// Map a client `devicePixelRatio` to the smallest [`SIZE_LADDER`] rung that
/// fully covers a THUMB_DISPLAY_MAX_PX display at that ratio, clamped to the
/// top rung. Bogus ratios (≤0, NaN, non-finite) fall back to 1x.
pub fn rung_for_dpr(dpr: f64) -> u16 {
    let dpr = if dpr.is_finite() && dpr >= 1.0 {
        dpr
    } else {
        1.0
    };
    let needed = (THUMB_DISPLAY_MAX_PX * dpr).ceil() as u32;
    SIZE_LADDER
        .iter()
        .copied()
        .find(|&s| s as u32 >= needed)
        .unwrap_or_else(|| *SIZE_LADDER.last().unwrap())
}

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

    /// Build a fully signed NFTCDN URL. `path` is the endpoint including any
    /// trailing slash (e.g. `"metadata"`, `"files/0/"`, `"preview"`). `query` is
    /// extra params without a leading `&` (e.g. `"size=512"`) or `""` for none.
    /// `tk` is always the first query param and is signed with an empty value, as
    /// nftcdn.io requires (query-param order is significant). When no signing key
    /// is configured (preview network), the URL is returned unsigned.
    pub fn signed_url(&self, fingerprint: &str, path: &str, query: &str) -> String {
        let base = format!("https://{}.{}/{}", fingerprint, self.subdomain, path);
        let Some(key) = self.key.as_ref() else {
            return if query.is_empty() {
                base
            } else {
                format!("{}?{}", base, query)
            };
        };
        let to_sign = if query.is_empty() {
            format!("{}?tk=", base)
        } else {
            format!("{}?tk=&{}", base, query)
        };
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
        mac.update(to_sign.as_bytes());
        let tk = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        if query.is_empty() {
            format!("{}?tk={}", base, tk)
        } else {
            format!("{}?tk={}&{}", base, tk, query)
        }
    }

    /// Precompute signed tokens for every rung of [`SIZE_LADDER`]. Returns an
    /// empty vec when no signing key is configured (URLs are then unsigned).
    pub fn compute_ladder(&self, fingerprint: &str, path: &str) -> Vec<(u16, String)> {
        SIZE_LADDER
            .iter()
            .filter_map(|&size| {
                self.compute_tk(fingerprint, path, size)
                    .map(|tk| (size, tk))
            })
            .collect()
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
    fn test_signed_url_matches_known_token() {
        // Same fingerprint/size as test_compute_tk_preprod_image_example, so the
        // embedded tk must match that known-good token.
        let config = NftcdnConfig {
            subdomain: "preprod.nftcdn.io",
            key: Some(decode_key(PREPROD_KEY)),
        };
        let url = config.signed_url(
            "asset1cpfcfxay6s73xez8srvhf0pydtd9yqs8hyfawv",
            "image",
            "size=128",
        );
        assert_eq!(
            url,
            "https://asset1cpfcfxay6s73xez8srvhf0pydtd9yqs8hyfawv.preprod.nftcdn.io/image?tk=ZZ388CZwJhhLzm2djfRwaaPb8I_w7luNh5hOHJ2Ev4I&size=128"
        );
    }

    #[test]
    fn test_signed_url_unsigned_without_key() {
        let config = NftcdnConfig {
            subdomain: "preview.nftcdn.io",
            key: None,
        };
        assert_eq!(
            config.signed_url("asset1xyz", "metadata", ""),
            "https://asset1xyz.preview.nftcdn.io/metadata"
        );
        assert_eq!(
            config.signed_url("asset1xyz", "preview", "size=512"),
            "https://asset1xyz.preview.nftcdn.io/preview?size=512"
        );
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
