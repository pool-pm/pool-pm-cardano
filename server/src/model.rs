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
    /// Epoch at which the pool retires, if a retirement is pending and not cancelled by
    /// a later (re-)registration. The pool is active while this is `None` or still in
    /// the future. Maintained per block in `apply_block`.
    #[serde(default)]
    pub retiring_epoch: Option<i64>,
    /// Lifetime blocks minted by the pool. Seeded from db-sync, incremented per block in
    /// `apply_block`; carried across param updates (unlike `retiring_epoch`, which a
    /// re-registration resets). Rollbacks revert via snapshot history truncation.
    #[serde(default)]
    pub blocks: i64,
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
            // A (re-)registration cancels any pending retirement.
            retiring_epoch: None,
            // Lifetime count; the caller carries the prior value across param updates.
            blocks: 0,
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
    /// Epoch through which the DRep stays active (db-sync `drep_distr.active_until`,
    /// with `drepActivity` + dormancy applied). The DRep is active while this is
    /// `Some(e)` with `e >= current_epoch`; `None` = expired/deregistered. Seeded at
    /// reset, refreshed each epoch boundary in `apply_block` (a per-epoch ledger value,
    /// not cert-driven). Analogous to `Pool::retiring_epoch`.
    #[serde(default)]
    pub active_until: Option<i64>,
}

/// A stake credential's current delegation: the target it backs, plus the slot at which its
/// **current uninterrupted run** with that target began.
///
/// A re-delegation cert to the same target carries `since_slot` over — delegating again to the
/// pool/DRep you're already on doesn't reset the clock; only switching away, or a stake
/// deregistration followed by a return, restamps it. The slot (not the epoch) is stored so the
/// exact moment survives: the epoch is derived on demand with `State::epoch_for_slot` and the
/// wall-clock time with `slot_to_timestamp`.
///
/// `target` is a `Box<[u8]>` rather than a `Vec<u8>` deliberately: the hash is fixed-size, so
/// `Vec`'s capacity word is dead weight, and dropping it pays for `since_slot` exactly — the map
/// entry stays the same 48 bytes it was when the value was a bare `Vec<u8>` (24 B key + 24 B
/// value), over ~1.7M entries. Same reasoning as the boxed asset ids in `asset_holdings`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Delegation {
    /// Pool hash (28 B) or tagged DRep bytes (1 + 28 B). Persisted as msgpack `bin`; see
    /// `state::wire` for why that isn't the default.
    #[serde(with = "crate::state::wire_boxed_bytes")]
    pub target: Box<[u8]>,
    /// Slot of the first cert of the current run; 0 = unknown (pre-backfill snapshot).
    pub since_slot: u64,
}

impl Delegation {
    pub fn new(target: &[u8], since_slot: u64) -> Self {
        Self {
            target: target.into(),
            since_slot,
        }
    }
}

/// Governance votes cast by a DRep — the DRep-feed counterpart of `Pool::blocks` +
/// the pool's current-epoch block count.
///
/// Counted per **governance action, not per vote cast**: a DRep may vote on the same action
/// several times (the ledger keeps the latest), and 1,424 of mainnet's ~30k DRep votes are
/// such re-votes, so raw `voting_procedure` rows overstate participation. `actions` is
/// therefore the source of truth — one entry per action ever voted on — and the three
/// counters below are derived from it in [`DRepVotes::add`].
///
/// `epoch_votes` is stamped with the `epoch` it was counted in, so the per-epoch tally
/// resets itself on the first vote of a new epoch: no epoch-boundary sweep over every
/// DRep, and a reader compares the stamp with the current epoch (a stale stamp reads as
/// 0, exactly like a pool's `epoch_blocks`). Lives in `BlockSnapshot`, so rollbacks
/// revert it with the rest of the snapshot.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DRepVotes {
    /// Distinct governance actions this DRep has voted on (`actions.len()`).
    pub total: u64,
    /// Epoch `epoch_votes` was counted in.
    pub epoch: u64,
    /// Distinct actions voted on **during** `epoch` (a re-vote on an action already voted in
    /// an earlier epoch counts here — this is per-epoch activity, not new-action discovery).
    pub epoch_votes: u32,
    /// Governance actions this DRep could have voted on: those whose voting window overlapped
    /// its registered period. The denominator of the participation `%` on the DRep card, so a
    /// DRep registered after an action was decided isn't marked down for it. Refreshed from
    /// db-sync at startup and at each epoch boundary (actions are proposed every few days, so
    /// a mid-epoch proposal moves it by well under a point until then).
    #[serde(default)]
    pub eligible: u32,
    /// `"tx_hash#index"` of every action voted on → the epoch of the DRep's **latest** vote on
    /// it (0 for votes seeded from db-sync that predate the seeded epoch). Dedupes re-votes at
    /// the tip, where a db query is not an option. ~36 entries per voting DRep on mainnet.
    ///
    /// A trailing `#[serde(default)]` field, so snapshots written before it still load (they
    /// arrive with it empty, which is what triggers the db-sync repopulate that fills it).
    #[serde(default)]
    pub actions: imbl::HashMap<String, u64>,
}

impl DRepVotes {
    /// Votes cast in `epoch` — 0 unless the stamp matches (the DRep hasn't voted yet in it).
    pub fn votes_in(&self, epoch: u64) -> u32 {
        if self.epoch == epoch {
            self.epoch_votes
        } else {
            0
        }
    }

    /// Record a vote on `action` cast in `epoch`, then re-derive the counters from `actions`.
    /// Re-voting an action the DRep already voted on leaves `total` unchanged; the counters
    /// can't drift from `actions` because they're recomputed, not incremented.
    pub fn add(&mut self, action: &str, epoch: u64) {
        self.actions.insert(action.to_string(), epoch);
        self.total = self.actions.len() as u64;
        self.epoch = epoch;
        self.epoch_votes = self.actions.values().filter(|e| **e == epoch).count() as u32;
    }

    /// Build from a db-sync seed: `actions` (action → epoch of the latest vote, 0 if earlier
    /// than `epoch`) as of the snapshot's tx bound.
    pub fn from_actions(actions: imbl::HashMap<String, u64>, epoch: u64, eligible: u32) -> Self {
        let epoch_votes = actions.values().filter(|e| **e == epoch).count() as u32;
        Self {
            total: actions.len() as u64,
            epoch,
            epoch_votes,
            eligible,
            actions,
        }
    }
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

/// A UTXO's multi-assets, grouped by policy exactly like Pallas's `Multiasset`
/// (`policy → name → quantity`): each 28-byte policy id is stored once, with its
/// `(asset name, quantity)` entries. Binary throughout (raw policy / name bytes, not
/// hex) to keep the serialized snapshot compact; the CIP-14 fingerprint is derived on
/// demand via `asset_fingerprint(policy, name)` rather than stored.
pub type PolicyAssets = Vec<(Vec<u8>, Vec<(Vec<u8>, u64)>)>;

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TxOutput {
    pub lovelaces: Decimal,
    pub address: Vec<u8>,
    #[serde(default)]
    pub assets: PolicyAssets,
}

/// ADA Handle policy IDs (same on all networks).
pub const HANDLE_POLICIES: [[u8; 28]; 2] = [
    // Classic policy (f0ff48...)
    [
        0xf0, 0xff, 0x48, 0xbb, 0xb7, 0xbb, 0xe9, 0xd5, 0x9a, 0x40, 0xf1, 0xce, 0x90, 0xe9, 0xe9,
        0xd0, 0xff, 0x50, 0x02, 0xec, 0x48, 0xf2, 0x32, 0xb4, 0x9c, 0xa0, 0xfb, 0x9a,
    ],
    // DeMi policy (6c32db...)
    [
        0x6c, 0x32, 0xdb, 0x33, 0xa4, 0x22, 0xe0, 0xbc, 0x2c, 0xb5, 0x35, 0xbb, 0x85, 0x0b, 0x5a,
        0x6e, 0x9a, 0x95, 0x72, 0x22, 0x20, 0x56, 0xd6, 0xdd, 0xc9, 0xcb, 0xc2, 0x6e,
    ],
];

/// CIP-67 label prefixes (4 bytes each).
pub const CIP67_LABEL_000: &[u8] = &[0x00, 0x00, 0x00, 0x00]; // (000) Virtual SubHandle
pub const CIP67_LABEL_001: &[u8] = &[0x00, 0x00, 0x10, 0x70]; // (001) SubHandle root
pub const CIP67_LABEL_100: &[u8] = &[0x00, 0x06, 0x43, 0xb0]; // (100) CIP-68 reference NFT
pub const CIP67_LABEL_222: &[u8] = &[0x00, 0x0d, 0xe1, 0x40]; // (222) CIP-68 user NFT

/// Check if a policy ID is an ADA Handle policy.
pub fn is_handle_policy(policy_id: &[u8]) -> bool {
    HANDLE_POLICIES.iter().any(|p| p == policy_id)
}

/// Extract handle name from an asset name, if it's a handle token.
/// Returns Some((handle_name, is_virtual)) or None.
///
/// Known asset name formats under handle policies:
/// - Classic: plain UTF-8 handle name (no prefix)
/// - CIP-67 label 222: user NFT, prefix 000de140 + UTF-8 name → resolve to holder
/// - CIP-67 label 000: virtual subhandle, prefix 00000000 + UTF-8 name → resolve from datum
/// - CIP-67 label 001: subhandle root, prefix 00001070 → not a user handle, skip
/// - CIP-67 label 100: reference NFT, prefix 000643b0 → metadata only, skip
pub fn parse_handle_name(asset_name: &[u8]) -> Option<(String, bool)> {
    if asset_name.starts_with(CIP67_LABEL_222) {
        // CIP-68 user NFT (222): strip prefix, resolve to token holder
        parse_name_after_prefix(asset_name, false)
    } else if asset_name.starts_with(CIP67_LABEL_000) {
        // Virtual subhandle (000): strip prefix, needs datum resolution
        parse_name_after_prefix(asset_name, true)
    } else if asset_name.starts_with(CIP67_LABEL_001) || asset_name.starts_with(CIP67_LABEL_100) {
        // Subhandle root (001) or reference NFT (100): not user-facing
        None
    } else if asset_name.is_empty() {
        None
    } else if let Ok(s) = std::str::from_utf8(asset_name) {
        // Classic handle: plain UTF-8, no CIP-67 prefix
        Some((s.to_string(), false))
    } else {
        tracing::warn!(
            name_hex = hex::encode(asset_name),
            "unexpected handle asset name"
        );
        None
    }
}

fn parse_name_after_prefix(asset_name: &[u8], is_virtual: bool) -> Option<(String, bool)> {
    std::str::from_utf8(&asset_name[4..])
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| (s.to_string(), is_virtual))
}

/// Parse the `resolved_addresses.ada` field from a virtual handle's inline datum bytes.
pub fn parse_virtual_handle_address(datum_bytes: &[u8]) -> Option<String> {
    use pallas::ledger::primitives::PlutusData;
    let data = pallas::codec::minicbor::decode::<PlutusData>(datum_bytes).ok()?;
    parse_virtual_handle_address_from_datum(&data)
}

/// Parse the `resolved_addresses.ada` field from a PlutusData datum.
/// The datum is Constr 0 [map, ...] where map contains "resolved_addresses"
/// → map with "ada" → address bytes.
pub fn parse_virtual_handle_address_from_datum(
    data: &pallas::ledger::primitives::PlutusData,
) -> Option<String> {
    use pallas::ledger::primitives::PlutusData;
    let fields = match data {
        PlutusData::Constr(c) => &c.fields,
        _ => return None,
    };
    // Search all map fields for "resolved_addresses" (can be in metadata or extra)
    for field in fields.iter() {
        let map = match field {
            PlutusData::Map(m) => m,
            _ => continue,
        };
        for (k, v) in map.iter() {
            let key_bytes: &[u8] = match k {
                PlutusData::BoundedBytes(b) => b.as_ref(),
                _ => continue,
            };
            if key_bytes != b"resolved_addresses" {
                continue;
            }
            let addr_map = match v {
                PlutusData::Map(m) => m,
                _ => return None,
            };
            for (ak, av) in addr_map.iter() {
                let addr_key: &[u8] = match ak {
                    PlutusData::BoundedBytes(b) => b.as_ref(),
                    _ => continue,
                };
                if addr_key != b"ada" {
                    continue;
                }
                let addr_bytes = match av {
                    PlutusData::BoundedBytes(b) => b.as_ref(),
                    _ => return None,
                };
                return pallas::ledger::addresses::Address::from_bytes(addr_bytes)
                    .ok()
                    .map(|a| a.to_string());
            }
        }
    }
    None
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

#[cfg(test)]
mod handle_tests {
    use super::*;

    // --- is_handle_policy ---

    #[test]
    fn test_classic_policy() {
        assert!(is_handle_policy(&HANDLE_POLICIES[0]));
    }

    #[test]
    fn test_demi_policy() {
        assert!(is_handle_policy(&HANDLE_POLICIES[1]));
    }

    #[test]
    fn test_unknown_policy() {
        assert!(!is_handle_policy(&[0u8; 28]));
    }

    // --- parse_handle_name ---

    #[test]
    fn test_classic_handle() {
        let (name, is_virtual) = parse_handle_name(b"my.cool.handle").unwrap();
        assert_eq!(name, "my.cool.handle");
        assert!(!is_virtual);
    }

    #[test]
    fn test_cip68_handle() {
        // 000de140 prefix + "my.cool.handle"
        let mut asset_name = vec![0x00, 0x0d, 0xe1, 0x40];
        asset_name.extend_from_slice(b"my.cool.handle");
        let (name, is_virtual) = parse_handle_name(&asset_name).unwrap();
        assert_eq!(name, "my.cool.handle");
        assert!(!is_virtual);
    }

    #[test]
    fn test_virtual_handle() {
        // 00000000 prefix + "john@my.cool.handle"
        let mut asset_name = vec![0x00, 0x00, 0x00, 0x00];
        asset_name.extend_from_slice(b"john@my.cool.handle");
        let (name, is_virtual) = parse_handle_name(&asset_name).unwrap();
        assert_eq!(name, "john@my.cool.handle");
        assert!(is_virtual);
    }

    #[test]
    fn test_reference_token_rejected() {
        // CIP-67 label 100 (000643b0): reference NFT, metadata only
        let mut asset_name = vec![0x00, 0x06, 0x43, 0xb0];
        asset_name.extend_from_slice(b"my.handle");
        assert!(parse_handle_name(&asset_name).is_none());
    }

    #[test]
    fn test_subhandle_root_rejected() {
        // CIP-67 label 1 (00001070): subhandle root, not user-facing
        let mut asset_name = vec![0x00, 0x00, 0x10, 0x70];
        asset_name.extend_from_slice(b"adaprotocol");
        assert!(parse_handle_name(&asset_name).is_none());
    }

    #[test]
    fn test_empty_name_rejected() {
        assert!(parse_handle_name(b"").is_none());
    }

    #[test]
    fn test_cip68_empty_after_prefix() {
        let asset_name = vec![0x00, 0x0d, 0xe1, 0x40]; // just the prefix, no name
        assert!(parse_handle_name(&asset_name).is_none());
    }

    #[test]
    fn test_invalid_utf8_rejected() {
        assert!(parse_handle_name(&[0xff, 0xfe]).is_none());
    }

    // --- parse_virtual_handle_address ---

    #[test]
    fn test_parse_real_virtual_datum() {
        // Real datum from $old@2084 virtual handle
        let datum_hex = concat!(
            "d8799faf446e616d6549246f6c64403230383445696d6167655838697066733a2f2f7a646a375756",
            "7073734a6e705844503644715856715154615472485147764171315276773434614442706347766d",
            "365764496d65646961547970654a696d6167652f6a706567426f6700496f675f6e756d6265720046",
            "72617269747946636f6d6d6f6e466c656e677468084a63686172616374657273476e756d62657273",
            "516e756d657269635f6d6f64696669657273404b68616e646c655f74797065517669727475616c5f",
            "73756268616e646c654776657273696f6e014a7375625f72617269747944726172654a7375625f6c",
            "656e677468034e7375625f63686172616374657273476c657474657273557375625f6e756d657269",
            "635f6d6f646966696572734001b6477669727475616ca24c657870697265735f74696d651b000001",
            "99304dc63c4b7075626c69635f6d696e7401527265736f6c7665645f616464726573736573a14361",
            "6461583901f348c50736ab21606fc3a67de7bc3bb82b3f74c774f827a247cd7eafd110803451b99f",
            "c710e079af17f5e7e95e5eb850419de520bb9aefda4862675f696d6167655f5840697066733a2f2f",
            "6261667962656964636f756536763273687737686d7a777a6c6872326b7172626f7737776f797337",
            "6d6d6371666576666c366161363774366d426a69ff497066705f696d6167654046706f7274616c40",
            "4864657369676e65725838697066733a2f2f7a623272686e6134786f5442373663737939416f755a",
            "6f596144775766384247546a5a516274464e516f596d68316d743247736f6369616c73404676656e",
            "646f72404764656661756c74004e7374616e646172645f696d6167655838697066733a2f2f7a6232",
            "726858696d6a4d52524e346537654654537868354a6b5836676a5562374656756169385173455446",
            "4c3834666f4a536c6173745f7570646174655f61646472657373583901f348c50736ab21606fc3a6",
            "7de7bc3bb82b3f74c774f827a247cd7eafd110803451b99fc710e079af17f5e7e95e5eb850419de5",
            "20bb9aefda4c76616c6964617465645f6279581c4da965a049dfd15ed1ee19fba6e2974a0b79fc41",
            "6dd1796a1f97f5e14a696d6167655f686173685820b553d7143921c6f82db035ea70bbe26ddbf992",
            "1304a661741056af4e5df7d6dc537374616e646172645f696d6167655f6861736858201002472635",
            "2d46e46afb17690bf5960bf71a25e99110fc8b422344447308101d4b7376675f76657273696f6e45",
            "332e302e384c6167726565645f7465726d735768747470733a2f2f68616e646c652e6d652f242f74",
            "6f75546d6967726174655f7369675f726571756972656400446e7366770045747269616c004a707a",
            "5f656e61626c656401506c6173745f6564697465645f74696d651b00000191d8a074454862675f61",
            "737365745838d8f554a9d28c9c826236d629236b815250e1c44b9ef1cd20e08efa65001bc2805553",
            "54524120507265656d204261636b67726f756e642032ff",
        );
        let datum_bytes = hex::decode(datum_hex).unwrap();
        let addr = parse_virtual_handle_address(&datum_bytes).unwrap();
        assert!(addr.starts_with("addr1"));
        // Verify the resolved address matches the expected one
        assert_eq!(
            addr,
            "addr1q8e533g8x64jzcr0cwn8meau8wuzk0m5ca60sfazglxhat73zzqrg5denlr3pcre4utltelfte0ts5zpnhjjpwu6aldq07s2rm"
        );
    }

    #[test]
    fn test_parse_invalid_datum() {
        assert!(parse_virtual_handle_address(&[0xd8, 0x79, 0x80]).is_none());
    }

    #[test]
    fn test_parse_empty_datum() {
        assert!(parse_virtual_handle_address(&[]).is_none());
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    fn hexd(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    /// CIP-14 asset fingerprint, checked against the spec's official test vectors.
    #[test]
    fn cip14_asset_fingerprint_known_vectors() {
        let p1 = hexd("7eae28af2208be856f7a119668ae52a49b73725e326dc16579dcc373");
        assert_eq!(
            asset_fingerprint(&p1, b""),
            "asset1rjklcrnsdzqp65wjgrg55sy9723kw09mlgvlc3"
        );
        assert_eq!(
            asset_fingerprint(&p1, &hexd("504154415445")),
            "asset13n25uv0yaf5kus35fm2k86cqy60z58d9xmde92"
        );
        let p2 = hexd("1e349c9bdea19fd6c147626a5260bc44b71635f398b67c59881df209");
        assert_eq!(
            asset_fingerprint(&p2, b""),
            "asset1uyuxku60yqe57nusqzjx38aan3f2wq6s93f6ea"
        );
    }

    #[test]
    fn pool_and_drep_bech32_ids() {
        // Pool: bech32 `pool1…` round-tripping the 28-byte hash.
        let hash = vec![0xabu8; 28];
        let pool = pool_bech32_id(&hash);
        assert!(pool.starts_with("pool1"));
        let (hrp, data) = bech32::decode(&pool).unwrap();
        assert_eq!(hrp.as_str(), "pool");
        assert_eq!(data, hash);

        // DRep: tag 0x00 → `drep1…`, tag 0x01 → `drep_script1…`, plus the predefined ones.
        let key_drep = [&[0x00u8][..], &[0xd1u8; 28]].concat();
        assert!(drep_bech32_id(&key_drep).starts_with("drep1"));
        let script_drep = [&[0x01u8][..], &[0xd1u8; 28]].concat();
        assert!(drep_bech32_id(&script_drep).starts_with("drep_script1"));
        assert_eq!(drep_bech32_id(&[0x02]), "drep_always_abstain");
        assert_eq!(drep_bech32_id(&[0x03]), "drep_always_no_confidence");
    }

    /// Votes are counted per governance action: re-voting an action the DRep already voted on
    /// (allowed on-chain, the ledger keeps the latest) must not raise the total.
    #[test]
    fn revoting_the_same_action_does_not_raise_the_total() {
        let mut v = DRepVotes::default();
        v.add("a#0", 10);
        v.add("b#0", 10);
        assert_eq!((v.total, v.votes_in(10)), (2, 2));

        v.add("a#0", 10); // same action, same epoch
        assert_eq!((v.total, v.votes_in(10)), (2, 2));
    }

    /// `epoch_votes` is per-epoch *activity*: an action first voted in an earlier epoch and
    /// re-voted now counts for the new epoch, while the distinct lifetime total doesn't move.
    /// The stamp makes the tally self-reset, so a past epoch reads 0 without a sweep.
    #[test]
    fn epoch_tally_counts_distinct_actions_voted_in_that_epoch() {
        let mut v = DRepVotes::default();
        v.add("a#0", 10);
        v.add("b#0", 10);

        v.add("a#0", 11); // re-vote in the next epoch
        assert_eq!(v.total, 2);
        assert_eq!(v.votes_in(11), 1);
        assert_eq!(
            v.votes_in(10),
            0,
            "the tally follows the current epoch only"
        );

        v.add("c#0", 11);
        assert_eq!((v.total, v.votes_in(11)), (3, 2));
    }

    /// `eligible` and `actions` are **trailing** `#[serde(default)]` fields, so a snapshot
    /// written before them (rmp-serde encodes a struct as an array, so it holds only the first
    /// three) still loads — which is what keeps this change from forcing a cold reset. The empty
    /// `actions` is the signal `populate_drep_vote_counts` looks for to re-seed from db-sync.
    #[test]
    fn loads_a_snapshot_written_before_the_new_fields() {
        let old = rmp_serde::to_vec(&(7u64, 646u64, 2u32)).unwrap();
        let v: DRepVotes = rmp_serde::from_slice(&old).unwrap();
        assert_eq!((v.total, v.epoch, v.epoch_votes), (7, 646, 2));
        assert_eq!(v.eligible, 0);
        assert!(v.actions.is_empty());
    }

    /// A db-sync seed carries the actions voted before the seeded epoch with epoch 0, so only
    /// the current epoch's votes land in `epoch_votes`.
    #[test]
    fn from_actions_derives_the_counters() {
        let actions: imbl::HashMap<String, u64> = [
            ("a#0".to_string(), 0u64),
            ("b#0".to_string(), 0),
            ("c#0".to_string(), 646),
        ]
        .into_iter()
        .collect();
        let v = DRepVotes::from_actions(actions, 646, 151);
        assert_eq!(v.total, 3);
        assert_eq!(v.votes_in(646), 1);
        assert_eq!(v.eligible, 151);
    }
}
