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

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// Global per-address multi-asset holdings: `(stake credential | None for
    /// enterprise, payment-address bytes) → policy → name → #unspent UTXOs holding it`
    /// (see [`HeldAssets`] — the leaf is grouped by policy like a UTXO, binary
    /// throughout). Sorted by credential first, so every payment address sharing a stake
    /// key forms a contiguous prefix range — the stake-page count is that prefix's union
    /// of `(policy, name)` pairs (derived on demand, never pre-aggregated, so each asset
    /// is stored *once*). The address-page count is one O(1) lookup's token total. A key
    /// exists iff the address currently holds ≥1 token; a leaf `0→1` is a freshly held
    /// token, `1→0` means it fully left. Fully populated at `reset()` from db-sync,
    /// serialized into the snapshot (warm resume skips the populate), maintained per
    /// block by the sink, and — being a snapshot field — reverted automatically on
    /// rollback. ADA keeps its two maps (`address_balances` + `stakes`) because it has a
    /// hot aggregation path; assets don't, so one composite map suffices.
    #[serde(default)]
    pub asset_holdings: OrdMap<AssetKey, HeldAssets>,
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

/// One subject's held assets, grouped by policy exactly like a UTXO's
/// [`crate::model::PolicyAssets`]: 28-byte policy id → asset name bytes → number of
/// unspent UTXOs holding that token. Binary throughout (the CIP-14 fingerprint is
/// derived from policy+name on demand); grouping by policy stores the policy once.
/// Both levels are `OrdMap` (sorted, with `diff` + `ptr_eq`) so a connection can derive
/// its live tile changes by structurally diffing this against the previous snapshot —
/// O(actual changes), skipping shared subtrees — see [`held_diff`]. Serializes
/// identically to a `HashMap` (rmp encodes both as a map), so the on-disk snapshot is
/// unaffected.
pub type HeldAssets = OrdMap<Vec<u8>, OrdMap<Vec<u8>, u64>>;

impl BlockSnapshot {
    /// Distinct multi-assets currently held by one payment address — an O(1) lookup,
    /// then the sum of each policy's token count.
    pub fn address_asset_count(&self, address: &[u8]) -> u32 {
        let cred = stake_credential_from_address_bytes(address);
        self.asset_holdings
            .get(&(cred, address.to_vec()))
            .map(held_asset_count)
            .unwrap_or(0)
    }

    /// Distinct multi-assets held across every payment address sharing a 28-byte stake
    /// credential — the union of `(policy, name)` pairs over the credential's contiguous
    /// prefix range. In-memory (~ms even for a 167k-asset whale) vs the old
    /// `COUNT(DISTINCT)` db query (tens of seconds).
    pub fn stake_asset_count(&self, cred: &[u8]) -> u32 {
        let mut union: std::collections::HashSet<(&[u8], &[u8])> = std::collections::HashSet::new();
        for (_addr, held) in cred_range(&self.asset_holdings, cred) {
            for (policy, names) in held {
                for name in names.keys() {
                    union.insert((policy.as_slice(), name.as_slice()));
                }
            }
        }
        union.len() as u32
    }

    /// Every `(policy, name, quantity)` token currently held by a payment address — the
    /// rows the owned-assets grid renders, straight from memory (no db scan). Unsorted;
    /// the caller sorts and paginates.
    pub fn address_held_assets(&self, address: &[u8]) -> Vec<(Vec<u8>, Vec<u8>, u64)> {
        let cred = stake_credential_from_address_bytes(address);
        let mut out = Vec::new();
        if let Some(held) = self.asset_holdings.get(&(cred, address.to_vec())) {
            for (policy, names) in held {
                for (name, qty) in names {
                    out.push((policy.clone(), name.clone(), *qty));
                }
            }
        }
        out
    }

    /// Distinct `(policy, name, quantity)` tokens held across every payment address
    /// sharing a stake credential — the same asset on two of the credential's addresses
    /// is one owned asset, with the quantities summed. Unsorted; the caller paginates.
    pub fn stake_held_assets(&self, cred: &[u8]) -> Vec<(Vec<u8>, Vec<u8>, u64)> {
        let mut sums: std::collections::HashMap<(Vec<u8>, Vec<u8>), u64> =
            std::collections::HashMap::new();
        for (_addr, held) in cred_range(&self.asset_holdings, cred) {
            for (policy, names) in held {
                for (name, qty) in names {
                    *sums.entry((policy.clone(), name.clone())).or_insert(0) += qty;
                }
            }
        }
        sums.into_iter().map(|((p, n), q)| (p, n, q)).collect()
    }
}

/// Quantity of one `(policy, name)` token held by a single payment-address key (0 if not
/// held). For resolving a live tile delta's amount from the current snapshot.
pub fn address_token_qty(
    holdings: &AssetHoldings,
    key: &AssetKey,
    policy: &[u8],
    name: &[u8],
) -> u64 {
    holdings
        .get(key)
        .and_then(|held| held.get(policy))
        .and_then(|names| names.get(name))
        .copied()
        .unwrap_or(0)
}

/// Quantity of one `(policy, name)` token summed across every payment address sharing a
/// stake credential — the stake-level owned amount.
pub fn stake_token_qty(holdings: &AssetHoldings, cred: &[u8], policy: &[u8], name: &[u8]) -> u64 {
    cred_range(holdings, cred)
        .filter_map(|(_addr, held)| held.get(policy).and_then(|names| names.get(name)).copied())
        .sum()
}

/// Total distinct tokens in a subject's [`HeldAssets`] — `Σ` over policies of the
/// policy's token count.
fn held_asset_count(held: &HeldAssets) -> u32 {
    held.values().map(|names| names.len() as u32).sum()
}

/// On-disk snapshot format version. Bump on any breaking change to a persisted field's
/// shape/semantics that rmp can't catch (it tolerates int-width changes) — a mismatch is
/// rejected on load so the state rebuilds from db-sync. v2: `asset_holdings` leaf went
/// from UTXO count to summed held quantity.
const SNAPSHOT_FORMAT: u32 = 2;

/// Serialize `(snap, feed_index, magic, format)` and write it atomically (temp file +
/// rename, so a crash mid-write leaves the previous snapshot intact). Free fn so it can
/// run on a `spawn_blocking` thread from owned clones, without the `chain_state` lock or
/// `&self`. Returns the persisted slot.
pub fn write_snapshot(
    path: &Path,
    snap: &BlockSnapshot,
    feed_index: &FeedIndex,
    network_magic: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let data = rmp_serde::to_vec(&(snap, feed_index, network_magic, SNAPSHOT_FORMAT))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &data)?;
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
pub type AssetHoldings = OrdMap<AssetKey, HeldAssets>;

/// Apply a single token's `+1`/`-1` to one address's `policy → name → unspent-UTXO count`
/// in the global holdings map. Empty name/policy/address entries are pruned, so the map's
/// keys stay exactly the addresses currently holding ≥1 token. The live tile deltas are
/// *not* emitted here — each open assets page derives them by diffing snapshots
/// ([`held_diff`]); this only maintains the map.
fn bump_one(
    holdings: &mut OrdMap<AssetKey, HeldAssets>,
    key: &AssetKey,
    policy: &[u8],
    name: &[u8],
    qty: u64,
    add: bool,
) {
    if add {
        let held = holdings.entry(key.clone()).or_default();
        let names = held.entry(policy.to_vec()).or_default();
        *names.entry(name.to_vec()).or_insert(0) += qty;
    } else {
        let Some(held) = holdings.get_mut(key) else {
            return;
        };
        let Some(names) = held.get_mut(policy) else {
            return;
        };
        let Some(c) = names.get_mut(name) else { return };
        // produced/consumed amounts balance exactly; saturate as a guard against
        // any stray underflow rather than panic.
        *c = c.saturating_sub(qty);
        if *c == 0 {
            names.remove(name);
            if names.is_empty() {
                held.remove(policy);
            }
            if held.is_empty() {
                holdings.remove(key);
            }
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
    holdings: &mut OrdMap<AssetKey, HeldAssets>,
) {
    if assets.is_empty() {
        return;
    }
    let cred = stake_credential_from_address_bytes(address);
    let key: AssetKey = (cred, address.to_vec());
    for (policy, names) in assets {
        for (name, qty) in names {
            bump_one(holdings, &key, policy, name, *qty, add);
        }
    }
}

/// The credential's payment-address holdings — the contiguous `(Some(cred), …)` prefix of
/// the sorted map, in address order. Shared by the count/grid helpers and the live diff.
fn cred_range<'a>(
    holdings: &'a OrdMap<AssetKey, HeldAssets>,
    cred: &'a [u8],
) -> impl Iterator<Item = (&'a [u8], &'a HeldAssets)> {
    let start: AssetKey = (Some(cred.to_vec()), Vec::new());
    holdings
        .range(start..)
        .take_while(move |((c, _), _)| c.as_deref() == Some(cred))
        .map(|((_, addr), held)| (addr.as_slice(), held))
}

/// Does any payment address sharing `cred` *other than* `exclude` currently hold the
/// `(policy, name)` token? A prefix range-scan over the credential's contiguous keys,
/// O(#addresses of the credential). Used by [`stake_tile_diff`] to lift an address-level
/// gain/loss to the stake's union.
fn stake_holds(
    holdings: &OrdMap<AssetKey, HeldAssets>,
    cred: &[u8],
    exclude: &[u8],
    policy: &[u8],
    name: &[u8],
) -> bool {
    cred_range(holdings, cred).any(|(addr, held)| {
        addr != exclude
            && held
                .get(policy)
                .is_some_and(|names| names.contains_key(name))
    })
}

/// Accumulate the `(policy, name)` tokens added/removed between two `HeldAssets`
/// snapshots into `added`/`removed`, via imbl's structural `diff` — shared subtrees are
/// skipped, so the cost is O(actual changes). At the name level a key that appears is a
/// tile add, one that disappears (we prune at count 0) is a tile remove; a count change
/// with the key still present is no tile change.
fn held_diff(
    prev: &HeldAssets,
    curr: &HeldAssets,
    added: &mut Vec<Token>,
    removed: &mut Vec<Token>,
) {
    use imbl::ordmap::DiffItem;
    for d in prev.diff(curr) {
        match d {
            DiffItem::Add(policy, names) => {
                added.extend(names.keys().map(|n| (policy.clone(), n.clone())))
            }
            DiffItem::Remove(policy, names) => {
                removed.extend(names.keys().map(|n| (policy.clone(), n.clone())))
            }
            DiffItem::Update {
                old: (policy, old_names),
                new: (_, new_names),
            } => {
                for nd in old_names.diff(new_names) {
                    match nd {
                        DiffItem::Add(name, _) => added.push((policy.clone(), name.clone())),
                        DiffItem::Remove(name, _) => removed.push((policy.clone(), name.clone())),
                        DiffItem::Update { .. } => {}
                    }
                }
            }
        }
    }
}

/// Live tile changes for one payment address between two holdings snapshots: `(added,
/// removed)` `(policy, name)` tokens. O(1) when the address's entry is structurally
/// unchanged (shared node).
pub fn address_tile_diff(
    prev: &AssetHoldings,
    curr: &AssetHoldings,
    key: &AssetKey,
) -> (Vec<Token>, Vec<Token>) {
    let (p, c) = (prev.get(key), curr.get(key));
    if let (Some(p), Some(c)) = (p, c) {
        if p.ptr_eq(c) {
            return (Vec::new(), Vec::new());
        }
    }
    let empty = HeldAssets::new();
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    held_diff(
        p.unwrap_or(&empty),
        c.unwrap_or(&empty),
        &mut added,
        &mut removed,
    );
    (added, removed)
}

/// Live tile changes for a stake credential (the union over its addresses) between two
/// holdings snapshots. A token is on the stake's grid iff *any* of the credential's
/// addresses holds it, so a per-address gain/loss is a stake change only when the
/// credential-wide membership flips. We diff each changed address to find the *candidate*
/// tokens (cheap — `ptr_eq` skips unchanged addresses), dedupe them, then keep only those
/// whose union membership actually flipped between `prev` and `curr` (so two addresses
/// gaining the same token in one step is a single add, not two).
pub fn stake_tile_diff(
    prev: &AssetHoldings,
    curr: &AssetHoldings,
    cred: &[u8],
) -> (Vec<Token>, Vec<Token>) {
    use std::collections::{HashMap, HashSet};
    let prev_addrs: HashMap<&[u8], &HeldAssets> = cred_range(prev, cred).collect();
    let curr_addrs: HashMap<&[u8], &HeldAssets> = cred_range(curr, cred).collect();
    let empty = HeldAssets::new();
    let mut candidates: HashSet<Token> = HashSet::new();
    let addrs: HashSet<&[u8]> = prev_addrs
        .keys()
        .chain(curr_addrs.keys())
        .copied()
        .collect();
    for addr in addrs {
        let (p, c) = (prev_addrs.get(addr).copied(), curr_addrs.get(addr).copied());
        if let (Some(p), Some(c)) = (p, c) {
            if p.ptr_eq(c) {
                continue;
            }
        }
        // At the stake level an add and a remove are both just "this token changed on
        // some address" — collect them together as candidates.
        let (mut a_add, mut a_rem) = (Vec::new(), Vec::new());
        held_diff(
            p.unwrap_or(&empty),
            c.unwrap_or(&empty),
            &mut a_add,
            &mut a_rem,
        );
        candidates.extend(a_add);
        candidates.extend(a_rem);
    }
    // `stake_holds(.., &[], ..)` = "any address of the credential holds it" (no real
    // address is the empty slice), i.e. union membership.
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for (policy, name) in candidates {
        let in_prev = stake_holds(prev, cred, &[], &policy, &name);
        let in_curr = stake_holds(curr, cred, &[], &policy, &name);
        match (in_prev, in_curr) {
            (false, true) => added.push((policy, name)),
            (true, false) => removed.push((policy, name)),
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
    ) -> Result<OrdMap<AssetKey, HeldAssets>, sqlx::Error> {
        use pallas::ledger::addresses::Address;
        let mut holdings: OrdMap<AssetKey, HeldAssets> = OrdMap::new();
        let mut cur_addr: Option<String> = None;
        let mut cur_key: Option<AssetKey> = None;
        let mut held = HeldAssets::new();
        let mut rows: u64 = 0;
        db.asset_holdings_for_each(last_tx_id, |addr, policy, name, count| {
            rows += 1;
            if cur_addr.as_deref() != Some(addr.as_str()) {
                if let Some(k) = cur_key.take() {
                    if !held.is_empty() {
                        holdings.insert(k, std::mem::take(&mut held));
                    }
                }
                cur_key = Address::from_bech32(&addr).ok().map(|a| {
                    let bytes = a.to_vec();
                    (stake_credential_from_address_bytes(&bytes), bytes)
                });
                cur_addr = Some(addr);
            }
            if cur_key.is_some() && count > 0 {
                held.entry(policy).or_default().insert(name, count as u64);
            }
        })
        .await?;
        if let Some(k) = cur_key.take() {
            if !held.is_empty() {
                holdings.insert(k, held);
            }
        }
        tracing::info!(
            rows,
            addresses = holdings.len(),
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

        tracing::info!("Fetching pools...");
        let pools = db.pools(last_tx_id, slot as i64).await?;
        tracing::info!("{} pools retrieved", pools.len());

        tracing::info!("Fetching pool delegations...");
        let (pool_delegations, pool_delegators) = db.pool_delegations(last_tx_id).await?;
        tracing::info!(
            "{} pool delegations in {} pools retrieved",
            pool_delegations.len(),
            pool_delegators.len()
        );

        tracing::info!("Fetching DRep delegations...");
        let (drep_delegations, drep_delegators) = db.drep_delegations(last_tx_id).await?;
        tracing::info!(
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
        tracing::info!("{} stake addresses with rewards", rewards.len());

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
        tracing::info!("{} addresses with UTXOs", balance_rows.len());
        let (address_balances, stakes) = Self::balances_and_stakes(balance_rows);
        tracing::info!(
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
    /// Live asset-grid deltas aren't produced here — each open assets page derives its
    /// own by diffing `asset_holdings` against the previous snapshot ([`held_diff`]).
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
        for (key, output) in produced {
            let bal: i64 = output
                .lovelaces
                .try_into()
                .expect("lovelace value must fit i64");
            *address_balances.entry(output.address.clone()).or_insert(0) += bal;
            apply_utxo_assets(&output.address, &output.assets, true, &mut asset_holdings);
            utxos.insert(key, output);
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
    pub fn load_snapshot(path: &Path, network_magic: u64) -> Option<(BlockSnapshot, FeedIndex)> {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!("failed to read snapshot from {}: {}", path.display(), e);
                return None;
            }
        };
        tracing::info!("loading snapshot from {}...", path.display());
        match rmp_serde::from_slice::<(BlockSnapshot, FeedIndex, u64, u32)>(&data) {
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
        let mut holdings: OrdMap<AssetKey, HeldAssets> = OrdMap::new();
        let key: AssetKey = (None, b"addr".to_vec());
        let policy = vec![0xaau8; 28];
        let name = b"TOKEN".to_vec();

        bump_one(&mut holdings, &key, &policy, &name, 1, true);
        assert_eq!(holdings[&key][&policy][&name], 1);
        bump_one(&mut holdings, &key, &policy, &name, 1, true);
        assert_eq!(holdings[&key][&policy][&name], 2);
        bump_one(&mut holdings, &key, &policy, &name, 1, false);
        assert_eq!(holdings[&key][&policy][&name], 1);

        // Last UTXO spent → name/policy dropped and the (now-empty) address key removed,
        // so the map's keys stay exactly the addresses currently holding ≥1 token.
        bump_one(&mut holdings, &key, &policy, &name, 1, false);
        assert!(holdings.is_empty());

        // Spending a token the map never had is a no-op.
        bump_one(&mut holdings, &key, &policy, &name, 1, false);
        assert!(holdings.is_empty());
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
        let s0: OrdMap<AssetKey, HeldAssets> = OrdMap::new();
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
}
