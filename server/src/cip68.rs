//! CIP-68 reference token datum parsing for asset decimals.
//!
//! CIP-68 tokens use a two-asset system: a reference NFT (label 100) holds
//! metadata in its datum, and user tokens (label 333 FT, 444 RFT) live in
//! wallets. The datum structure is: Constr 0 [metadata_map, version, extra].
//! The metadata map may contain a "decimals" key with an integer value.

use pallas::crypto::hash::Hasher;
use pallas::ledger::primitives::alonzo::{BigInt, PlutusData};
use pallas::ledger::traverse::{MultiEraOutput, MultiEraTx};
use std::collections::HashMap;

use crate::model::asset_fingerprint;

/// CIP-67 label prefixes (4 bytes each)
pub const LABEL_100: [u8; 4] = [0x00, 0x06, 0x43, 0xb0]; // Reference NFT
pub const LABEL_222: [u8; 4] = [0x00, 0x0d, 0xe1, 0x40]; // User NFT
pub const LABEL_333: [u8; 4] = [0x00, 0x14, 0xdf, 0x10]; // User FT
pub const LABEL_444: [u8; 4] = [0x00, 0x1b, 0xc2, 0x80]; // User RFT

/// The four standard CIP-68 asset-name label prefixes.
const CIP68_LABELS: [[u8; 4]; 4] = [LABEL_100, LABEL_222, LABEL_333, LABEL_444];

/// True if the asset name begins with a standard CIP-68 (CIP-67) label prefix.
pub fn has_cip68_label(asset_name: &[u8]) -> bool {
    asset_name.len() >= 4 && CIP68_LABELS.iter().any(|l| asset_name[..4] == *l)
}

/// "decimals" as UTF-8 bytes
const DECIMALS_KEY: &[u8] = b"decimals";

/// Check if an asset name starts with the label 100 (reference NFT) prefix.
pub fn is_reference_token(asset_name: &[u8]) -> bool {
    asset_name.len() >= 4 && asset_name[..4] == LABEL_100
}

/// Extract the base name, stripping the 4-byte CIP-68 label prefix **only when one is
/// actually present**. A plain asset name that merely happens to be ≥4 bytes (e.g.
/// `unsig01037`) is returned unchanged — previously the first 4 bytes were always
/// dropped, mangling every non-CIP-68 name (`unsig01037` → `g01037`).
pub fn base_name(asset_name: &[u8]) -> &[u8] {
    if has_cip68_label(asset_name) {
        &asset_name[4..]
    } else {
        asset_name
    }
}

/// Compute the user FT (label 333) fingerprint for a reference token.
pub fn ft_fingerprint(policy_id: &[u8], ref_asset_name: &[u8]) -> String {
    let base = base_name(ref_asset_name);
    let mut ft_name = Vec::with_capacity(4 + base.len());
    ft_name.extend_from_slice(&LABEL_333);
    ft_name.extend_from_slice(base);
    asset_fingerprint(policy_id, &ft_name)
}

/// Compute the user RFT (label 444) fingerprint for a reference token.
pub fn rft_fingerprint(policy_id: &[u8], ref_asset_name: &[u8]) -> String {
    let base = base_name(ref_asset_name);
    let mut rft_name = Vec::with_capacity(4 + base.len());
    rft_name.extend_from_slice(&LABEL_444);
    rft_name.extend_from_slice(base);
    asset_fingerprint(policy_id, &rft_name)
}

/// Extract decimals from a PlutusData datum (CIP-68 format).
/// Expected structure: Constr 0 [metadata_map, version, extra]
/// where metadata_map contains key "decimals" → integer.
pub fn extract_decimals(datum: &PlutusData) -> Option<u8> {
    let constr = match datum {
        PlutusData::Constr(c) => c,
        _ => return None,
    };
    if constr.constr_index() != 0 {
        return None;
    }
    let metadata = constr.fields.first()?;
    extract_decimals_from_map(metadata)
}

/// Extract decimals from a PlutusData map by looking for key "decimals".
fn extract_decimals_from_map(metadata: &PlutusData) -> Option<u8> {
    let entries = match metadata {
        PlutusData::Map(kvs) => kvs,
        _ => return None,
    };
    for (key, value) in entries.iter() {
        if let PlutusData::BoundedBytes(k) = key {
            if k.as_slice() == DECIMALS_KEY {
                return plutus_int_to_u8(value);
            }
        }
    }
    None
}

/// Convert a PlutusData integer to u8.
fn plutus_int_to_u8(pd: &PlutusData) -> Option<u8> {
    if let PlutusData::BigInt(BigInt::Int(i)) = pd {
        let n: i128 = (*i).into();
        if (0..=255).contains(&n) {
            return Some(n as u8);
        }
    }
    None
}

type DatumHash = pallas::crypto::hash::Hash<32>;

/// Build a map of datum hashes to PlutusData from transaction witness data.
fn witness_datum_map<'a>(tx: &'a MultiEraTx<'_>) -> HashMap<DatumHash, &'a PlutusData> {
    tx.plutus_data()
        .iter()
        .map(|datum| (Hasher::<256>::hash(datum.raw_cbor()), &**datum))
        .collect()
}

/// Resolve the PlutusData datum for an output: inline datum or hash lookup.
fn resolve_datum<'a>(
    output: &MultiEraOutput<'_>,
    witness_datums: &'a HashMap<DatumHash, &'a PlutusData>,
) -> Option<DatumRef<'a>> {
    use pallas::ledger::primitives::conway::DatumOption;
    match output.datum() {
        Some(DatumOption::Data(data)) => Some(DatumRef::Owned(data.0.unwrap())),
        Some(DatumOption::Hash(hash)) => witness_datums.get(&hash).map(|pd| DatumRef::Ref(pd)),
        None => None,
    }
}

enum DatumRef<'a> {
    Owned(PlutusData),
    Ref(&'a PlutusData),
}

impl std::ops::Deref for DatumRef<'_> {
    type Target = PlutusData;
    fn deref(&self) -> &PlutusData {
        match self {
            DatumRef::Owned(d) => d,
            DatumRef::Ref(d) => d,
        }
    }
}

/// Scan a transaction's outputs for reference token assets and extract decimals.
/// Returns (fingerprint, decimals) pairs for both FT (333) and RFT (444) variants.
pub fn extract_from_tx(tx: &MultiEraTx<'_>) -> Vec<(String, u8)> {
    let witness_datums = witness_datum_map(tx);
    let mut results = Vec::new();
    for output in tx.outputs().iter() {
        // Quick check: does this output have any reference token?
        let has_ref_token = output
            .value()
            .assets()
            .iter()
            .any(|pa| pa.assets().iter().any(|a| is_reference_token(a.name())));
        if !has_ref_token {
            continue;
        }

        let datum = match resolve_datum(output, &witness_datums) {
            Some(d) => d,
            None => continue,
        };

        for policy_assets in output.value().assets().iter() {
            let policy_id = policy_assets.policy().as_ref().to_vec();
            for asset in policy_assets.assets().iter() {
                let name = asset.name();
                if !is_reference_token(name) {
                    continue;
                }
                let decimals = extract_decimals(&datum).unwrap_or(0);
                results.push((ft_fingerprint(&policy_id, name), decimals));
                results.push((rft_fingerprint(&policy_id, name), decimals));
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_reference_token() {
        assert!(is_reference_token(&[0x00, 0x06, 0x43, 0xb0, 0x01, 0x02]));
        assert!(!is_reference_token(&[0x00, 0x14, 0xdf, 0x10, 0x01]));
        assert!(!is_reference_token(&[0x00, 0x00]));
    }

    #[test]
    fn test_label_constants() {
        assert_eq!(LABEL_100, [0x00, 0x06, 0x43, 0xb0]);
        assert_eq!(LABEL_222, [0x00, 0x0d, 0xe1, 0x40]);
        assert_eq!(LABEL_333, [0x00, 0x14, 0xdf, 0x10]);
        assert_eq!(LABEL_444, [0x00, 0x1b, 0xc2, 0x80]);
    }

    #[test]
    fn base_name_strips_only_labeled() {
        // A CIP-68 label prefix (here user NFT 222 and reference 100) is stripped.
        assert_eq!(base_name(&[0x00, 0x0d, 0xe1, 0x40, b'A', b'B']), b"AB");
        assert_eq!(base_name(&[0x00, 0x06, 0x43, 0xb0, b'X']), b"X");
        // A plain (non-CIP-68) name is returned unchanged — regression for `unsig01037`,
        // which used to render as `g01037` when the first 4 bytes were always dropped.
        assert_eq!(base_name(b"unsig01037"), b"unsig01037");
        assert_eq!(base_name(b"abc"), b"abc");
    }
}
