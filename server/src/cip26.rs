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
        .get(config.commit_api_url())
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

/// One registry entry worth keeping: its decimals, its ticker, or both.
pub struct RegistryEntry {
    pub fingerprint: String,
    /// Absent when the entry declares none, or declares 0 (which is the default anyway).
    pub decimals: Option<u8>,
    /// Only when it differs from the asset's own on-chain name — see [`parse_registry_entry`].
    pub ticker: Option<String>,
}

/// Download and parse the token registry. Returns only entries that say something the
/// chain doesn't: a non-zero decimals, or a ticker the asset name doesn't already give.
pub async fn fetch_registry(
    client: &reqwest::Client,
    config: &RegistryConfig,
) -> Vec<RegistryEntry> {
    let start = Instant::now();
    info!(
        "downloading CIP-26 token registry from {}/{}...",
        config.owner, config.repo
    );

    let resp = match client.get(config.tarball_url()).send().await {
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
        if let Some(entry) = parse_registry_entry(&content) {
            if entry.decimals.is_some() || entry.ticker.is_some() {
                results.push(entry);
            }
        }
    }

    info!(
        files = file_count,
        with_decimals = results.iter().filter(|e| e.decimals.is_some()).count(),
        with_ticker = results.iter().filter(|e| e.ticker.is_some()).count(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "CIP-26 token registry loaded"
    );
    results
}

/// Parse a single registry JSON entry.
///
/// The ticker is kept only when it differs from the name the asset already carries on
/// chain: of 7,504 registry tickers, 2,799 simply restate the asset name and storing
/// those would be dead weight, since the client falls back to that name anyway. What's
/// left is the part that earns its place — 1,722 genuine renames (`nutcoin` → `NUT`,
/// `SOLANA` → `SOL`) and, more importantly, 2,761 assets whose on-chain name is empty or
/// not text at all, where the ticker is the only name that exists.
///
/// A ticker differing from the name only by case is kept: that's the issuer's branding
/// (`DUCKSHILL` → `DuckShill`), and there are only ~222 of them.
fn parse_registry_entry(json: &str) -> Option<RegistryEntry> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let subject = v.get("subject")?.as_str()?;

    // Subject = hex(policyId) + hex(assetName), policyId is 28 bytes = 56 hex chars
    if subject.len() < 56 {
        return None;
    }
    let policy = hex::decode(&subject[..56]).ok()?;
    let name = hex::decode(&subject[56..]).ok()?;
    let fingerprint = asset_fingerprint(&policy, &name);

    let decimals = v
        .get("decimals")
        .and_then(|d| d.get("value"))
        .and_then(|d| d.as_u64())
        .map(|d| d as u8)
        .filter(|d| *d > 0);

    let on_chain = crate::model::display_asset_name(&name);
    let ticker = v
        .get("ticker")
        .and_then(|t| t.get("value"))
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .filter(|t| on_chain.as_deref() != Some(t))
        .map(String::from);

    Some(RegistryEntry {
        fingerprint,
        decimals,
        ticker,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `nutcoin` on chain; the entry declares decimals and no ticker.
    const NUTCOIN: &str = "00000002df633853f6a47465c9496721d2d5b1291b8398016c0e87ae6e7574636f696e";

    #[test]
    fn test_parse_registry_entry() {
        let json = format!(
            r#"{{"subject": "{NUTCOIN}",
                "name": {{"value": "nutcoin"}},
                "decimals": {{"value": 6}} }}"#
        );
        let e = parse_registry_entry(&json).unwrap();
        assert_eq!(e.decimals, Some(6));
        assert!(e.fingerprint.starts_with("asset"));
    }

    #[test]
    fn test_parse_registry_entry_no_decimals() {
        let json = format!(r#"{{"subject": "{NUTCOIN}", "name": {{"value": "nutcoin"}} }}"#);
        let e = parse_registry_entry(&json).unwrap();
        assert_eq!(e.decimals, None);
        assert_eq!(e.ticker, None);
    }

    /// A ticker the asset name already gives is dead weight — the client falls back to
    /// that name anyway. 2,799 of the registry's 7,504 tickers are this case.
    #[test]
    fn ticker_matching_the_asset_name_is_dropped() {
        let json = format!(r#"{{"subject": "{NUTCOIN}", "ticker": {{"value": "nutcoin"}} }}"#);
        assert_eq!(parse_registry_entry(&json).unwrap().ticker, None);
    }

    /// A rename is the whole point: `nutcoin` on chain, `NUT` to a reader.
    #[test]
    fn ticker_differing_from_the_asset_name_is_kept() {
        let json = format!(r#"{{"subject": "{NUTCOIN}", "ticker": {{"value": "NUT"}} }}"#);
        assert_eq!(
            parse_registry_entry(&json).unwrap().ticker,
            Some("NUT".to_string())
        );
    }

    /// Differing only in case is the issuer's branding, and there are ~222 of them.
    #[test]
    fn ticker_differing_only_by_case_is_kept() {
        let json = format!(r#"{{"subject": "{NUTCOIN}", "ticker": {{"value": "NutCoin"}} }}"#);
        assert_eq!(
            parse_registry_entry(&json).unwrap().ticker,
            Some("NutCoin".to_string())
        );
    }

    /// The most valuable case: an empty on-chain name, where the ticker is the only name
    /// the asset has. 2,761 registry entries are like this.
    #[test]
    fn ticker_is_kept_when_the_asset_has_no_readable_name() {
        // Subject is the 28-byte policy alone — the asset name is empty.
        let json = r#"{"subject": "00000002df633853f6a47465c9496721d2d5b1291b8398016c0e87ae",
                       "ticker": {"value": "SNEKBATCH"}}"#;
        assert_eq!(
            parse_registry_entry(json).unwrap().ticker,
            Some("SNEKBATCH".to_string())
        );
    }

    /// A CIP-67 label is not part of what a token is called, so the comparison has to
    /// see past it — otherwise `0014df10` + `WLK` looks unlike ticker `WLK` and gets
    /// stored to fix a name that was never broken.
    #[test]
    fn cip67_label_does_not_make_a_ticker_look_different() {
        // policy(28B) + 0014df10 + "WLK"
        let json = r#"{"subject": "00000002df633853f6a47465c9496721d2d5b1291b8398016c0e87ae0014df10574c4b",
                       "ticker": {"value": "WLK"}}"#;
        assert_eq!(parse_registry_entry(json).unwrap().ticker, None);
    }
}
