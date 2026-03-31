//! CIP-26 token registry: download and parse decimals from GitHub repos.
//!
//! Mainnet: github.com/cardano-foundation/cardano-token-registry (mappings/)
//! Testnet: github.com/input-output-hk/metadata-registry-testnet (registry/)

use std::io::Read;
use std::time::Instant;
use tracing::{info, warn};

use crate::model::asset_fingerprint;

/// Registry config for a specific network.
pub struct RegistryConfig {
    pub owner: &'static str,
    pub repo: &'static str,
    pub branch: &'static str,
    pub dir: &'static str,
}

impl RegistryConfig {
    pub fn mainnet() -> Self {
        Self {
            owner: "cardano-foundation",
            repo: "cardano-token-registry",
            branch: "master",
            dir: "mappings",
        }
    }

    pub fn testnet() -> Self {
        Self {
            owner: "input-output-hk",
            repo: "metadata-registry-testnet",
            branch: "master",
            dir: "registry",
        }
    }

    fn tarball_url(&self) -> String {
        format!(
            "https://github.com/{}/{}/archive/refs/heads/{}.tar.gz",
            self.owner, self.repo, self.branch
        )
    }

    fn commit_api_url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/commits/{}",
            self.owner, self.repo, self.branch
        )
    }

    /// Expected path prefix inside the tarball (e.g., "cardano-token-registry-master/mappings/")
    fn tar_prefix(&self) -> String {
        format!("{}-{}/{}/", self.repo, self.branch, self.dir)
    }
}

/// Fetch the latest commit SHA for the registry branch.
pub async fn fetch_commit_sha(client: &reqwest::Client, config: &RegistryConfig) -> Option<String> {
    let resp = client
        .get(&config.commit_api_url())
        .header("User-Agent", "pool-pm-cardano")
        .header("Accept", "application/vnd.github.sha")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        warn!(
            status = %resp.status(),
            "failed to fetch registry commit SHA"
        );
        return None;
    }
    resp.text().await.ok().map(|s| s.trim().to_string())
}

/// Download and parse the token registry, returning (fingerprint, decimals) pairs.
/// Only returns entries where decimals > 0.
pub async fn fetch_decimals(
    client: &reqwest::Client,
    config: &RegistryConfig,
) -> Vec<(String, u8)> {
    let start = Instant::now();
    info!(
        "downloading CIP-26 token registry from {}/{}...",
        config.owner, config.repo
    );

    let resp = match client.get(&config.tarball_url()).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            warn!(status = %r.status(), "failed to download token registry tarball");
            return vec![];
        }
        Err(e) => {
            warn!(error = %e, "failed to download token registry tarball");
            return vec![];
        }
    };

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "failed to read token registry tarball");
            return vec![];
        }
    };

    // Decompress + parse tarball
    let gz = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(gz);
    let prefix = config.tar_prefix();
    let mut results = Vec::new();
    let mut file_count = 0u32;

    let entries = match archive.entries() {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "failed to read tarball entries");
            return vec![];
        }
    };

    for entry in entries {
        let mut entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = match entry.path() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };
        if !path.starts_with(&prefix) || !path.ends_with(".json") {
            continue;
        }
        file_count += 1;
        let mut content = String::new();
        if entry.read_to_string(&mut content).is_err() {
            continue;
        }
        if let Some((fp, d)) = parse_registry_entry(&content) {
            if d > 0 {
                results.push((fp, d));
            }
        }
    }

    info!(
        files = file_count,
        with_decimals = results.len(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "CIP-26 token registry loaded"
    );
    results
}

/// Parse a single registry JSON entry, returning (fingerprint, decimals).
fn parse_registry_entry(json: &str) -> Option<(String, u8)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let subject = v.get("subject")?.as_str()?;
    let decimals = v.get("decimals")?.get("value")?.as_u64()? as u8;

    // Subject = hex(policyId) + hex(assetName), policyId is 28 bytes = 56 hex chars
    if subject.len() < 56 {
        return None;
    }
    let policy_hex = &subject[..56];
    let name_hex = &subject[56..];
    let policy = hex::decode(policy_hex).ok()?;
    let name = hex::decode(name_hex).ok()?;
    let fp = asset_fingerprint(&policy, &name);
    Some((fp, decimals))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_registry_entry() {
        let json = r#"{
            "subject": "00000002df633853f6a47465c9496721d2d5b1291b8398016c0e87ae6e7574636f696e",
            "name": {"value": "nutcoin", "sequenceNumber": 0},
            "decimals": {"value": 6, "sequenceNumber": 0}
        }"#;
        let (fp, d) = parse_registry_entry(json).unwrap();
        assert_eq!(d, 6);
        assert!(fp.starts_with("asset"));
    }

    #[test]
    fn test_parse_registry_entry_no_decimals() {
        let json = r#"{
            "subject": "00000002df633853f6a47465c9496721d2d5b1291b8398016c0e87ae6e7574636f696e",
            "name": {"value": "nutcoin", "sequenceNumber": 0}
        }"#;
        assert!(parse_registry_entry(json).is_none());
    }
}
