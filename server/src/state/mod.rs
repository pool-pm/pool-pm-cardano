mod dbsync;
pub mod feed_index;

use imbl::{hashmap::HashMap, hashset::HashSet, ordmap::OrdMap};
use std::path::Path;
use url::Url;

use crate::cip26;
use crate::model::{
    asset_fingerprint, parse_virtual_handle_address, DRep, Pool, TxOutput, HANDLE_POLICIES,
};
use crate::pallas::{stake_credential_from_address_bytes, PoolUpdate};
pub use dbsync::DbSync;
pub use feed_index::FeedIndex;

#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlockSnapshot {
    pub slot: u64,
    pub block_hash: Option<String>,
    pub last_epoch: Option<u64>,
    pub utxos: HashMap<(Vec<u8>, i16), TxOutput>,
    pub pools: HashMap<String, Pool>,
    pub pool_delegations: HashMap<Vec<u8>, Vec<u8>>,
    pub pool_delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    pub drep_delegations: HashMap<Vec<u8>, Vec<u8>>,
    pub drep_delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    pub stakes: HashMap<Vec<u8>, i64>,
    pub rewards: HashMap<Vec<u8>, i64>,
    /// DRep bytes → DRep metadata (given_name from off-chain vote data)
    pub dreps: HashMap<Vec<u8>, DRep>,
    /// Asset fingerprint → decimals (non-zero only, from CIP-26 + CIP-68)
    pub decimals: HashMap<String, u8>,
    /// ADA Handle: address → list of handle names owned
    #[serde(default)]
    pub handle_by_address: HashMap<String, Vec<String>>,
    /// ADA Handle: handle name → owner address
    #[serde(default)]
    pub address_by_handle: HashMap<String, String>,
    /// Governance action titles: "tx_hash#index" → title
    #[serde(default)]
    pub gov_action_titles: HashMap<String, String>,
    /// Live ADA balance per payment address (raw address bytes → lovelace).
    /// Entries with zero balance are removed; mirrors `stakes` at address level.
    /// Drives the live balance in the address feed header (replacing what used
    /// to be a one-shot db-sync query at connect).
    #[serde(default)]
    pub address_balances: HashMap<Vec<u8>, i64>,
    /// True iff `address_balances` was fully populated from db-sync (either by
    /// `reset()` or `populate_address_balances`). False on old snapshots and
    /// after a failed populate — in which case the sink will have added a
    /// *partial* set of entries during catch-up that would otherwise fool an
    /// `is_empty()` check and skip the re-populate.
    #[serde(default)]
    pub address_balances_populated: bool,
    /// Total live stake delegated to pools = Σ over `pool_delegations` of
    /// `stakes[cred] + rewards[cred]`. Maintained incrementally: exact at `reset()`,
    /// then adjusted in `apply_block` only when a pool delegation is added/removed (a
    /// stable delegator's balance drift between resets isn't re-summed — fine for the
    /// homepage % figure, re-synced on restart via `populate_total_staked`).
    #[serde(default)]
    pub total_staked: i64,
    /// Global per-address multi-asset holdings: a single **flat** map from a composite
    /// [`HeldKey`] = `((stake credential | None, payment address), policy ++ name)` to the
    /// held [`Qty`]. Flat (not nested per address/policy) because millions of tiny `imbl`
    /// sub-maps each waste a near-empty fixed-capacity chunk; one big map fills its chunks
    /// (~22 GB → ~3-4 GB on mainnet). The key sorts `(cred, addr, policy, name)`, so a
    /// stake's payment addresses are a contiguous prefix and one address's tokens are a
    /// sub-prefix — counts/grids/diffs are prefix range scans (see [`cred_range`] /
    /// [`addr_range`]). A key exists iff that token is currently held (`> 0`). Fully
    /// populated at `reset()` from db-sync, serialized into the snapshot (warm resume skips
    /// the populate), maintained per block by the sink, and — being a snapshot field —
    /// reverted automatically on rollback.
    #[serde(default)]
    pub asset_holdings: AssetHoldings,
    /// True iff `asset_holdings` was fully populated from db-sync (by `reset()` or
    /// `populate_asset_holdings`). False on snapshots saved before the field existed,
    /// so warm resume runs the one-time populate. Mirrors `address_balances_populated`.
    #[serde(default)]
    pub asset_holdings_populated: bool,
}

/// Key into [`BlockSnapshot::asset_holdings`]: `(stake credential | None for an
/// enterprise/pointer address, payment-address bytes)`. Ordered by credential first so
/// a stake's payment addresses are a contiguous range.
pub type AssetKey = (Option<Vec<u8>>, Vec<u8>);

/// A held token quantity. `u128`, because a per-address sum across UTXOs can exceed `u64`
/// (the ledger bounds a single output to `i64`, but several add up). MessagePack/rmp has
/// no 128-bit int, so it serializes as a *variable-length* value: a plain int when it fits
/// `u64` (the near-universal case — 1 byte for small amounts, fully back-compatible with
/// the old `u64` leaf), else a `(low, high)` pair. Arithmetic uses the inner `.0`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Qty(pub u128);

impl serde::Serialize for Qty {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match u64::try_from(self.0) {
            Ok(small) => s.serialize_u64(small),
            Err(_) => (self.0 as u64, (self.0 >> 64) as u64).serialize(s),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Qty {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct QtyVisitor;
        impl<'de> serde::de::Visitor<'de> for QtyVisitor {
            type Value = Qty;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a u64, or a (low, high) u64 pair for amounts exceeding u64")
            }
            // Narrower uint widths forward here via serde's defaults.
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Qty, E> {
                Ok(Qty(v as u128))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Qty, A::Error> {
                use serde::de::Error;
                let lo: u64 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("qty low"))?;
                let hi: u64 = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("qty high"))?;
                Ok(Qty(((hi as u128) << 64) | lo as u128))
            }
        }
        d.deserialize_any(QtyVisitor)
    }
}

/// A held token's asset id: `policy (28 bytes) ++ name`. Since `policy` is fixed-width the
/// packed bytes sort exactly as `(policy, name)` — and it's one allocation, not two.
pub type AssetId = Box<[u8]>;

/// Composite key of the flat [`AssetHoldings`] map: `((cred, addr), policy ++ name)`,
/// sorting `(cred, addr, policy, name)`.
pub type HeldKey = (AssetKey, AssetId);

/// Pack a token's policy + name into an [`AssetId`].
fn asset_id(policy: &[u8], name: &[u8]) -> AssetId {
    let mut v = Vec::with_capacity(policy.len() + name.len());
    v.extend_from_slice(policy);
    v.extend_from_slice(name);
    v.into_boxed_slice()
}

/// Split an [`AssetId`] back into `(policy, name)` (`policy` is always 28 bytes).
fn split_asset(id: &[u8]) -> (&[u8], &[u8]) {
    id.split_at(28)
}

impl BlockSnapshot {
    /// Distinct multi-assets currently held by one payment address — a count over the
    /// address's contiguous key prefix.
    pub fn address_asset_count(&self, address: &[u8]) -> u32 {
        let cred = stake_credential_from_address_bytes(address);
        addr_range(&self.asset_holdings, &(cred, address.to_vec())).count() as u32
    }

    /// Distinct multi-assets held across every payment address sharing a 28-byte stake
    /// credential — the union of asset ids over the credential's contiguous prefix range
    /// (the same asset on two addresses dedupes). In-memory (~ms even for a whale) vs the
    /// old `COUNT(DISTINCT)` db query (tens of seconds).
    pub fn stake_asset_count(&self, cred: &[u8]) -> u32 {
        let mut union: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
        for ((_, asset), _) in cred_range(&self.asset_holdings, cred) {
            union.insert(asset);
        }
        union.len() as u32
    }

    /// Every `(policy, name, quantity)` token currently held by a payment address — the
    /// rows the owned-assets grid renders, straight from memory (no db scan). Unsorted;
    /// the caller sorts and paginates.
    pub fn address_held_assets(&self, address: &[u8]) -> Vec<(Vec<u8>, Vec<u8>, u128)> {
        let cred = stake_credential_from_address_bytes(address);
        addr_range(&self.asset_holdings, &(cred, address.to_vec()))
            .map(|((_, asset), qty)| {
                let (policy, name) = split_asset(asset);
                (policy.to_vec(), name.to_vec(), qty.0)
            })
            .collect()
    }

    /// Distinct `(policy, name, quantity)` tokens held across every payment address
    /// sharing a stake credential — the same asset on two of the credential's addresses
    /// is one owned asset, with the quantities summed. Unsorted; the caller paginates.
    pub fn stake_held_assets(&self, cred: &[u8]) -> Vec<(Vec<u8>, Vec<u8>, u128)> {
        let mut sums: std::collections::HashMap<&[u8], u128> = std::collections::HashMap::new();
        for ((_, asset), qty) in cred_range(&self.asset_holdings, cred) {
            *sums.entry(asset).or_insert(0) += qty.0;
        }
        sums.into_iter()
            .map(|(asset, q)| {
                let (policy, name) = split_asset(asset);
                (policy.to_vec(), name.to_vec(), q)
            })
            .collect()
    }
}

/// Quantity of one `(policy, name)` token held by a single payment-address key (0 if not
/// held). For resolving a live tile delta's amount from the current snapshot.
pub fn address_token_qty(
    holdings: &AssetHoldings,
    key: &AssetKey,
    policy: &[u8],
    name: &[u8],
) -> u128 {
    holdings
        .get(&(key.clone(), asset_id(policy, name)))
        .map(|q| q.0)
        .unwrap_or(0)
}

/// Quantity of one `(policy, name)` token summed across every payment address sharing a
/// stake credential — the stake-level owned amount.
pub fn stake_token_qty(holdings: &AssetHoldings, cred: &[u8], policy: &[u8], name: &[u8]) -> u128 {
    let target = asset_id(policy, name);
    cred_range(holdings, cred)
        .filter_map(|((_, asset), q)| (asset == &target).then_some(q.0))
        .sum()
}

/// Process resident set size in MB (Linux `/proc/self/statm`, field 2 = resident pages),
/// 0 if unavailable. For coarse memory tracing — pair with entry counts below to see
/// which structure dominates and which `reset` step grows RSS the most.
pub fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map_or(0, |pages| pages * 4096 / (1024 * 1024))
}

impl BlockSnapshot {
    /// Log the entry count of every in-memory map plus current RSS. O(addresses) — skips
    /// the ~15M asset-holdings leaf walk (its leaf count is logged at build time). A rough
    /// per-entry byte estimate (`est_*_mb`) flags the dominant maps; RSS is the truth.
    pub fn log_sizes(&self, label: &str) {
        let mb = |n: usize, per: usize| (n.saturating_mul(per)) / (1024 * 1024);
        tracing::info!(
            label,
            rss_mb = rss_mb(),
            utxos = self.utxos.len(),
            stakes = self.stakes.len(),
            rewards = self.rewards.len(),
            address_balances = self.address_balances.len(),
            pool_delegations = self.pool_delegations.len(),
            pool_delegators = self.pool_delegators.len(),
            drep_delegations = self.drep_delegations.len(),
            drep_delegators = self.drep_delegators.len(),
            dreps = self.dreps.len(),
            pools = self.pools.len(),
            decimals = self.decimals.len(),
            handles = self.handle_by_address.len(),
            gov_titles = self.gov_action_titles.len(),
            asset_holding_addrs = self.asset_holdings.len(),
            // rough byte estimates (key+value+imbl node overhead), per map shape
            est_stakes_mb = mb(self.stakes.len(), 96),
            est_rewards_mb = mb(self.rewards.len(), 96),
            est_addr_bal_mb = mb(self.address_balances.len(), 104),
            est_pool_deleg_mb = mb(self.pool_delegations.len(), 136),
            est_drep_deleg_mb = mb(self.drep_delegations.len(), 137),
            "in-memory map sizes",
        );
    }
}

/// On-disk snapshot format version. Bump on any breaking change to a persisted field's
/// shape/semantics that rmp can't catch (it tolerates int-width changes) — a mismatch is
/// rejected on load so the state rebuilds from db-sync. v2: `asset_holdings` leaf went
/// from UTXO count to summed held quantity. v3: that leaf became a `u128` `Qty`. v4:
/// `asset_holdings` flattened to one `OrdMap<HeldKey, Qty>`. v5: force a rebuild to heal
/// `address_balances`/`asset_holdings` drift accumulated by the intra-block debit-drop bug
/// (produced now applied before consumed in `apply_block`).
const SNAPSHOT_FORMAT: u32 = 5;

/// Serialize `(snap, feed_index, magic, format)` and write it atomically (temp file +
/// rename, so a crash mid-write leaves the previous snapshot intact). Free fn so it can
/// run on a `spawn_blocking` thread from owned clones, without the `chain_state` lock or
/// `&self`. Returns the persisted slot.
///
/// Serializes **straight into a buffered file writer** rather than into one big `Vec<u8>`:
/// the whole-state buffer would be a multi-GB transient (~half the live set) stacked on top
/// of the live structures every `SNAPSHOT_INTERVAL` blocks — the dominant RSS spike. The
/// `BufWriter` keeps the in-flight buffer to its 8 KB capacity.
pub fn write_snapshot(
    path: &Path,
    snap: &BlockSnapshot,
    feed_index: &FeedIndex,
    network_magic: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut wr = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    rmp_serde::encode::write(&mut wr, &(snap, feed_index, network_magic, SNAPSHOT_FORMAT))?;
    wr.flush()?;
    wr.into_inner()?.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(snap.slot)
}

impl BlockSnapshot {
    /// Look up the shortest ADA Handle for an address, if any.
    pub fn handle_for(&self, address: &str) -> Option<String> {
        self.handle_by_address
            .get(address)
            .and_then(|handles| handles.iter().min_by_key(|h| h.len()).cloned())
    }
}

/// A `(policy bytes, name bytes)` token — the unit a live assets-grid tile is keyed by;
/// its CIP-14 fingerprint is derived on demand via `asset_fingerprint`.
pub type Token = (Vec<u8>, Vec<u8>);

/// The global per-address holdings map ([`BlockSnapshot::asset_holdings`]'s type). A
/// connection caches the previous block's handle (O(1)) and diffs the current one for
/// its live grid tiles.
pub type AssetHoldings = OrdMap<HeldKey, Qty>;

/// Apply a single token's `±qty` to one `(address, policy, name)` entry in the flat
/// holdings map. An entry is pruned when it hits 0, so the keys stay exactly the tokens
/// currently held. Live tile deltas are *not* emitted here — each open assets page derives
/// them by diffing snapshots; this only maintains the map.
fn bump_one(
    holdings: &mut AssetHoldings,
    key: &AssetKey,
    policy: &[u8],
    name: &[u8],
    qty: u128,
    add: bool,
) {
    let hkey = (key.clone(), asset_id(policy, name));
    if add {
        holdings.entry(hkey).or_default().0 += qty;
    } else {
        // produced/consumed amounts balance exactly; saturate as a guard against any stray
        // underflow rather than panic, and prune the entry at 0.
        let drop = match holdings.get_mut(&hkey) {
            Some(c) => {
                c.0 = c.0.saturating_sub(qty);
                c.0 == 0
            }
            None => false,
        };
        if drop {
            holdings.remove(&hkey);
        }
    }
}

/// Apply one UTXO's policy-grouped assets to the global holdings map (`add` = produced,
/// else consumed), computing the UTXO's stake credential once. Maintained for *every*
/// address; live tile deltas are derived per connection by diffing, not emitted here.
fn apply_utxo_assets(
    address: &[u8],
    assets: &crate::model::PolicyAssets,
    add: bool,
    holdings: &mut AssetHoldings,
) {
    if assets.is_empty() {
        return;
    }
    let cred = stake_credential_from_address_bytes(address);
    let key: AssetKey = (cred, address.to_vec());
    for (policy, names) in assets {
        for (name, qty) in names {
            bump_one(holdings, &key, policy, name, *qty as u128, add);
        }
    }
}

/// The credential's held tokens — the contiguous `(Some(cred), …)` key prefix of the flat
/// map, yielding `(&HeldKey, &Qty)`. Shared by the count/grid/diff helpers.
fn cred_range<'a>(
    holdings: &'a AssetHoldings,
    cred: &'a [u8],
) -> impl Iterator<Item = (&'a HeldKey, &'a Qty)> {
    let start: HeldKey = ((Some(cred.to_vec()), Vec::new()), Box::default());
    holdings
        .range(start..)
        .take_while(move |(((c, _), _), _)| c.as_deref() == Some(cred))
}

/// One payment address's held tokens — the contiguous `((cred, addr), …)` key prefix.
fn addr_range<'a>(
    holdings: &'a AssetHoldings,
    key: &'a AssetKey,
) -> impl Iterator<Item = (&'a HeldKey, &'a Qty)> {
    let start: HeldKey = (key.clone(), Box::default());
    holdings
        .range(start..)
        .take_while(move |((k, _), _)| k == key)
}

/// Live tile changes for one payment address between two holdings snapshots: `(added,
/// removed)` `(policy, name)` tokens. We walk the flat map's structural diff
/// (`prev.diff(curr)`, O(the block's actual changes) — shared subtrees skipped) and keep
/// only the changed keys whose `(cred, addr)` prefix is this subject. A key appearing is a
/// tile add, one disappearing (we prune at qty 0) is a remove; a qty change with the key
/// still present is no tile change. Whale-safe: only the block's moved tokens are visited.
pub fn address_tile_diff(
    prev: &AssetHoldings,
    curr: &AssetHoldings,
    key: &AssetKey,
) -> (Vec<Token>, Vec<Token>) {
    use imbl::ordmap::DiffItem;
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for d in prev.diff(curr) {
        match d {
            DiffItem::Add((k, asset), _) if k == key => {
                let (policy, name) = split_asset(asset);
                added.push((policy.to_vec(), name.to_vec()));
            }
            DiffItem::Remove((k, asset), _) if k == key => {
                let (policy, name) = split_asset(asset);
                removed.push((policy.to_vec(), name.to_vec()));
            }
            _ => {}
        }
    }
    (added, removed)
}

/// Live tile changes for a stake credential (the union over its addresses) between two
/// holdings snapshots. A token is on the stake's grid iff *any* of the credential's
/// addresses holds it, so a per-address gain/loss is a stake change only when the
/// credential-wide membership flips. From the flat map's structural diff we collect the
/// *candidate* assets (every asset whose entry changed on any of the credential's
/// addresses), then resolve each candidate's union membership in `prev` and `curr` with
/// one cred-prefix range scan each (whale-bounded, restricted to the candidates) — so two
/// addresses gaining the same token in one step is a single add, not two.
pub fn stake_tile_diff(
    prev: &AssetHoldings,
    curr: &AssetHoldings,
    cred: &[u8],
) -> (Vec<Token>, Vec<Token>) {
    use imbl::ordmap::DiffItem;
    use std::collections::HashSet;
    let mut candidates: HashSet<AssetId> = HashSet::new();
    for d in prev.diff(curr) {
        let (k, asset) = match d {
            DiffItem::Add((k, asset), _) => (k, asset),
            DiffItem::Remove((k, asset), _) => (k, asset),
            DiffItem::Update {
                old: ((k, asset), _),
                ..
            } => (k, asset),
        };
        if k.0.as_deref() == Some(cred) {
            candidates.insert(asset.clone());
        }
    }
    if candidates.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // Which candidate assets the credential holds on *any* address in a snapshot — one
    // range scan over the cred prefix, kept to the candidate set.
    let present = |map: &AssetHoldings| -> HashSet<AssetId> {
        let mut held: HashSet<AssetId> = HashSet::new();
        for ((_, asset), _) in cred_range(map, cred) {
            if candidates.contains(asset.as_ref()) {
                held.insert(asset.clone());
            }
        }
        held
    };
    let (in_prev, in_curr) = (present(prev), present(curr));
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for asset in candidates {
        let (policy, name) = split_asset(&asset);
        match (in_prev.contains(&asset), in_curr.contains(&asset)) {
            (false, true) => added.push((policy.to_vec(), name.to_vec())),
            (true, false) => removed.push((policy.to_vec(), name.to_vec())),
            _ => {}
        }
    }
    (added, removed)
}

/// A delegation map plus its reverse index: `(cred -> target, target -> {creds})`.
/// Returned by `apply_delegation_changes` for both pool and DRep delegations.
type DelegationIndex = (
    HashMap<Vec<u8>, Vec<u8>>,
    HashMap<Vec<u8>, HashSet<Vec<u8>>>,
);

/// All per-block data the sink hands to [`State::apply_block`], bundled into one
/// argument. Owns the produced outputs (the sink no longer needs them) and
/// borrows the rest from sink-local buffers for the duration of the call.
pub struct BlockUpdate<'a> {
    pub slot: u64,
    pub block_hash: String,
    pub epoch: u64,
    pub produced: Vec<((Vec<u8>, i16), TxOutput)>,
    pub consumed: &'a [((Vec<u8>, i16), TxOutput)],
    pub pool_delegation_changes: &'a [(Vec<u8>, Option<Vec<u8>>)],
    pub drep_delegation_changes: &'a [(Vec<u8>, Option<Vec<u8>>)],
    pub pool_updates: &'a [PoolUpdate],
    /// Pool retirement certs `(operator, retiring_epoch)`.
    pub pool_retirements: &'a [(Vec<u8>, u64)],
    /// Pool that minted this block (the slot leader), if known — increments its
    /// lifetime block count.
    pub issuer_pool_hash: Option<&'a [u8]>,
    pub stake_changes: &'a [(Vec<u8>, i64)],
    pub withdrawal_changes: &'a [(Vec<u8>, i64)],
    pub reward_deltas: Option<&'a HashMap<Vec<u8>, i64>>,
    /// At an epoch boundary: each DRep's refreshed `active_until` (tagged key → epoch);
    /// `None` between boundaries. DReps absent from the map become expired.
    pub drep_active_until: Option<&'a HashMap<Vec<u8>, i64>>,
}

pub struct State {
    history: Vec<BlockSnapshot>,
    db_url: Url,
    db: tokio::sync::OnceCell<DbSync>,
    pub feed_index: FeedIndex,
    // In-memory cursors for the live off-chain metadata refresh (pool tickers / DRep
    // names). Not in BlockSnapshot, so the persisted snapshot stays independent of
    // db-sync ids. Seeded to the current max at `reset`; left at 0 on warm resume so
    // the first post-catch-up block backfills (and a rollback resets them to 0).
    pub pool_meta_cursor: i64,
    pub drep_meta_cursor: i64,
}

impl State {
    pub fn new(db_url: Url) -> Self {
        Self {
            history: Vec::new(),
            db_url,
            db: tokio::sync::OnceCell::new(),
            feed_index: FeedIndex::new(),
            pool_meta_cursor: 0,
            drep_meta_cursor: 0,
        }
    }

    /// Populate `address_balances` from db-sync if the loaded snapshot wasn't
    /// built with it. Runs once after warm-resume from a pre-balances snapshot
    /// (or one whose previous populate failed); subsequent blocks maintain the
    /// map incrementally in `apply_block`. Gated on the explicit
    /// `address_balances_populated` flag, not `is_empty()`: if a previous
    /// populate failed, the sink will have inserted *partial* entries during
    /// catch-up that would otherwise fool an emptiness check and skip the
    /// re-populate.
    pub async fn populate_address_balances(&mut self) {
        let already_populated = self
            .history
            .last()
            .map(|s| s.address_balances_populated)
            .unwrap_or(false);
        if already_populated {
            return;
        }
        let Some(snap_slot) = self.history.last().map(|s| s.slot) else {
            return;
        };
        let Some(db) = self.db().await else { return };
        let (last_tx_id, _) = match db.slot_info(snap_slot).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to fetch slot_info for balances: {e}");
                return;
            }
        };
        let rows = match db.address_balances(last_tx_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to fetch address balances: {e}");
                return;
            }
        };
        use pallas::ledger::addresses::Address;
        let mut balances: HashMap<Vec<u8>, i64> = HashMap::new();
        for (bech32, balance) in rows {
            if let Ok(addr) = Address::from_bech32(&bech32) {
                balances.insert(addr.to_vec(), balance);
            }
        }
        let snap = self.history.last_mut().unwrap();
        snap.address_balances = balances;
        snap.address_balances_populated = true;
        tracing::info!(
            addresses = snap.address_balances.len(),
            "address balances populated from db-sync"
        );
    }

    /// Populate `asset_holdings` from db-sync if the loaded snapshot predates the field
    /// (the `#[serde(default)]` empty map, `asset_holdings_populated == false`). Runs
    /// once after a warm resume from such a snapshot; thereafter `apply_block` maintains
    /// the map per block. Gated on the explicit flag (not `is_empty()`) so a genuinely
    /// asset-free chain state isn't re-scanned every restart. Builds as-of the
    /// snapshot's slot — same point-in-time semantics as `reset()`.
    pub async fn populate_asset_holdings(&mut self) {
        let already = self
            .history
            .last()
            .map(|s| s.asset_holdings_populated)
            .unwrap_or(false);
        if already {
            return;
        }
        let Some(snap_slot) = self.history.last().map(|s| s.slot) else {
            return;
        };
        let last_tx_id = {
            let Some(db) = self.db().await else { return };
            match db.slot_info(snap_slot).await {
                Ok((id, _)) => id,
                Err(e) => {
                    tracing::warn!("slot_info for asset holdings: {e}");
                    return;
                }
            }
        };
        let holdings = {
            let Some(db) = self.db().await else { return };
            match Self::fetch_asset_holdings(db, last_tx_id).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("failed to fetch asset holdings: {e}");
                    return;
                }
            }
        };
        let snap = self.history.last_mut().unwrap();
        snap.asset_holdings = holdings;
        snap.asset_holdings_populated = true;
        tracing::info!(
            addresses = snap.asset_holdings.len(),
            "asset holdings populated from db-sync"
        );
    }

    /// Σ over `pool_delegations` of `stakes[cred] + rewards[cred]` — the total live
    /// stake delegated to pools. The one-time full scan behind `total_staked`.
    fn sum_delegated_stake(
        pool_delegations: &HashMap<Vec<u8>, Vec<u8>>,
        stakes: &HashMap<Vec<u8>, i64>,
        rewards: &HashMap<Vec<u8>, i64>,
    ) -> i64 {
        pool_delegations
            .keys()
            .map(|cred| {
                stakes.get(cred).copied().unwrap_or(0) + rewards.get(cred).copied().unwrap_or(0)
            })
            .sum()
    }

    /// Recompute `total_staked` for the current snapshot when it's missing (0 from a
    /// snapshot saved before the field existed). Pure in-memory; call at startup before
    /// blocks are applied. A genuinely-populated snapshot (post-feature) keeps its value.
    pub fn populate_total_staked(&mut self) {
        let Some(snap) = self.history.last() else {
            return;
        };
        if snap.total_staked != 0 || snap.pool_delegations.is_empty() {
            return;
        }
        let total = Self::sum_delegated_stake(&snap.pool_delegations, &snap.stakes, &snap.rewards);
        self.history.last_mut().unwrap().total_staked = total;
        tracing::info!(
            total_staked = total,
            "total_staked recomputed from snapshot"
        );
    }

    /// Backfill `Pool::retiring_epoch` for the current snapshot from db-sync. Needed
    /// when resuming from a snapshot saved before the field existed (where it defaults
    /// to `None`, so long-retired pools would wrongly count as active until the next
    /// reset). Idempotent — a correctly-populated snapshot is left unchanged. Thereafter
    /// `apply_block` maintains the field per block.
    pub async fn populate_pool_retirements(&mut self) {
        let Some(snap_slot) = self.history.last().map(|s| s.slot) else {
            return;
        };
        let Some(db) = self.db().await else { return };
        let last_tx_id = match db.slot_info(snap_slot).await {
            Ok((id, _)) => id,
            Err(e) => {
                tracing::warn!("slot_info for pool retirements: {e}");
                return;
            }
        };
        let pending = match db.pending_pool_retirements(last_tx_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to fetch pool retirements: {e}");
                return;
            }
        };
        let retiring: HashMap<String, i64> = pending
            .into_iter()
            .map(|(h, e)| (hex::encode(h), e))
            .collect();
        let Some(snap) = self.history.last_mut() else {
            return;
        };
        let keys: Vec<String> = snap.pools.keys().cloned().collect();
        for key in keys {
            let want = retiring.get(&key).copied();
            if let Some(pool) = snap.pools.get_mut(&key) {
                if pool.retiring_epoch != want {
                    pool.retiring_epoch = want;
                }
            }
        }
        tracing::info!(
            retiring = retiring.len(),
            "pool retirements populated from db-sync"
        );
    }

    /// Backfill `Pool::blocks` for the current snapshot from db-sync. Needed when
    /// resuming from a snapshot saved before the field existed (where it defaults to 0).
    /// Gated on all-zero — a populated snapshot (from `reset` or a prior run) is left
    /// untouched. Thereafter `apply_block` maintains it per block.
    pub async fn populate_block_counts(&mut self) {
        let needs = self
            .history
            .last()
            .map(|s| !s.pools.is_empty() && s.pools.values().all(|p| p.blocks == 0))
            .unwrap_or(false);
        if !needs {
            return;
        }
        let Some(snap_slot) = self.history.last().map(|s| s.slot) else {
            return;
        };
        let Some(db) = self.db().await else { return };
        let counts = match db.pool_block_counts(snap_slot as i64).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to fetch pool block counts: {e}");
                return;
            }
        };
        let counts: HashMap<String, i64> = counts
            .into_iter()
            .map(|(h, c)| (hex::encode(h), c))
            .collect();
        let Some(snap) = self.history.last_mut() else {
            return;
        };
        let keys: Vec<String> = snap.pools.keys().cloned().collect();
        for key in keys {
            if let Some(&c) = counts.get(&key) {
                if let Some(pool) = snap.pools.get_mut(&key) {
                    pool.blocks = c;
                }
            }
        }
        tracing::info!(
            pools = counts.len(),
            "pool block counts backfilled from db-sync"
        );
    }

    /// Backfill `DRep::active_until` from db-sync when missing (resume from a pre-field
    /// snapshot — all `None`). Thereafter `apply_block` refreshes it each epoch boundary.
    pub async fn populate_drep_active(&mut self) {
        let needs = self
            .history
            .last()
            .map(|s| !s.dreps.is_empty() && s.dreps.values().all(|d| d.active_until.is_none()))
            .unwrap_or(false);
        if !needs {
            return;
        }
        let Some(active) = self.drep_active_until().await else {
            return;
        };
        let Some(snap) = self.history.last_mut() else {
            return;
        };
        let keys: Vec<Vec<u8>> = snap.dreps.keys().cloned().collect();
        for key in keys {
            let au = active.get(&key).copied();
            if let Some(drep) = snap.dreps.get_mut(&key) {
                drep.active_until = au;
            }
        }
        tracing::info!(
            active = active.len(),
            "drep active_until backfilled from db-sync"
        );
    }

    /// Populate ADA Handle cache from db-sync if empty.
    pub async fn populate_handles(&mut self) {
        let is_empty = self
            .history
            .last()
            .map(|s| s.address_by_handle.is_empty())
            .unwrap_or(true);
        if !is_empty {
            return;
        }
        let policies: Vec<&[u8]> = HANDLE_POLICIES.iter().map(|p| p.as_slice()).collect();
        let rows = match self.db().await {
            Some(db) => match db.handles(&policies).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("failed to fetch handles: {e}");
                    return;
                }
            },
            None => return,
        };
        let total = rows.len();
        let snap = self.history.last_mut().unwrap();
        let mut virtual_ok = 0usize;
        let mut virtual_fail = 0usize;
        for (handle, addr, datum) in rows {
            let resolved_addr = if datum.is_some() {
                match datum.and_then(|d| parse_virtual_handle_address(&d)) {
                    Some(a) => {
                        virtual_ok += 1;
                        a
                    }
                    None => {
                        virtual_fail += 1;
                        continue;
                    }
                }
            } else {
                addr
            };
            snap.handle_by_address
                .entry(resolved_addr.clone())
                .or_default()
                .push(handle.clone());
            snap.address_by_handle.insert(handle, resolved_addr);
        }
        let resolved = snap.address_by_handle.len();
        tracing::info!(
            total,
            resolved,
            virtual_ok,
            virtual_fail,
            "ADA Handles populated from db-sync"
        );
    }

    /// Populate governance action titles from db-sync if empty.
    pub async fn populate_gov_titles(&mut self) {
        let is_empty = self
            .history
            .last()
            .map(|s| s.gov_action_titles.is_empty())
            .unwrap_or(true);
        if !is_empty {
            return;
        }
        let titles = match self.db().await {
            Some(db) => match db.gov_action_titles().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("failed to fetch gov action titles: {e}");
                    return;
                }
            },
            None => return,
        };
        tracing::info!(
            "{} governance action titles populated from db-sync",
            titles.len()
        );
        let snap = self.history.last_mut().unwrap();
        for (key, title) in titles {
            snap.gov_action_titles.insert(key, title);
        }
    }

    async fn db(&self) -> Option<&DbSync> {
        self.db
            .get_or_try_init(|| async { DbSync::new(&self.db_url).await })
            .await
            .ok()
    }

    /// Synchronous, lock-friendly clone of the db handle for callers that
    /// want to run a db query *without* holding the `chain_state` lock for
    /// its duration (avoiding head-of-line blocking when one slow query —
    /// e.g. a whale's `assets_count` — would otherwise stall every other
    /// reader behind the sink's pending writer).
    ///
    /// Returns `None` until `db()` has been awaited at least once; the
    /// daemon's startup `populate_*` calls do this before SSE accepts
    /// connections, so handlers can rely on `Some`.
    pub fn db_handle(&self) -> Option<DbSync> {
        self.db.get().cloned()
    }

    pub fn current(&self) -> Option<&BlockSnapshot> {
        self.history.last()
    }

    pub fn current_mut(&mut self) -> Option<&mut BlockSnapshot> {
        self.history.last_mut()
    }

    /// Parse the `address_balances` scan rows (`(bech32 address, lovelace)`) into
    /// the per-address balance map and the per-stake-credential UTXO stake (the
    /// sum of balances over each credential's payment addresses). The latter
    /// replaces a separate `utxo_stakes` query and is keyed by the same
    /// `stake_credential_from_address_bytes` the sink uses per-block, so reset
    /// and live maintenance agree (including on pointer addresses, which both
    /// skip). Byron / non-bech32 addresses are dropped — they never reach feeds.
    fn balances_and_stakes(
        balance_rows: Vec<(String, i64)>,
    ) -> (HashMap<Vec<u8>, i64>, HashMap<Vec<u8>, i64>) {
        use pallas::ledger::addresses::Address;
        let mut address_balances: HashMap<Vec<u8>, i64> = HashMap::new();
        let mut stakes: HashMap<Vec<u8>, i64> = HashMap::new();
        for (bech32, balance) in balance_rows {
            let Ok(addr) = Address::from_bech32(&bech32) else {
                continue;
            };
            let bytes = addr.to_vec();
            if let Some(cred) = stake_credential_from_address_bytes(&bytes) {
                *stakes.entry(cred).or_insert(0) += balance;
            }
            address_balances.insert(bytes, balance);
        }
        (address_balances, stakes)
    }

    /// Build the global [`BlockSnapshot::asset_holdings`] map from db-sync's
    /// `(bech32 address, fingerprint, unspent-UTXO count)` stream (ordered by address),
    /// computing each address's stake credential once. Streamed row-by-row so the full
    /// ~15M-row result never materializes as one big `Vec`. The heavy cold-start query;
    /// warm resume deserializes the map instead.
    async fn fetch_asset_holdings(
        db: &DbSync,
        last_tx_id: i64,
    ) -> Result<AssetHoldings, sqlx::Error> {
        use pallas::ledger::addresses::Address;
        let mut holdings: AssetHoldings = OrdMap::new();
        // The query is ordered by address, so cache the decoded `(cred, addr)` key and
        // reuse it across that address's rows instead of re-decoding the bech32 each row.
        let mut cur_addr: Option<String> = None;
        let mut cur_key: Option<AssetKey> = None;
        let mut rows: u64 = 0;
        db.asset_holdings_for_each(last_tx_id, |addr, policy, name, count| {
            rows += 1;
            if cur_addr.as_deref() != Some(addr.as_str()) {
                cur_key = Address::from_bech32(&addr).ok().map(|a| {
                    let bytes = a.to_vec();
                    (stake_credential_from_address_bytes(&bytes), bytes)
                });
                cur_addr = Some(addr);
            }
            // `count` is the summed quantity as text; parse to u128 (saturates only at the
            // absurd u128 ceiling, far beyond any real token — so no precision is lost).
            let qty = count.parse::<u128>().unwrap_or(u128::MAX);
            if let (Some(key), true) = (&cur_key, qty > 0) {
                holdings.insert((key.clone(), asset_id(&policy, &name)), Qty(qty));
            }
            if rows.is_multiple_of(1_000_000) {
                tracing::info!(
                    rss_mb = rss_mb(),
                    rows,
                    entries = holdings.len(),
                    "asset holdings: building (streaming)"
                );
            }
        })
        .await?;
        tracing::info!(
            rss_mb = rss_mb(),
            rows,
            entries = holdings.len(),
            "asset holdings built from db-sync"
        );
        Ok(holdings)
    }

    /// Initialize state from db-sync data at a given reset point.
    /// Fetches pools, delegations, stakes, and rewards from db-sync,
    /// replaces all history with a single snapshot.
    pub async fn reset(
        &mut self,
        slot: u64,
        genesis: &oura::framework::GenesisValues,
        mainnet: bool,
    ) -> Result<(), sqlx::Error> {
        let db = self
            .db
            .get_or_try_init(|| async { DbSync::new(&self.db_url).await })
            .await?;

        let (last_tx_id, block_hash) = db.slot_info(slot).await?;
        tracing::info!(
            rss_mb = rss_mb(),
            "reset: start (rebuilding state from db-sync)"
        );

        tracing::info!("Fetching pools...");
        let pools = db.pools(last_tx_id, slot as i64).await?;
        tracing::info!("{} pools retrieved", pools.len());

        tracing::info!("Fetching pool delegations...");
        let (pool_delegations, pool_delegators) = db.pool_delegations(last_tx_id).await?;
        tracing::info!(
            rss_mb = rss_mb(),
            "{} pool delegations in {} pools retrieved",
            pool_delegations.len(),
            pool_delegators.len()
        );

        tracing::info!("Fetching DRep delegations...");
        let (drep_delegations, drep_delegators) = db.drep_delegations(last_tx_id).await?;
        tracing::info!(
            rss_mb = rss_mb(),
            "{} DRep delegations in {} DReps retrieved",
            drep_delegations.len(),
            drep_delegators.len()
        );

        // UTXO stakes are derived from the per-address balance scan in
        // `warm_asset_cache` below (summed per stake credential), so there's no
        // separate `utxo_stakes` query.

        let current_epoch = Self::epoch_for_slot(slot, genesis);
        tracing::info!("Fetching rewards (epoch {})...", current_epoch);
        let rewards = db.rewards(current_epoch, last_tx_id).await?;
        tracing::info!(
            rss_mb = rss_mb(),
            "{} stake addresses with rewards",
            rewards.len()
        );

        tracing::info!("Fetching DRep metadata...");
        let dreps = db.drep_metadata(last_tx_id, 0).await?;
        tracing::info!("{} DReps with metadata", dreps.len());

        // Seed the live-refresh cursors to the current max: this reset just loaded the
        // current tickers/names, so the per-block refresh only needs newer rows.
        let pool_meta_cursor = db.max_pool_meta_id().await?;
        let drep_meta_cursor = db.max_drep_meta_id().await?;

        tracing::info!("Fetching CIP-68 reference token decimals...");
        let cip68_rows = db.cip68_decimals(last_tx_id).await?;
        let mut decimals = HashMap::new();
        // `cip68_decimals` returns the real (333/444) user token, so store exactly
        // one fingerprint per token (the same key `decimals.get` computes at
        // display time) — no dead ft/rft variant.
        for (policy, name, d) in &cip68_rows {
            if *d > 0 && *d <= 255 {
                decimals.insert(asset_fingerprint(policy, name), *d as u8);
            }
        }
        tracing::info!("{} CIP-68 tokens with decimals", decimals.len());

        // CIP-26: fetch decimals from GitHub token registry
        let registry = if mainnet {
            cip26::RegistryConfig::mainnet()
        } else {
            cip26::RegistryConfig::testnet()
        };
        let client = reqwest::Client::new();
        let cip26_entries = cip26::fetch_decimals(&client, &registry).await;
        for (fp, d) in cip26_entries {
            decimals.entry(fp).or_insert(d); // CIP-68 takes precedence
        }
        tracing::info!(
            "{} total tokens with decimals (CIP-68 + CIP-26)",
            decimals.len()
        );

        tracing::info!("Fetching ADA Handle owners...");
        let policies: Vec<&[u8]> = HANDLE_POLICIES.iter().map(|p| p.as_slice()).collect();
        let handle_rows = db.handles(&policies).await?;
        let mut handle_by_address: HashMap<String, Vec<String>> = HashMap::new();
        let mut address_by_handle: HashMap<String, String> = HashMap::new();
        for (handle, addr, datum) in &handle_rows {
            let resolved_addr = if datum.is_some() {
                match datum.as_ref().and_then(|d| parse_virtual_handle_address(d)) {
                    Some(a) => a,
                    None => continue,
                }
            } else {
                addr.clone()
            };
            handle_by_address
                .entry(resolved_addr.clone())
                .or_default()
                .push(handle.clone());
            address_by_handle.insert(handle.clone(), resolved_addr);
        }
        tracing::info!("{} ADA Handles resolved", handle_by_address.len());

        tracing::info!("Fetching governance action titles...");
        let gov_action_titles = db.gov_action_titles().await?.into();
        tracing::info!("governance action titles fetched");

        tracing::info!("Fetching per-address balances...");
        let balance_rows = db.address_balances(last_tx_id).await?;
        tracing::info!(
            rss_mb = rss_mb(),
            "{} address-balance rows fetched (transient Vec)",
            balance_rows.len()
        );
        let (address_balances, stakes) = Self::balances_and_stakes(balance_rows);
        tracing::info!(
            rss_mb = rss_mb(),
            "{} addresses with UTXOs, {} stake credentials",
            address_balances.len(),
            stakes.len()
        );

        let total_staked = Self::sum_delegated_stake(&pool_delegations, &stakes, &rewards);

        tracing::info!("Fetching per-address asset holdings...");
        let asset_holdings = Self::fetch_asset_holdings(db, last_tx_id).await?;

        self.history.clear();
        self.history.push(BlockSnapshot {
            slot,
            block_hash: Some(block_hash),
            last_epoch: Some(current_epoch),
            utxos: HashMap::new(),
            pools,
            pool_delegations,
            pool_delegators,
            total_staked,
            drep_delegations,
            drep_delegators,
            address_balances,
            address_balances_populated: true,
            dreps,
            stakes,
            rewards,
            decimals,
            handle_by_address,
            address_by_handle,
            gov_action_titles,
            asset_holdings,
            asset_holdings_populated: true,
        });
        self.feed_index = FeedIndex::new();
        self.pool_meta_cursor = pool_meta_cursor;
        self.drep_meta_cursor = drep_meta_cursor;

        if let Some(snap) = self.history.last() {
            snap.log_sizes("reset complete");
        }
        Ok(())
    }

    pub fn epoch_for_slot(slot: u64, genesis: &oura::framework::GenesisValues) -> u64 {
        // shelley_known_slot is in Byron slot numbering; byron_epoch_length is in seconds
        let shelley_start_epoch = genesis.shelley_known_slot * genesis.byron_slot_length as u64
            / genesis.byron_epoch_length as u64;
        shelley_start_epoch
            + (slot - genesis.shelley_known_slot) / genesis.shelley_epoch_length as u64
    }

    /// Apply a new block: clone current snapshot (O(1) structural sharing),
    /// apply UTXO changes, stake changes, withdrawals, and push to history.
    ///
    /// `consumed` carries each input's `(utxo_ref, resolved_output)` so
    /// `address_balances` can be decremented without re-looking up inputs that
    /// predate the snapshot (and aren't in `prev.utxos`).
    ///
    /// Live asset-grid deltas aren't produced here — each open assets page derives its own
    /// by diffing `asset_holdings` against the previous snapshot ([`address_tile_diff`] /
    /// [`stake_tile_diff`]).
    pub fn apply_block(&mut self, update: BlockUpdate) {
        let BlockUpdate {
            slot,
            block_hash,
            epoch,
            produced,
            consumed,
            pool_delegation_changes,
            drep_delegation_changes,
            pool_updates,
            pool_retirements,
            issuer_pool_hash,
            stake_changes,
            withdrawal_changes,
            reward_deltas,
            drep_active_until,
        } = update;

        let prev = self.history.last().expect("state not initialized");

        let mut utxos = prev.utxos.clone();
        let mut address_balances = prev.address_balances.clone();
        // Global per-address asset holdings (O(1) imbl clone), maintained for every
        // address. Live tile deltas are derived per open assets page by diffing this
        // against the previous snapshot — nothing is emitted from here.
        let mut asset_holdings = prev.asset_holdings.clone();
        let asset_holdings_populated = prev.asset_holdings_populated;

        // Apply produced (credits) *before* consumed (debits). A UTXO can be created and
        // spent within the same block; if we debited first, its entry wouldn't exist yet,
        // so the `get_mut`/`bump_one` debit would be silently dropped — over-counting the
        // balance/holdings — and `utxos.remove` would no-op, leaving a phantom UTXO. Doing
        // credits first guarantees the entry exists when the debit lands. (`stakes` is
        // immune either way: it's a flat sum of signed deltas.)
        for (key, output) in produced {
            let bal: i64 = output
                .lovelaces
                .try_into()
                .expect("lovelace value must fit i64");
            *address_balances.entry(output.address.clone()).or_insert(0) += bal;
            apply_utxo_assets(&output.address, &output.assets, true, &mut asset_holdings);
            utxos.insert(key, output);
        }
        for (key, output) in consumed {
            utxos.remove(key);
            let bal: i64 = output
                .lovelaces
                .try_into()
                .expect("lovelace value must fit i64");
            if let Some(entry) = address_balances.get_mut(&output.address) {
                *entry -= bal;
                if *entry <= 0 {
                    address_balances.remove(&output.address);
                }
            }
            apply_utxo_assets(&output.address, &output.assets, false, &mut asset_holdings);
        }

        let (pool_delegations, pool_delegators) = Self::apply_delegation_changes(
            &prev.pool_delegations,
            &prev.pool_delegators,
            pool_delegation_changes,
        );

        let (drep_delegations, drep_delegators) = Self::apply_delegation_changes(
            &prev.drep_delegations,
            &prev.drep_delegators,
            drep_delegation_changes,
        );

        // Cloned every block (cheap, O(1) structural share) since the issuer's lifetime
        // block count is incremented below.
        let mut pools = prev.pools.clone();
        // Registrations first: `from_registration` resets `retiring_epoch` to None, so a
        // (re-)registration cancels a pending retirement — but the lifetime block count
        // must survive a param update, so carry it across like `ticker`.
        for (operator, pledge, cost, margin_num, margin_den) in pool_updates {
            let key = hex::encode(operator);
            let (ticker, blocks) = pools
                .get(&key)
                .map(|p| (p.ticker.clone(), p.blocks))
                .unwrap_or((None, 0));
            let mut pool =
                Pool::from_registration(operator.clone(), *pledge, *cost, *margin_num, *margin_den);
            pool.ticker = ticker;
            pool.blocks = blocks;
            pools.insert(key, pool);
        }
        // Then retirements: record the retiring epoch (the pool stays active until it).
        for (operator, retiring_epoch) in pool_retirements {
            if let Some(pool) = pools.get_mut(&hex::encode(operator)) {
                pool.retiring_epoch = Some(*retiring_epoch as i64);
            }
        }
        // This block's minting pool: +1 lifetime block.
        if let Some(hash) = issuer_pool_hash {
            if let Some(pool) = pools.get_mut(&hex::encode(hash)) {
                pool.blocks += 1;
            }
        }

        let mut stakes = prev.stakes.clone();
        for (cred, delta) in stake_changes {
            let entry = stakes.entry(cred.clone()).or_insert(0);
            *entry += delta;
        }

        let mut rewards = prev.rewards.clone();
        for (cred, amount) in withdrawal_changes {
            let entry = rewards.entry(cred.clone()).or_insert(0);
            *entry -= amount;
        }
        if let Some(deltas) = reward_deltas {
            for (cred, delta) in deltas {
                let entry = rewards.entry(cred.clone()).or_insert(0);
                *entry += delta;
            }
        }

        // Maintain total_staked: adjust only when a credential enters or leaves the
        // pool-delegated set (the per-block balance drift of stable delegators is
        // intentionally not re-summed — re-synced on reset).
        let total_staked = prev.total_staked
            + Self::delegation_stake_delta(
                &prev.pool_delegations,
                pool_delegation_changes,
                &stakes,
                &rewards,
            );

        // At an epoch boundary, refresh each DRep's `active_until` from db-sync's
        // `drep_distr`; DReps absent from the map have expired/deregistered (→ None).
        let dreps = if let Some(active) = drep_active_until {
            let mut dreps = prev.dreps.clone();
            let keys: Vec<Vec<u8>> = dreps.keys().cloned().collect();
            for key in keys {
                let au = active.get(&key).copied();
                if let Some(drep) = dreps.get_mut(&key) {
                    drep.active_until = au;
                }
            }
            dreps
        } else {
            prev.dreps.clone()
        };
        let decimals = prev.decimals.clone();
        let handle_by_address = prev.handle_by_address.clone();
        let address_by_handle = prev.address_by_handle.clone();
        let gov_action_titles = prev.gov_action_titles.clone();
        let address_balances_populated = prev.address_balances_populated;
        self.history.push(BlockSnapshot {
            slot,
            block_hash: Some(block_hash),
            last_epoch: Some(epoch),
            utxos,
            pools,
            pool_delegations,
            pool_delegators,
            drep_delegations,
            drep_delegators,
            dreps,
            stakes,
            rewards,
            decimals,
            address_balances,
            address_balances_populated,
            asset_holdings,
            asset_holdings_populated,
            handle_by_address,
            address_by_handle,
            gov_action_titles,
            total_staked,
        });

        // In-memory history serves only rollbacks: feed replay uses db-sync + feed_index
        // (~30 blocks), and pool/drep/stake/address feeds + all other readers use the
        // latest snapshot — none of them depend on this depth. Sized to k = 2160 (worst-
        // case reversal); a rollback deeper than history safely falls back to reset() (see
        // `sink`'s rollback handler). Each retained snapshot pins that block's per-map imbl
        // delta nodes, so this is a *steady-state* memory lever — can drop to ~180 (~1 h,
        // far beyond any real rollback) once the size logs confirm it's worth it.
        const MAX_HISTORY: usize = 2160;
        if self.history.len() > MAX_HISTORY {
            self.history.drain(..self.history.len() - MAX_HISTORY);
        }
    }

    /// Net change to `total_staked` from this block's pool delegation changes: a
    /// credential entering the delegated set adds its `stakes+rewards`, one leaving
    /// (deregistration) subtracts it, and re-delegation pool→pool is a no-op. Valued
    /// at the post-block `stakes`/`rewards`. Pure — unit-tested.
    fn delegation_stake_delta(
        prev_pool_delegations: &HashMap<Vec<u8>, Vec<u8>>,
        pool_delegation_changes: &[(Vec<u8>, Option<Vec<u8>>)],
        stakes: &HashMap<Vec<u8>, i64>,
        rewards: &HashMap<Vec<u8>, i64>,
    ) -> i64 {
        let mut delta = 0;
        for (cred, maybe_target) in pool_delegation_changes {
            let was = prev_pool_delegations.contains_key(cred);
            let now = maybe_target.is_some();
            if was == now {
                continue;
            }
            let val =
                stakes.get(cred).copied().unwrap_or(0) + rewards.get(cred).copied().unwrap_or(0);
            if now {
                delta += val;
            } else {
                delta -= val;
            }
        }
        delta
    }

    fn apply_delegation_changes(
        prev_delegations: &HashMap<Vec<u8>, Vec<u8>>,
        prev_delegators: &HashMap<Vec<u8>, HashSet<Vec<u8>>>,
        changes: &[(Vec<u8>, Option<Vec<u8>>)],
    ) -> DelegationIndex {
        if changes.is_empty() {
            return (prev_delegations.clone(), prev_delegators.clone());
        }
        let mut delegations = prev_delegations.clone();
        let mut delegators = prev_delegators.clone();
        for (stake_addr, maybe_target) in changes {
            if let Some(old_target) = delegations.remove(stake_addr) {
                if let Some(set) = delegators.get_mut(&old_target) {
                    set.remove(stake_addr);
                }
            }
            if let Some(target) = maybe_target {
                delegations.insert(stake_addr.clone(), target.clone());
                delegators
                    .entry(target.clone())
                    .or_default()
                    .insert(stake_addr.clone());
            }
        }
        (delegations, delegators)
    }

    /// Compute total live stake for a pool by summing stakes + rewards
    /// of all its delegators.
    pub fn pool_live_stake(snap: &BlockSnapshot, pool_hash: &[u8]) -> Option<i64> {
        let delegators = snap.pool_delegators.get(pool_hash)?;
        let mut utxo_total: i64 = 0;
        let mut reward_total: i64 = 0;
        let mut with_stake = 0u32;
        let mut with_reward = 0u32;
        for cred in delegators.iter() {
            if let Some(&s) = snap.stakes.get(cred) {
                utxo_total += s;
                with_stake += 1;
            }
            if let Some(&r) = snap.rewards.get(cred) {
                reward_total += r;
                with_reward += 1;
            }
        }
        tracing::debug!(
            pool = hex::encode(pool_hash),
            delegators = delegators.len(),
            with_stake,
            with_reward,
            utxo_total,
            reward_total,
            total = utxo_total + reward_total,
            "pool_live_stake"
        );
        Some(utxo_total + reward_total)
    }

    /// Compute total live stake for a DRep by summing stakes + rewards
    /// of all its delegators.
    pub fn drep_live_stake(snap: &BlockSnapshot, drep_bytes: &[u8]) -> Option<i64> {
        let delegators = snap.drep_delegators.get(drep_bytes)?;
        let mut total: i64 = 0;
        for cred in delegators.iter() {
            if let Some(&s) = snap.stakes.get(cred) {
                total += s;
            }
            if let Some(&r) = snap.rewards.get(cred) {
                total += r;
            }
        }
        Some(total)
    }

    /// Find the most recent block at or before the given slot.
    /// Creates a temporary db connection (for use at startup before State is fully initialized).
    pub async fn boundary_block(db_url: &Url, boundary_slot: u64) -> Option<(u64, String)> {
        let db = DbSync::new(db_url).await.ok()?;
        db.boundary_block(boundary_slot).await
    }

    /// Fetch epoch reward deltas from db-sync for a new epoch.
    pub async fn epoch_reward_delta(&self, epoch: u64) -> Option<HashMap<Vec<u8>, i64>> {
        let db = self.db().await?;
        db.epoch_reward_delta(epoch).await.ok()
    }

    /// Latest `drep_distr.active_until` per DRep, keyed by the tagged hash bytes used in
    /// the `dreps` map (`[has_script ? 0x01 : 0x00] ++ raw`). Refreshes
    /// `DRep::active_until` at epoch boundaries; DReps absent here are expired/deregistered.
    pub async fn drep_active_until(&self) -> Option<HashMap<Vec<u8>, i64>> {
        let db = self.db().await?;
        let rows = db.drep_active_until().await.ok()?;
        let mut map = HashMap::new();
        for (raw, has_script, active_until) in rows {
            let tag = if has_script { 0x01u8 } else { 0x00 };
            map.insert([&[tag][..], &raw[..]].concat(), active_until);
        }
        Some(map)
    }

    /// Rollback to the given slot: drop all snapshots after it.
    /// Returns false if history is empty after truncation (snapshot was too old).
    pub fn rollback(&mut self, slot: u64) -> bool {
        let keep = self
            .history
            .iter()
            .rposition(|s| s.slot <= slot)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.history.truncate(keep);
        self.feed_index.rollback(slot);
        !self.history.is_empty()
    }

    /// Restore state from a previously saved snapshot.
    pub fn restore_from_snapshot(&mut self, snapshot: BlockSnapshot) {
        self.history.clear();
        self.history.push(snapshot);
    }

    /// Save snapshot + feed_index + network_magic to disk. Picks the snapshot
    /// `depth` blocks back from tip. Writes atomically via tmp file + rename.
    /// Synchronously serialize + atomically write the snapshot `depth` blocks behind the
    /// tip. Used by the rare post-reset save (already off the steady-state hot path); the
    /// periodic save offloads via [`State::clone_for_save`] + [`write_snapshot`] instead.
    pub fn save_snapshot(
        &self,
        path: &Path,
        depth: usize,
        network_magic: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let idx = self.history.len().saturating_sub(depth);
        write_snapshot(path, &self.history[idx], &self.feed_index, network_magic)
    }

    /// Clone the point-in-time data to persist (the snapshot `depth` blocks behind the
    /// tip + the feed index), so the caller can serialize + write it *off* the
    /// `chain_state` lock. The `BlockSnapshot` clone is O(1) (`imbl` structural sharing);
    /// the `FeedIndex` clone is a ms-scale deep copy of the pruned 5-day index. Returns
    /// the cloned data plus its slot.
    pub fn clone_for_save(&self, depth: usize) -> (BlockSnapshot, FeedIndex, u64) {
        let idx = self.history.len().saturating_sub(depth);
        let snap = self.history[idx].clone();
        let slot = snap.slot;
        (snap, self.feed_index.clone(), slot)
    }

    /// Load snapshot + feed_index from disk. Validates network magic matches.
    ///
    /// Deserializes **straight from a buffered file reader** rather than reading the whole
    /// file into a `Vec<u8>` first: that byte buffer would be a multi-GB transient stacked
    /// on the structures being built (the startup RSS spike). The `BufReader` streams it.
    pub fn load_snapshot(path: &Path, network_magic: u64) -> Option<(BlockSnapshot, FeedIndex)> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!("failed to read snapshot from {}: {}", path.display(), e);
                return None;
            }
        };
        tracing::info!("loading snapshot from {}...", path.display());
        let rd = std::io::BufReader::new(file);
        match rmp_serde::from_read::<_, (BlockSnapshot, FeedIndex, u64, u32)>(rd) {
            Ok((snap, fi, magic, format)) => {
                if magic != network_magic {
                    tracing::warn!(
                        "snapshot network mismatch: snapshot={}, expected={}",
                        magic,
                        network_magic
                    );
                    return None;
                }
                if format != SNAPSHOT_FORMAT {
                    tracing::warn!(
                        "snapshot format mismatch: snapshot={}, expected={} — rebuilding from db-sync",
                        format,
                        SNAPSHOT_FORMAT
                    );
                    return None;
                }
                Some((snap, fi))
            }
            Err(e) => {
                tracing::warn!("failed to deserialize snapshot: {}", e);
                None
            }
        }
    }

    /// Resolve an input by (tx_hash, output_index): check in-memory UTXOs first,
    /// then fall back to db-sync. Returns (address, lovelace, assets).
    pub async fn resolve_input(
        &self,
        tx_hash: &[u8],
        index: i16,
    ) -> (Option<String>, u64, crate::model::PolicyAssets) {
        if let Some(utxo) = self
            .current()
            .and_then(|s| s.utxos.get(&(tx_hash.to_vec(), index)))
        {
            return (
                pallas::ledger::addresses::Address::from_bytes(&utxo.address)
                    .ok()
                    .map(|a| a.to_string()),
                utxo.lovelaces
                    .try_into()
                    .expect("lovelace value must fit u64"),
                utxo.assets.clone(),
            );
        }
        if let Some(db) = self.db().await {
            if let Ok(Some((address, value, assets))) = db.resolve_utxo(tx_hash, index).await {
                return (
                    Some(address),
                    value.try_into().expect("lovelace value must fit u64"),
                    assets,
                );
            }
        }
        (None, 0, vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oura::framework::GenesisValues;

    #[test]
    fn bump_one_maintains_and_prunes() {
        let mut holdings: AssetHoldings = OrdMap::new();
        let key: AssetKey = (None, b"addr".to_vec());
        let policy = vec![0xaau8; 28];
        let name = b"TOKEN".to_vec();
        let hkey = (key.clone(), asset_id(&policy, &name));

        bump_one(&mut holdings, &key, &policy, &name, 1, true);
        assert_eq!(holdings[&hkey].0, 1);
        bump_one(&mut holdings, &key, &policy, &name, 1, true);
        assert_eq!(holdings[&hkey].0, 2);
        bump_one(&mut holdings, &key, &policy, &name, 1, false);
        assert_eq!(holdings[&hkey].0, 1);

        // Last UTXO spent → the entry hits 0 and is pruned, so the map's keys stay exactly
        // the tokens currently held by ≥1 UTXO.
        bump_one(&mut holdings, &key, &policy, &name, 1, false);
        assert!(holdings.is_empty());

        // Spending a token the map never had is a no-op.
        bump_one(&mut holdings, &key, &policy, &name, 1, false);
        assert!(holdings.is_empty());
    }

    #[test]
    fn qty_serde_roundtrips_including_above_u64() {
        // Exact round-trip across the whole range — crucially the values above u64 that
        // a `::bigint` SUM would have thrown on / clamping would have lost.
        for v in [
            0u128,
            1,
            u64::MAX as u128,
            u64::MAX as u128 + 1,
            (u64::MAX as u128) * 3, // a few near-i64::MAX UTXOs summed
            u128::MAX,
        ] {
            let bytes = rmp_serde::to_vec(&Qty(v)).unwrap();
            let back: Qty = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(back.0, v, "round-trip {v}");
        }
        // Back-compat: a leaf serialized as a plain u64 (old format) reads as Qty.
        let old = rmp_serde::to_vec(&123u64).unwrap();
        assert_eq!(rmp_serde::from_slice::<Qty>(&old).unwrap().0, 123);
        // Small values stay 1 byte (same as the old u64 leaf), not the 2-int fallback.
        assert_eq!(rmp_serde::to_vec(&Qty(1)).unwrap().len(), 1);
    }

    #[test]
    fn tile_diffs_address_stake_and_rollback() {
        let cred = vec![0xcd; 28];
        let addr1 = b"addr-one".to_vec();
        let addr2 = b"addr-two".to_vec();
        let key1: AssetKey = (Some(cred.clone()), addr1.clone());
        let key2: AssetKey = (Some(cred.clone()), addr2.clone());
        let policy = vec![0xab; 28];
        let name = b"SHARED".to_vec();
        let tok = (policy.clone(), name.clone());

        // s0 empty → s1: addr1 gains the token.
        let s0: AssetHoldings = OrdMap::new();
        let mut s1 = s0.clone();
        bump_one(&mut s1, &key1, &policy, &name, 1, true);

        // Address diff: addr1 gains a tile; an untouched address sees nothing.
        assert_eq!(
            address_tile_diff(&s0, &s1, &key1),
            (vec![tok.clone()], vec![])
        );
        assert_eq!(address_tile_diff(&s0, &s1, &key2), (vec![], vec![]));

        // Stake diff: first holder → the union gains the token.
        assert_eq!(
            stake_tile_diff(&s0, &s1, &cred),
            (vec![tok.clone()], vec![])
        );

        // s1 → s2: addr2 also gains it. The union already had it (addr1) → no change.
        let mut s2 = s1.clone();
        bump_one(&mut s2, &key2, &policy, &name, 1, true);
        assert_eq!(stake_tile_diff(&s1, &s2, &cred), (vec![], vec![]));

        // s2 → s3: addr1 drops it; addr2 still holds → union unchanged.
        let mut s3 = s2.clone();
        bump_one(&mut s3, &key1, &policy, &name, 1, false);
        assert_eq!(stake_tile_diff(&s2, &s3, &cred), (vec![], vec![]));

        // s3 → s4: addr2 drops it; nobody holds → union loses it.
        let mut s4 = s3.clone();
        bump_one(&mut s4, &key2, &policy, &name, 1, false);
        assert_eq!(
            stake_tile_diff(&s3, &s4, &cred),
            (vec![], vec![tok.clone()])
        );

        // Rollback s4 → s2 (both addresses regain it in one step): one corrective add,
        // not two — the union flipped once.
        assert_eq!(
            stake_tile_diff(&s4, &s2, &cred),
            (vec![tok.clone()], vec![])
        );
    }

    #[test]
    fn epoch_for_slot_mainnet() {
        let genesis = GenesisValues::mainnet();
        // Shelley start: epoch 208, slot 4492800
        assert_eq!(State::epoch_for_slot(4492800, &genesis), 208);
        // Known block at slot 181914346 is epoch 618
        assert_eq!(State::epoch_for_slot(181914346, &genesis), 618);
    }

    #[test]
    fn total_staked_delegation_delta() {
        let cred_a = vec![0xaa; 28];
        let cred_b = vec![0xbb; 28];
        let pool = vec![0x01; 28];
        let pool2 = vec![0x02; 28];

        let mut stakes = HashMap::new();
        stakes.insert(cred_a.clone(), 100i64);
        stakes.insert(cred_b.clone(), 30i64);
        let mut rewards = HashMap::new();
        rewards.insert(cred_a.clone(), 5i64);

        // A already delegates to `pool`; B does not yet.
        let mut prev_delegations = HashMap::new();
        prev_delegations.insert(cred_a.clone(), pool.clone());

        // New delegation: B enters the set → +(stake + rewards) = +30.
        let changes = vec![(cred_b.clone(), Some(pool.clone()))];
        assert_eq!(
            State::delegation_stake_delta(&prev_delegations, &changes, &stakes, &rewards),
            30
        );

        // Deregistration: A leaves the set → −(100 + 5) = −105.
        let changes = vec![(cred_a.clone(), None)];
        assert_eq!(
            State::delegation_stake_delta(&prev_delegations, &changes, &stakes, &rewards),
            -105
        );

        // Re-delegation pool→pool2: A stays delegated → no change.
        let changes = vec![(cred_a.clone(), Some(pool2))];
        assert_eq!(
            State::delegation_stake_delta(&prev_delegations, &changes, &stakes, &rewards),
            0
        );

        // Dereg of a never-delegated credential → no change.
        let changes = vec![(cred_b.clone(), None)];
        assert_eq!(
            State::delegation_stake_delta(&prev_delegations, &changes, &stakes, &rewards),
            0
        );
    }

    /// One fake block that touches **every** value `apply_block` maintains, then a rollback.
    /// Asserts each tracked value is updated exactly, and that the rollback restores the
    /// *entire* snapshot to its initial state (`BlockSnapshot: PartialEq` compares all fields).
    #[test]
    fn apply_block_updates_every_tracked_value_and_rollback_restores_them() {
        use rust_decimal::Decimal;

        // Credentials, pools, dreps, policies, addresses (57-byte base addresses so the
        // holdings map derives a real stake credential from the address bytes).
        let cred_a = vec![0xa1u8; 28];
        let cred_b = vec![0xb1u8; 28];
        let cred_c = vec![0xc1u8; 28]; // introduced by the block
        let pool_p = vec![0x01u8; 28];
        let pool_q = vec![0x02u8; 28]; // created by the block
        let drep_x = [&[0x00u8][..], &[0xd1u8; 28]].concat(); // tagged (key) drep
        let drep_y = [&[0x00u8][..], &[0xd2u8; 28]].concat();
        let policy1 = vec![0x11u8; 28];
        let name1 = b"TOK1".to_vec();
        let policy2 = vec![0x22u8; 28];
        let name2 = b"TOK2".to_vec();
        let addr1 = [&[0x00u8][..], &[0x31; 28], &[0x41; 28]].concat(); // consumed
        let addr2 = [&[0x00u8][..], &[0x32; 28], &[0x42; 28]].concat(); // untouched
        let addr3 = [&[0x00u8][..], &[0x33; 28], &[0x43; 28]].concat(); // produced
        let txh0 = vec![0xf0u8; 32];
        let txh1 = vec![0xf1u8; 32];
        let txh2 = vec![0xf2u8; 32];

        let p1n1 = |q: u64| -> crate::model::PolicyAssets {
            vec![(policy1.clone(), vec![(name1.clone(), q)])]
        };
        let p2n2 = |q: u64| -> crate::model::PolicyAssets {
            vec![(policy2.clone(), vec![(name2.clone(), q)])]
        };
        let txout = |lov: u64, addr: &[u8], assets: crate::model::PolicyAssets| TxOutput {
            lovelaces: Decimal::from(lov),
            address: addr.to_vec(),
            assets,
        };
        let set =
            |creds: &[&[u8]]| -> HashSet<Vec<u8>> { creds.iter().map(|c| c.to_vec()).collect() };

        // ---- Initial snapshot: non-trivial values in every tracked field. ----
        let mut initial = BlockSnapshot {
            slot: 100,
            block_hash: Some("hash100".into()),
            last_epoch: Some(10),
            total_staked: 105, // cred_a delegated to pool_p: stakes 100 + rewards 5
            address_balances_populated: true,
            asset_holdings_populated: true,
            ..BlockSnapshot::default()
        };
        initial
            .utxos
            .insert((txh0.clone(), 0), txout(2_000_000, &addr2, p1n1(3)));
        initial
            .utxos
            .insert((txh1.clone(), 0), txout(1_000_000, &addr1, p1n1(10)));
        initial.address_balances.insert(addr1.clone(), 1_000_000);
        initial.address_balances.insert(addr2.clone(), 2_000_000);
        apply_utxo_assets(&addr1, &p1n1(10), true, &mut initial.asset_holdings);
        apply_utxo_assets(&addr2, &p1n1(3), true, &mut initial.asset_holdings);
        initial.stakes.insert(cred_a.clone(), 100);
        initial.stakes.insert(cred_b.clone(), 50);
        initial.rewards.insert(cred_a.clone(), 5);
        initial.rewards.insert(cred_b.clone(), 2);
        initial
            .pool_delegations
            .insert(cred_a.clone(), pool_p.clone());
        initial
            .pool_delegators
            .insert(pool_p.clone(), set(&[&cred_a]));
        initial
            .drep_delegations
            .insert(cred_a.clone(), drep_x.clone());
        initial
            .drep_delegators
            .insert(drep_x.clone(), set(&[&cred_a]));
        let mut pp = Pool::from_registration(pool_p.clone(), 1000, 340, 3, 100);
        pp.ticker = Some("AAA".into());
        pp.blocks = 5;
        initial.pools.insert(hex::encode(&pool_p), pp);
        initial.dreps.insert(
            drep_x.clone(),
            DRep {
                hash_bytes: drep_x[1..].to_vec(),
                given_name: Some("X".into()),
                active_until: Some(20),
            },
        );
        initial.dreps.insert(
            drep_y.clone(),
            DRep {
                hash_bytes: drep_y[1..].to_vec(),
                given_name: Some("Y".into()),
                active_until: Some(30),
            },
        );
        // Maps apply_block carries unchanged (verify they survive a block).
        initial.decimals.insert("fp1".into(), 6);
        initial
            .handle_by_address
            .insert(hex::encode(&addr1), vec!["alice".into()]);
        initial
            .address_by_handle
            .insert("alice".into(), hex::encode(&addr1));
        initial
            .gov_action_titles
            .insert("txg#0".into(), "Title".into());

        let mut state = State::new(Url::parse("postgresql:///test").unwrap());
        state.restore_from_snapshot(initial.clone());

        // ---- The fake block: exercises every apply_block input. ----
        let produced = vec![((txh2.clone(), 0i16), txout(3_000_000, &addr3, p2n2(7)))];
        let consumed = vec![((txh1.clone(), 0i16), txout(1_000_000, &addr1, p1n1(10)))];
        // cred_a re-delegates pool_p -> pool_q (stays in set); cred_c newly delegates to pool_q.
        let pool_deleg = vec![
            (cred_a.clone(), Some(pool_q.clone())),
            (cred_c.clone(), Some(pool_q.clone())),
        ];
        // cred_a undelegates its drep; cred_b newly delegates to drep_x.
        let drep_deleg = vec![
            (cred_a.clone(), None),
            (cred_b.clone(), Some(drep_x.clone())),
        ];
        // pool_q registered (new); pool_p re-registered (must carry ticker + lifetime blocks).
        let pool_updates: Vec<PoolUpdate> = vec![
            (pool_q.clone(), 2000, 500, 1, 100),
            (pool_p.clone(), 1500, 400, 5, 100),
        ];
        let pool_retire = vec![(pool_p.clone(), 15u64)]; // applied after the re-registration
        let stake_changes = vec![
            (cred_a.clone(), 25i64),
            (cred_c.clone(), 200i64),
            (cred_b.clone(), -10i64),
        ];
        let withdrawals = vec![(cred_a.clone(), 3i64), (cred_b.clone(), 1i64)];
        let mut reward_deltas = HashMap::new();
        reward_deltas.insert(cred_a.clone(), 1i64);
        reward_deltas.insert(cred_c.clone(), 10i64);
        let mut drep_active = HashMap::new();
        drep_active.insert(drep_x.clone(), 25i64); // drep_y absent -> expires

        state.apply_block(BlockUpdate {
            slot: 200,
            block_hash: "hash200".into(),
            epoch: 11,
            produced,
            consumed: &consumed,
            pool_delegation_changes: &pool_deleg,
            drep_delegation_changes: &drep_deleg,
            pool_updates: &pool_updates,
            pool_retirements: &pool_retire,
            issuer_pool_hash: Some(&pool_q),
            stake_changes: &stake_changes,
            withdrawal_changes: &withdrawals,
            reward_deltas: Some(&reward_deltas),
            drep_active_until: Some(&drep_active),
        });

        // ---- Forward: every tracked value updated exactly. ----
        let cur = state.current().unwrap();
        assert_eq!(cur.slot, 200);
        assert_eq!(cur.block_hash.as_deref(), Some("hash200"));
        assert_eq!(cur.last_epoch, Some(11));

        // UTXOs: consumed dropped, produced inserted, untouched kept.
        assert_eq!(cur.utxos.len(), 2);
        assert!(cur.utxos.contains_key(&(txh0.clone(), 0)));
        assert!(cur.utxos.contains_key(&(txh2.clone(), 0)));
        assert!(!cur.utxos.contains_key(&(txh1.clone(), 0)));
        assert_eq!(
            cur.utxos.get(&(txh2.clone(), 0)).unwrap().lovelaces,
            Decimal::from(3_000_000u64)
        );

        // Address balances: addr1 spent to 0 (pruned), addr2 untouched, addr3 credited.
        assert_eq!(cur.address_balances.get(&addr1).copied(), None);
        assert_eq!(cur.address_balances.get(&addr2).copied(), Some(2_000_000));
        assert_eq!(cur.address_balances.get(&addr3).copied(), Some(3_000_000));

        // Asset holdings: addr1's token removed, addr2 untouched, addr3's token added.
        assert!(cur.address_held_assets(&addr1).is_empty());
        assert_eq!(
            cur.address_held_assets(&addr2),
            vec![(policy1.clone(), name1.clone(), 3u128)]
        );
        assert_eq!(
            cur.address_held_assets(&addr3),
            vec![(policy2.clone(), name2.clone(), 7u128)]
        );

        // Pools: pool_q new + minted this block; pool_p carried blocks/ticker, params updated, retiring set.
        let pq = cur.pools.get(&hex::encode(&pool_q)).unwrap();
        assert_eq!(pq.blocks, 1);
        assert_eq!(pq.ticker, None);
        assert_eq!(pq.retiring_epoch, None);
        let pp = cur.pools.get(&hex::encode(&pool_p)).unwrap();
        assert_eq!(pp.blocks, 5);
        assert_eq!(pp.ticker.as_deref(), Some("AAA"));
        assert_eq!(pp.retiring_epoch, Some(15));
        assert_eq!(pp.pledge, Decimal::from(1500u64));

        // Pool delegations / delegators / live stake.
        assert_eq!(cur.pool_delegations.get(&cred_a), Some(&pool_q));
        assert_eq!(cur.pool_delegations.get(&cred_c), Some(&pool_q));
        let q_del = cur.pool_delegators.get(&pool_q).unwrap();
        assert!(q_del.len() == 2 && q_del.contains(&cred_a) && q_del.contains(&cred_c));
        assert!(cur.pool_delegators.get(&pool_p).unwrap().is_empty());
        assert_eq!(State::pool_live_stake(cur, &pool_q), Some(338)); // (125+3)+(200+10)
        assert_eq!(State::pool_live_stake(cur, &pool_p), Some(0));

        // DRep delegations / delegators / live stake.
        assert_eq!(cur.drep_delegations.get(&cred_a), None);
        assert_eq!(cur.drep_delegations.get(&cred_b), Some(&drep_x));
        let x_del = cur.drep_delegators.get(&drep_x).unwrap();
        assert!(x_del.len() == 1 && x_del.contains(&cred_b));
        assert_eq!(State::drep_live_stake(cur, &drep_x), Some(41)); // 40 + 1

        // Stakes.
        assert_eq!(cur.stakes.get(&cred_a).copied(), Some(125));
        assert_eq!(cur.stakes.get(&cred_b).copied(), Some(40));
        assert_eq!(cur.stakes.get(&cred_c).copied(), Some(200));

        // Rewards: withdrawals applied first, then epoch accruals.
        assert_eq!(cur.rewards.get(&cred_a).copied(), Some(3)); // 5 - 3 + 1
        assert_eq!(cur.rewards.get(&cred_b).copied(), Some(1)); // 2 - 1
        assert_eq!(cur.rewards.get(&cred_c).copied(), Some(10)); // 0 + 10

        // DReps refreshed at the epoch boundary (drep_y absent -> expired).
        assert_eq!(cur.dreps.get(&drep_x).unwrap().active_until, Some(25));
        assert_eq!(cur.dreps.get(&drep_y).unwrap().active_until, None);

        // total_staked: only cred_c newly enters the pool-delegated set (+210).
        assert_eq!(cur.total_staked, 315);

        // Carried-through maps unchanged by the block.
        assert_eq!(cur.decimals, initial.decimals);
        assert_eq!(cur.handle_by_address, initial.handle_by_address);
        assert_eq!(cur.address_by_handle, initial.address_by_handle);
        assert_eq!(cur.gov_action_titles, initial.gov_action_titles);
        assert!(cur.address_balances_populated && cur.asset_holdings_populated);

        // ---- Rollback: the entire snapshot returns to the initial values. ----
        assert!(state.rollback(100));
        assert!(
            *state.current().unwrap() == initial,
            "rollback did not restore the initial snapshot exactly"
        );
    }

    /// Regression for the intra-block debit-drop bug: a UTXO created *and* spent within one
    /// block must net to zero. `apply_block` applies produced before consumed, so the debit
    /// finds the freshly-credited entry — no over-counted balance, no phantom held asset, no
    /// phantom UTXO. (The old consumed-first order dropped the debit when the address started
    /// the block with no entry, inflating `address_balances`/`asset_holdings`.)
    #[test]
    fn apply_block_intra_block_create_and_spend_nets_to_zero() {
        use rust_decimal::Decimal;

        // 57-byte base address that starts the block with a zero balance (absent from the
        // maps) — the exact case the bug over-counted.
        let addr = [&[0x00u8][..], &[0x50; 28], &[0x60; 28]].concat();
        let cred = addr[29..57].to_vec();
        let policy = vec![0x11u8; 28];
        let name = b"TOK".to_vec();
        let assets = |q: u64| -> crate::model::PolicyAssets {
            vec![(policy.clone(), vec![(name.clone(), q)])]
        };
        let txout = |lov: u64| TxOutput {
            lovelaces: Decimal::from(lov),
            address: addr.clone(),
            assets: assets(5),
        };
        let txh = vec![0xaau8; 32];

        let initial = BlockSnapshot {
            slot: 100,
            block_hash: Some("h100".into()),
            last_epoch: Some(10),
            address_balances_populated: true,
            asset_holdings_populated: true,
            ..BlockSnapshot::default()
        };
        let mut state = State::new(Url::parse("postgresql:///test").unwrap());
        state.restore_from_snapshot(initial);

        // The same (txh, 0) UTXO is both produced and consumed in this block; the sink emits
        // matching +/- stake deltas for it.
        let produced = vec![((txh.clone(), 0i16), txout(500_000))];
        let consumed = vec![((txh.clone(), 0i16), txout(500_000))];
        let stake_changes = vec![(cred.clone(), 500_000i64), (cred.clone(), -500_000i64)];

        state.apply_block(BlockUpdate {
            slot: 200,
            block_hash: "h200".into(),
            epoch: 10,
            produced,
            consumed: &consumed,
            pool_delegation_changes: &[],
            drep_delegation_changes: &[],
            pool_updates: &[],
            pool_retirements: &[],
            issuer_pool_hash: None,
            stake_changes: &stake_changes,
            withdrawal_changes: &[],
            reward_deltas: None,
            drep_active_until: None,
        });

        let cur = state.current().unwrap();
        assert!(
            !cur.utxos.contains_key(&(txh.clone(), 0)),
            "created-and-spent UTXO must not linger in the utxo map"
        );
        assert_eq!(
            cur.address_balances.get(&addr).copied(),
            None,
            "balance over-counted: the intra-block debit was dropped"
        );
        assert!(
            cur.address_held_assets(&addr).is_empty(),
            "held asset over-counted: the intra-block debit was dropped"
        );
        assert_eq!(cur.stakes.get(&cred).copied(), Some(0));
    }

    /// `write_snapshot` → `load_snapshot` round-trips the whole snapshot (including a
    /// holding above `u64`, exercising the variable-length `Qty` serde), and load rejects
    /// a wrong network magic or an incompatible `SNAPSHOT_FORMAT`.
    #[test]
    fn snapshot_roundtrips_and_gates_on_magic_and_format() {
        use rust_decimal::Decimal;

        let mut snap = BlockSnapshot {
            slot: 4242,
            block_hash: Some("deadbeef".into()),
            last_epoch: Some(7),
            total_staked: 9_999,
            address_balances_populated: true,
            asset_holdings_populated: true,
            ..BlockSnapshot::default()
        };
        snap.stakes.insert(vec![0xaa; 28], 123);
        snap.rewards.insert(vec![0xaa; 28], 45);
        snap.address_balances.insert(vec![0x01; 57], 7_000_000);
        snap.utxos.insert(
            (vec![0xf1; 32], 0),
            TxOutput {
                lovelaces: Decimal::from(7_000_000u64),
                address: vec![0x01; 57],
                assets: vec![],
            },
        );
        // A holding above u64 — the variable-length Qty path must survive serialization.
        let big = u128::from(u64::MAX) + 1000;
        let hkey = (
            (Some(vec![0xcc; 28]), vec![0x01; 57]),
            asset_id(&[0x11; 28], b"BIG"),
        );
        snap.asset_holdings.insert(hkey.clone(), Qty(big));
        let mut poolp = Pool::from_registration(vec![0x01; 28], 1, 2, 3, 100);
        poolp.ticker = Some("TICK".into());
        poolp.blocks = 9;
        snap.pools.insert(hex::encode([0x01u8; 28]), poolp);
        snap.dreps.insert(
            [&[0x00u8][..], &[0xd1u8; 28]].concat(),
            DRep {
                hash_bytes: vec![0xd1; 28],
                given_name: Some("D".into()),
                active_until: Some(40),
            },
        );
        snap.decimals.insert("fp".into(), 6);

        let mut fi = FeedIndex::new();
        fi.add_pool_minted(
            vec![0x01; 28],
            crate::state::feed_index::BlockRef {
                slot: 4240,
                hash: "h".into(),
                number: 1,
            },
        );

        let magic = 764_824_073u64;
        let path = std::env::temp_dir().join("poolpm_snapshot_roundtrip_test.bin");
        write_snapshot(&path, &snap, &fi, magic).unwrap();

        // Wrong network magic → rejected.
        assert!(State::load_snapshot(&path, magic + 1).is_none());

        // Correct magic → the entire snapshot round-trips, including the >u64 holding.
        let (loaded, loaded_fi) = State::load_snapshot(&path, magic).unwrap();
        assert!(loaded == snap, "snapshot did not round-trip exactly");
        assert_eq!(loaded.asset_holdings.get(&hkey).unwrap().0, big);
        assert_eq!(loaded_fi.pool_minted_blocks(&[0x01; 28]).len(), 1);

        // A snapshot tagged with a different SNAPSHOT_FORMAT → rejected (forces rebuild).
        let bytes = rmp_serde::to_vec(&(&snap, &fi, magic, SNAPSHOT_FORMAT + 1)).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(State::load_snapshot(&path, magic).is_none());

        let _ = std::fs::remove_file(&path);
    }

    /// In-memory asset-holding accessors (the replacements for the old db `COUNT(DISTINCT)`
    /// / `SUM`): a stake credential's count is the *union* over its addresses (same asset on
    /// two addresses counts once) and its held amounts are *summed*; enterprise (no-stake)
    /// addresses are excluded from the credential's view.
    #[test]
    fn asset_holdings_stake_level_accessors_dedup_and_sum() {
        let cred = vec![0xcc; 28];
        let addr_x = [&[0x01u8][..], &[0x71; 28], &cred].concat(); // base, stake = cred
        let addr_y = [&[0x01u8][..], &[0x72; 28], &cred].concat(); // same stake cred
        let addr_e = [&[0x61u8][..], &[0x7e; 28]].concat(); // enterprise, no stake
        let pa = vec![0x11u8; 28];
        let pb = vec![0x22u8; 28];
        let na = b"A".to_vec();
        let nb = b"B".to_vec();

        let mut snap = BlockSnapshot::default();
        // addr_x: pa/na x10, pb/nb x5 ; addr_y: pa/na x3 (shared) ; addr_e: pa/na x100 (excluded).
        apply_utxo_assets(
            &addr_x,
            &vec![
                (pa.clone(), vec![(na.clone(), 10)]),
                (pb.clone(), vec![(nb.clone(), 5)]),
            ],
            true,
            &mut snap.asset_holdings,
        );
        apply_utxo_assets(
            &addr_y,
            &vec![(pa.clone(), vec![(na.clone(), 3)])],
            true,
            &mut snap.asset_holdings,
        );
        apply_utxo_assets(
            &addr_e,
            &vec![(pa.clone(), vec![(na.clone(), 100)])],
            true,
            &mut snap.asset_holdings,
        );

        // Per-address counts.
        assert_eq!(snap.address_asset_count(&addr_x), 2);
        assert_eq!(snap.address_asset_count(&addr_y), 1);

        // Stake-level: pa/na appears on both addresses → counted once; total distinct = 2.
        assert_eq!(snap.stake_asset_count(&cred), 2);

        // Stake-level held amounts: pa/na summed across addresses (10 + 3 = 13), pb/nb = 5.
        let mut held = snap.stake_held_assets(&cred);
        held.sort();
        assert_eq!(
            held,
            vec![
                (pa.clone(), na.clone(), 13u128),
                (pb.clone(), nb.clone(), 5u128)
            ]
        );

        // Token-quantity lookups.
        assert_eq!(stake_token_qty(&snap.asset_holdings, &cred, &pa, &na), 13);
        assert_eq!(stake_token_qty(&snap.asset_holdings, &cred, &pb, &nb), 5);
        let key_x: AssetKey = (Some(cred.clone()), addr_x.clone());
        let key_y: AssetKey = (Some(cred.clone()), addr_y.clone());
        assert_eq!(
            address_token_qty(&snap.asset_holdings, &key_x, &pa, &na),
            10
        );
        assert_eq!(address_token_qty(&snap.asset_holdings, &key_y, &pa, &na), 3);
        assert_eq!(address_token_qty(&snap.asset_holdings, &key_x, &pb, &nb), 5);

        // The enterprise address's 100 of pa/na is not attributed to the credential.
        assert_eq!(stake_token_qty(&snap.asset_holdings, &cred, &pa, &na), 13);
    }

    #[test]
    fn epoch_for_slot_preprod_and_preview() {
        // Preview: Shelley from genesis (epoch 0), 1-day epochs (86_400 one-second slots).
        let preview = GenesisValues::preview();
        assert_eq!(State::epoch_for_slot(0, &preview), 0);
        assert_eq!(State::epoch_for_slot(86_400, &preview), 1);
        assert_eq!(State::epoch_for_slot(86_400 * 50, &preview), 50);

        // Pre-prod: Shelley started at epoch 4 (slot 86_400), 5-day epochs (432_000 slots).
        let preprod = GenesisValues::preprod();
        assert_eq!(State::epoch_for_slot(86_400, &preprod), 4);
        assert_eq!(State::epoch_for_slot(86_400 + 432_000, &preprod), 5);
        assert_eq!(State::epoch_for_slot(86_400 + 432_000 * 200, &preprod), 204);
    }
}
