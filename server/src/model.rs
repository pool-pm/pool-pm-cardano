use bech32::{Bech32, Hrp};
use pallas::crypto::hash::Hasher;
use sqlx::types::Decimal;

#[derive(sqlx::FromRow, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pool {
    pub hash_raw: Vec<u8>,
    pub pledge: Decimal,
    pub margin: f64,
    pub fixed_cost: Decimal,
    pub ticker: Option<String>,
}

impl Pool {
    pub fn from_registration(
        hash_raw: Vec<u8>,
        pledge: u64,
        cost: u64,
        margin_num: u64,
        margin_den: u64,
    ) -> Self {
        Pool {
            hash_raw,
            pledge: Decimal::from(pledge),
            margin: margin_num as f64 / margin_den as f64,
            fixed_cost: Decimal::from(cost),
            ticker: None,
        }
    }
}

pub fn pool_bech32_id(hash_raw: &[u8]) -> String {
    bech32::encode::<Bech32>(Hrp::parse("pool").unwrap(), hash_raw).unwrap()
}

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DRep {
    pub hash_bytes: Vec<u8>,
    pub given_name: Option<String>,
}

/// Convert DRep bytes (tag + hash) to a human-readable identifier.
pub fn drep_bech32_id(bytes: &[u8]) -> String {
    match bytes.first() {
        Some(0x00) => bech32::encode::<Bech32>(Hrp::parse("drep").unwrap(), &bytes[1..]).unwrap(),
        Some(0x01) => {
            bech32::encode::<Bech32>(Hrp::parse("drep_script").unwrap(), &bytes[1..]).unwrap()
        }
        Some(0x02) => "drep_always_abstain".to_string(),
        Some(0x03) => "drep_always_no_confidence".to_string(),
        _ => String::new(),
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TxOutput {
    pub lovelaces: Decimal,
    pub address: Vec<u8>,
    #[serde(default)]
    pub assets: Vec<(String, u64)>,
}

/// Compute CIP-14 asset fingerprint from policy_id and asset_name.
/// Returns bech32 string with "asset" HRP (e.g. "asset1...").
pub fn asset_fingerprint(policy_id: &[u8], asset_name: &[u8]) -> String {
    let mut data = Vec::with_capacity(policy_id.len() + asset_name.len());
    data.extend_from_slice(policy_id);
    data.extend_from_slice(asset_name);
    let hash = Hasher::<160>::hash(&data);
    bech32::encode::<Bech32>(Hrp::parse("asset").unwrap(), hash.as_ref()).unwrap()
}
