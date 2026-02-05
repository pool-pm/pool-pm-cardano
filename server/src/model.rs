use bech32::{Bech32, Hrp};
use pallas::crypto::hash::Hasher;
use sqlx::types::Decimal;

#[derive(sqlx::FromRow, Clone)]
pub struct Pool {
    pub hash_raw: Vec<u8>,
    pub vrf_key_hash: Vec<u8>,
    pub pledge: Decimal,
    pub margin: f64,
    pub fixed_cost: Decimal,
}

#[derive(Clone)]
pub struct TxOutput {
    pub lovelaces: Decimal,
    pub address: Vec<u8>,
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
