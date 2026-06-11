//! Recognize on-chain oracle price-feed updates and decode them from an output's
//! inline datum. An oracle update is usually a self-transfer that re-creates a single
//! feed token at the same address with a fresh inline datum (the new price + validity
//! window) — so it looks like a plain transfer; the meaning is in the datum.
//!
//! **Adding another oracle:** register one `OracleDef { policy, decode }` in `ORACLES`
//! and write a `decode_*` for its datum schema. There is no oracle-datum standard, so
//! each protocol needs its own decoder (and its own price scale / pair, which aren't
//! self-describing on chain).

use pallas::ledger::primitives::{BigInt, PlutusData};
use pallas::ledger::traverse::MultiEraTx;

use crate::event::{OracleInfo, TxAnnotation};

/// A registered oracle: the policy of its feed token, and a decoder for its datum.
struct OracleDef {
    policy: [u8; 28],
    decode: fn(&PlutusData) -> Option<OracleInfo>,
}

/// AEGIS_PRICE_FEED_V1 policy (f0f14cd0…342301).
const AEGIS_POLICY: [u8; 28] = [
    0xf0, 0xf1, 0x4c, 0xd0, 0xdd, 0x1c, 0xae, 0x52, 0x39, 0x83, 0x60, 0xe3, 0xe4, 0x00, 0x13, 0x75,
    0x00, 0x00, 0x32, 0xcb, 0x39, 0x2c, 0xb3, 0xef, 0xeb, 0x34, 0x23, 0x01,
];
/// Implied decimals of the Aegis price integer. Best-effort (the scale isn't on chain);
/// change here if it's off. 164610 → "0.16461".
const AEGIS_PRICE_DECIMALS: usize = 6;
/// The Aegis feed prices ADA in USD, so the datum integer is USD per ADA. Neither the
/// pair nor the quote currency is on chain — both are properties of this feed.
const AEGIS_FEED: &str = "ADA/USD";
const AEGIS_QUOTE_SYMBOL: &str = "$";

const ORACLES: &[OracleDef] = &[OracleDef {
    policy: AEGIS_POLICY,
    decode: decode_aegis,
}];

/// If one of this tx's outputs carries a registered feed token and an inline datum the
/// matching protocol can decode, return the price update as a `TxAnnotation`. Gated on
/// the (cheap) policy match first, so only feed-bearing outputs are decoded.
pub fn extract_oracle(tx: &MultiEraTx<'_>) -> Option<TxAnnotation> {
    use pallas::ledger::primitives::conway::DatumOption;
    for output in tx.outputs() {
        let def = output
            .value()
            .assets()
            .iter()
            .flat_map(|pa| {
                let policy = pa.policy();
                ORACLES.iter().find(move |o| policy.as_ref() == o.policy)
            })
            .next();
        let Some(def) = def else { continue };
        if let Some(DatumOption::Data(data)) = output.datum().map(Into::into) {
            if let Some(info) = (def.decode)(&data.0) {
                return Some(TxAnnotation::Oracle(info));
            }
        }
    }
    None
}

/// Aegis datum: `Constr_0 [ Constr_2 [ { 0: price, 1: validFrom, 2: validUntil } ] ]`.
/// We read only the price (key 0); the validity window (keys 1/2, POSIX ms) isn't
/// surfaced.
fn decode_aegis(data: &PlutusData) -> Option<OracleInfo> {
    let inner = first_field(data)?; // Constr_0 → the Constr_2
    let map = first_field(inner)?; // Constr_2 → the map
    let price = map_int(map, 0)?;
    Some(OracleInfo {
        source: "Aegis".to_string(),
        feed: Some(AEGIS_FEED.to_string()),
        value: Some(format!(
            "{AEGIS_QUOTE_SYMBOL}{}",
            format_scaled(price, AEGIS_PRICE_DECIMALS)
        )),
    })
}

// --- PlutusData navigation helpers ---

/// First field of a `Constr`, if `data` is one.
fn first_field(data: &PlutusData) -> Option<&PlutusData> {
    match data {
        PlutusData::Constr(c) => c.fields.iter().next(),
        _ => None,
    }
}

/// Value at integer `key` in a `Map`, if `data` is a map keyed by integers.
fn map_int(data: &PlutusData, key: i128) -> Option<i128> {
    let PlutusData::Map(m) = data else {
        return None;
    };
    m.iter()
        .find(|(k, _)| as_int(k) == Some(key))
        .and_then(|(_, v)| as_int(v))
}

fn as_int(data: &PlutusData) -> Option<i128> {
    match data {
        PlutusData::BigInt(BigInt::Int(i)) => Some(i128::from(*i)),
        _ => None,
    }
}

/// Render a fixed-point integer with `decimals` implied decimal places, trimming
/// trailing fractional zeros (e.g. `164610, 6 -> "0.16461"`).
fn format_scaled(v: i128, decimals: usize) -> String {
    if decimals == 0 {
        return v.to_string();
    }
    let digits = format!("{:0>width$}", v.unsigned_abs(), width = decimals + 1);
    let (whole, frac) = digits.split_at(digits.len() - decimals);
    let frac = frac.trim_end_matches('0');
    let sign = if v < 0 { "-" } else { "" };
    if frac.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_formatting() {
        assert_eq!(format_scaled(164610, 6), "0.16461");
        assert_eq!(format_scaled(1_500_000, 6), "1.5");
        assert_eq!(format_scaled(42, 0), "42");
        assert_eq!(format_scaled(-2_340_000, 6), "-2.34");
        assert_eq!(format_scaled(0, 6), "0");
    }
}
