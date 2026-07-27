use serde::Serialize;

mod string {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}

mod string_i64 {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}

mod opt_string_i64 {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => serializer.serialize_str(&v.to_string()),
            None => serializer.serialize_none(),
        }
    }
}

#[derive(Clone, Serialize)]
pub struct BlockTx {
    pub hash: String,
    #[serde(with = "string")]
    pub fee: u64,
    pub size: usize,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutputInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub delegations: Vec<DelegationInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub votes: Vec<VoteInfo>,
    /// Ordered metadata display lines: CIP-20 message text plus a badge per other
    /// metadata label (Catalyst registration, "metadata N"). See `extract_tx_metadata`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Vec<String>>,
    /// Net stake change in lovelace for pool feed stake-change blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    #[serde(with = "opt_string_i64")]
    pub stake_change: Option<i64>,
    /// The feed subject's delegator stake address(es) this tx actually moved — the
    /// relevant account(s) among possibly many in a multi-party tx. Set on pool/DRep
    /// stake-change txs (non-delegation ones); the folded view shows these instead of the
    /// raw payment addresses. Empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stake_addresses: Vec<String>,
    /// CIP-36/CIP-15 Catalyst voting registration (label 61284), if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalyst: Option<CatalystInfo>,
    /// Recognized protocol-specific descriptions of this tx (oracle price updates,
    /// and in future DEX swaps, lending actions, …). Kept behind a `Vec` so adding a
    /// protocol never grows `BlockTx`. See `TxAnnotation`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<TxAnnotation>,
    /// Pre-extracted stake credentials from input/output addresses.
    #[serde(skip)]
    pub stake_credentials: Vec<Vec<u8>>,
    /// Withdrawals: (stake_credential, lovelace). Used for stake_change computation.
    #[serde(skip)]
    pub withdrawals: Vec<(Vec<u8>, u64)>,
}

/// A protocol-specific description of a tx, recognized by a decoder. Serialized
/// internally-tagged (`{ "kind": "oracle", … }`) so the frontend can render each by
/// `kind`. Add a protocol by adding a variant plus the decoder that produces it — no
/// new `BlockTx` field, no growth of the `Event` enum.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TxAnnotation {
    Oracle(OracleInfo),
}

/// A recognized on-chain oracle price-feed update, decoded from an output's inline
/// datum (gated on a known feed-token policy). Protocol-specific decoders fill this
/// in — see `oracle.rs`.
#[derive(Clone, Serialize)]
pub struct OracleInfo {
    /// Protocol name, e.g. "Aegis".
    pub source: String,
    /// The priced pair as `BASE/QUOTE` (e.g. "ADA/USD"); the client reads the base to
    /// render "1 BASE = value". `None` when the pair isn't known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    /// Display-formatted price including the quote-currency symbol (e.g. "$0.16461").
    /// The decoder applies the protocol's scale and symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Catalyst (CIP-36/CIP-15) voting registration: the registrant's stake address
/// (derived from the registered stake key) and, on a stake feed, its live stake at
/// the registration block (filled by the backward walk; `None` elsewhere).
#[derive(Clone, Serialize)]
pub struct CatalystInfo {
    pub stake_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "opt_string_i64")]
    pub live_stake: Option<i64>,
}

#[derive(Clone, Serialize)]
pub struct DelegationInfo {
    pub stake_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_drep_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_drep_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_drep_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_drep_name: Option<String>,
    #[serde(with = "string_i64")]
    pub live_stake: i64,
}

#[derive(Clone, Serialize)]
pub struct VoteInfo {
    pub voter_role: String,
    pub voter_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voter_name: Option<String>,
    pub vote: String,
    pub action_tx_hash: String,
    pub action_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_title: Option<String>,
}

/// One row in a per-epoch REWARDS capsule. `label` is the db-sync reward `type`
/// (`member`/`leader`/`reserves`/`treasury`/…); pool rewards (`member`/`leader`)
/// also carry the source pool so the client can color the ticker.
#[derive(Clone, Serialize)]
pub struct RewardRow {
    pub label: String,
    #[serde(with = "string")]
    pub amount: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_ticker: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    MempoolTx(BlockTx),
    Block {
        slot: u64,
        hash: String,
        number: u64,
        timestamp: u64,
        /// Serialized block size in bytes — the folded pool-own block on a feed shows this
        /// (as KB) and scales its box to it. The whole-block size even when `txs` is filtered.
        size: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        pool_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pool_ticker: Option<String>,
        txs: Vec<BlockTx>,
    },
    Rollback {
        slot: u64,
    },
    MempoolPrune {
        removed: Vec<String>,
    },
    /// Per-epoch staking rewards on a stake feed, positioned at the epoch-change
    /// (spendable-epoch boundary) `slot`/`timestamp`. One event per epoch; `rows`
    /// holds every reward source for that epoch (pool + treasury/reserves).
    Reward {
        epoch: u64,
        slot: u64,
        timestamp: u64,
        rows: Vec<RewardRow>,
    },
    /// Emitted once at the end of a replay so the client can paginate older history.
    /// Stake/address feeds: `slot` = oldest replayed block; `stake`/`epoch` = the
    /// pre-block stake-walk anchor for the next page (absent on address feeds).
    /// Pool/DRep feeds carry none of those — an empty marker that enables scrolling;
    /// pagination pages from the tip by per-source keyset id and dedups the overlap.
    ReplayCursor {
        #[serde(skip_serializing_if = "Option::is_none")]
        slot: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        epoch: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(with = "opt_string_i64")]
        #[serde(default)]
        stake: Option<i64>,
    },
}

#[derive(Clone, Serialize)]
pub struct TxInput {
    pub tx_hash: String,
    pub index: i16,
    pub address: Option<String>,
    #[serde(with = "string")]
    pub lovelace: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<AssetInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct TxOutputInfo {
    pub address: String,
    #[serde(with = "string")]
    pub lovelace: u64,
    pub assets: Vec<AssetInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct AssetInfo {
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub quantity: String,
    /// Precomputed signed-token ladder `(size, tk)`, one entry per power-of-two
    /// rung (see `nftcdn::SIZE_LADDER`). Internal only: collapsed to `tk`/`size`
    /// per client during DPR negotiation, so it never reaches the wire.
    #[serde(skip)]
    pub tks: Vec<(u16, String)>,
    /// Per-client resolved token for the negotiated `size` (when signing is on).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tk: Option<String>,
    /// Per-client resolved image size — the `SIZE_LADDER` rung matching the
    /// client's `devicePixelRatio`. Filled before serialization.
    pub size: u16,
}

/// Build display `AssetInfo`s from a UTXO's binary policy-grouped assets
/// ([`crate::model::PolicyAssets`]), deriving the CIP-14 fingerprint from policy+name
/// and decoding the (UTF-8) asset name. `decimals_of` / `ladder_of` are resolved per
/// fingerprint by the caller (the snapshot's `decimals` map and the nftcdn ladder).
/// Shared by the mempool and the block input-resolution paths so a resolved input shows
/// the same asset detail as a freshly-decoded output (including the name, which the old
/// fingerprint-only `TxOutput` couldn't provide).
pub fn policy_assets_to_info(
    assets: &crate::model::PolicyAssets,
    mut decimals_of: impl FnMut(&str) -> u8,
    mut ladder_of: impl FnMut(&str) -> Vec<(u16, String)>,
) -> Vec<AssetInfo> {
    let mut out = Vec::new();
    for (policy, tokens) in assets {
        for (name, qty) in tokens {
            let fingerprint = crate::model::asset_fingerprint(policy, name);
            let decimals = decimals_of(&fingerprint);
            let tks = ladder_of(&fingerprint);
            let name = std::str::from_utf8(name)
                .ok()
                .filter(|s| !s.is_empty())
                .map(String::from);
            out.push(AssetInfo {
                fingerprint,
                name,
                quantity: format_quantity(*qty as u128, decimals),
                tks,
                tk: None,
                size: 0,
            });
        }
    }
    out
}

/// Format a raw on-chain quantity with the given number of decimals.
/// E.g. `format_quantity(1500000, 6)` → `"1.5"`, `format_quantity(100, 0)` → `"100"`.
pub fn format_quantity(raw: u128, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    // String-based (not rust_decimal, whose ~7.9e28 ceiling is below u128): shift the
    // decimal point `decimals` places from the right, trimming trailing fractional zeros.
    let dec = decimals as usize;
    let digits = raw.to_string();
    let padded = if digits.len() <= dec {
        format!("{digits:0>width$}", width = dec + 1) // ensure ≥1 integer digit
    } else {
        digits
    };
    let cut = padded.len() - dec;
    let int_part = &padded[..cut];
    let frac = padded[cut..].trim_end_matches('0');
    if frac.is_empty() {
        int_part.to_string()
    } else {
        format!("{int_part}.{frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_quantity() {
        assert_eq!(format_quantity(100, 0), "100");
        assert_eq!(format_quantity(1500000, 6), "1.5");
        assert_eq!(format_quantity(1000000, 6), "1");
        assert_eq!(format_quantity(500, 6), "0.0005");
        assert_eq!(format_quantity(0, 6), "0");
        assert_eq!(format_quantity(123456789, 6), "123.456789");
        assert_eq!(format_quantity(1230000, 6), "1.23");
        assert_eq!(format_quantity(10, 2), "0.1");
        assert_eq!(format_quantity(1, 8), "0.00000001");
    }
}
