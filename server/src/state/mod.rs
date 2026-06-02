mod dbsync;
pub mod feed_index;

use imbl::{hashmap::HashMap, hashset::HashSet, ordmap::OrdMap, vector::Vector};
use std::path::Path;
use url::Url;

use crate::cip26;
use crate::cip68;
use crate::model::{parse_virtual_handle_address, DRep, Pool, TxOutput, HANDLE_POLICIES};
use crate::pallas::{stake_credential_from_address_bytes, PoolUpdate};
use dbsync::DbSync;
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

    // --- Owned-asset cache for scan-bound addresses (see plan: Part 2) ---
    // Caches the asset holdings of the few addresses whose `/assets` db query is
    // too slow (many UTXOs / huge output history), so they serve from memory.
    // Assets are referenced by our own `AssetRef` (a `u32` index), NOT db-sync's
    // `multi_asset.id` (which is reassigned on reinstall / db-sync rollback).
    /// Fingerprint → `AssetRef`. Reuse-not-reassign: a fingerprint keeps its ref
    /// for the life of the snapshot history, so total refs ≤ on-chain distinct
    /// assets (≈11M ≪ u32). Append-mostly; reverted on rollback by history
    /// truncation along with everything else in the snapshot.
    #[serde(default)]
    pub asset_fp_to_ref: HashMap<String, u32>,
    /// `AssetRef` (index) → (fingerprint, asset name). One canonical copy per
    /// asset; `address_assets` and any future reference store the 4-byte ref.
    /// Names come from the warm db fetch or the block's produced outputs.
    #[serde(default)]
    pub asset_meta: Vector<(String, Vec<u8>)>,
    /// Cached payment address (raw bytes) → (`AssetRef` → summed quantity over
    /// its unconsumed UTXOs). Ordered by ref ⇒ pagination order (refs seeded in
    /// mint order at warm); `len()` = asset count; entries with quantity 0 are
    /// removed. Present only for the scan-bound set.
    #[serde(default)]
    pub address_assets: HashMap<Vec<u8>, OrdMap<u32, u64>>,
    /// Cached stake credential (28-byte, header stripped) → (`AssetRef` →
    /// summed quantity across **all** the stake's payment addresses). Warmed for
    /// stakes whose total unconsumed UTXOs ≥ threshold, so the union is complete
    /// (a stake's assets can span light addresses that wouldn't be cached
    /// individually). Drives `/stake1…/assets`. Shares the intern table.
    #[serde(default)]
    pub stake_assets: HashMap<Vec<u8>, OrdMap<u32, u64>>,
    /// Unspent-UTXO count for each **cached** payment address (the `address_assets`
    /// key set). The free membership signal (see `MIN_UTXOS_TO_CACHE`): seeded at
    /// warm from the `address_balances` scan's `COUNT(*)`, decremented/incremented
    /// per-block, and used to **demote** an address (drop it from `address_assets`)
    /// once it falls below the threshold. Holds only the few cached keys, not all
    /// addresses, so the per-block clone stays cheap.
    #[serde(default)]
    pub address_utxos: HashMap<Vec<u8>, u32>,
    /// Unspent-UTXO count (summed across its payment addresses) for each **cached**
    /// stake credential. Same role as `address_utxos` for the `stake_assets` set.
    #[serde(default)]
    pub stake_utxos: HashMap<Vec<u8>, u32>,
    /// True once `address_assets` has been warmed from db-sync for this history.
    /// Gates the one-shot background `populate_address_assets` on warm resume.
    #[serde(default)]
    pub address_assets_populated: bool,
}

impl BlockSnapshot {
    /// Look up the shortest ADA Handle for an address, if any.
    pub fn handle_for(&self, address: &str) -> Option<String> {
        self.handle_by_address
            .get(address)
            .and_then(|handles| handles.iter().min_by_key(|h| h.len()).map(|h| h.clone()))
    }
}

/// Cache an address's owned assets in `BlockSnapshot` once it holds at least
/// this many unconsumed UTXOs — the "scan-bound" set whose `/assets` db query is
/// too slow (see plan). Chosen from the mainnet UTXO-count distribution (≈286
/// addresses ≥1000); tunable. `unspent_utxos` is a free, load-independent proxy
/// for the scan cost: it's the `COUNT(*)` the `address_balances` scan now returns
/// alongside each balance, so the heavy set falls out of a scan we already run —
/// no separate discovery query. Maintained per-block in `address_utxos`, so an
/// address that drops below the threshold is demoted live; new addresses that
/// cross it are picked up at the next reset.
pub const MIN_UTXOS_TO_CACHE: i64 = 1000;

/// The owned-asset cache, warmed from db-sync at a single cutoff. Built by
/// [`State::warm_asset_cache`] from the `address_balances` scan rows and consumed
/// by both `reset()` (cold start) and `populate_address_assets()` (warm resume).
struct WarmedCache {
    address_balances: HashMap<Vec<u8>, i64>,
    asset_fp_to_ref: HashMap<String, u32>,
    asset_meta: Vector<(String, Vec<u8>)>,
    address_assets: HashMap<Vec<u8>, OrdMap<u32, u64>>,
    stake_assets: HashMap<Vec<u8>, OrdMap<u32, u64>>,
    address_utxos: HashMap<Vec<u8>, u32>,
    stake_utxos: HashMap<Vec<u8>, u32>,
}

/// Intern `rows` (pre-ordered by mint id so refs seed in mint order ⇒ ref DESC
/// = newest-first pagination) into the shared intern table and accumulate
/// `key → (ref → quantity)` into `target`. Used for both the per-address and
/// per-stake caches so they share one `AssetRef` space.
fn build_cache_into(
    fp_to_ref: &mut HashMap<String, u32>,
    meta: &mut Vector<(String, Vec<u8>)>,
    target: &mut HashMap<Vec<u8>, OrdMap<u32, u64>>,
    rows: Vec<(Vec<u8>, String, Vec<u8>, i64)>,
) {
    for (key, fp, name, qty) in rows {
        let r = intern_asset(fp_to_ref, meta, &fp, &name);
        let q = u64::try_from(qty).unwrap_or(0);
        target.entry(key).or_default().insert(r, q);
    }
}

/// Subtract spent asset quantities from a cached `key`'s holdings (no-op if the
/// key isn't cached), dropping any asset whose quantity reaches zero.
fn cache_consume(
    target: &mut HashMap<Vec<u8>, OrdMap<u32, u64>>,
    key: &[u8],
    assets: &[(String, u64)],
    fp_to_ref: &HashMap<String, u32>,
) {
    let Some(map) = target.get_mut(key) else {
        return;
    };
    for (fp, qty) in assets {
        if let Some(&r) = fp_to_ref.get(fp) {
            if let Some(cur) = map.get(&r).copied() {
                let next = cur.saturating_sub(*qty);
                if next == 0 {
                    map.remove(&r);
                } else {
                    map.insert(r, next);
                }
            }
        }
    }
}

/// Adjust a cached `key`'s unspent-UTXO count by `delta` (no-op if not cached —
/// we never promote live). Saturates at zero; `demote_below_threshold` then drops
/// keys that fell under `MIN_UTXOS_TO_CACHE`.
fn cache_utxo_delta(counts: &mut HashMap<Vec<u8>, u32>, key: &[u8], delta: i64) {
    if let Some(n) = counts.get_mut(key) {
        *n = (*n as i64 + delta).max(0) as u32;
    }
}

/// Drop every cached key whose unspent-UTXO count fell below the threshold from
/// both the count map and its holdings cache. Iterates the count map (bounded by
/// the cached-set size, ~hundreds), so it's cheap per block.
fn demote_below_threshold(
    counts: &mut HashMap<Vec<u8>, u32>,
    cache: &mut HashMap<Vec<u8>, OrdMap<u32, u64>>,
) {
    let demote: Vec<Vec<u8>> = counts
        .iter()
        .filter(|(_, &n)| (n as i64) < MIN_UTXOS_TO_CACHE)
        .map(|(k, _)| k.clone())
        .collect();
    for key in demote {
        counts.remove(&key);
        cache.remove(&key);
    }
}

/// Add produced asset quantities to a cached `key`'s holdings (no-op if the key
/// isn't cached), interning new assets with the block-provided name.
fn cache_produce(
    target: &mut HashMap<Vec<u8>, OrdMap<u32, u64>>,
    key: &[u8],
    assets: &[(String, u64)],
    fp_to_ref: &mut HashMap<String, u32>,
    meta: &mut Vector<(String, Vec<u8>)>,
    names: &std::collections::HashMap<String, Vec<u8>>,
) {
    if !target.contains_key(key) {
        return;
    }
    for (fp, qty) in assets {
        let name = names.get(fp).map(|n| n.as_slice()).unwrap_or(&[]);
        let r = intern_asset(fp_to_ref, meta, fp, name);
        let map = target.entry(key.to_vec()).or_default();
        let next = map.get(&r).copied().unwrap_or(0) + *qty;
        map.insert(r, next);
    }
}

/// Intern a fingerprint into the asset table, returning its `AssetRef`. Assigns
/// the next ref (storing `name`) on first sight; idempotent afterwards. Refs are
/// never reassigned within a history, so `meta.len()` ≤ on-chain distinct assets
/// (≈11M ≪ u32::MAX); the `expect` is therefore effectively unreachable.
fn intern_asset(
    fp_to_ref: &mut HashMap<String, u32>,
    meta: &mut Vector<(String, Vec<u8>)>,
    fingerprint: &str,
    name: &[u8],
) -> u32 {
    if let Some(&r) = fp_to_ref.get(fingerprint) {
        return r;
    }
    let r = u32::try_from(meta.len()).expect("asset intern table exceeded u32 capacity");
    meta.push_back((fingerprint.to_string(), name.to_vec()));
    fp_to_ref.insert(fingerprint.to_string(), r);
    r
}

pub struct State {
    history: Vec<BlockSnapshot>,
    db_url: Url,
    db: tokio::sync::OnceCell<DbSync>,
    pub feed_index: FeedIndex,
}

impl State {
    pub fn new(db_url: Url) -> Self {
        Self {
            history: Vec::new(),
            db_url,
            db: tokio::sync::OnceCell::new(),
            feed_index: FeedIndex::new(),
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
        // Keep only the balance field here (the UTXO count drives the asset cache,
        // warmed separately in `populate_address_assets`).
        use pallas::ledger::addresses::Address;
        let mut balances: HashMap<Vec<u8>, i64> = HashMap::new();
        for (bech32, balance, _n_utxos) in rows {
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

    /// Warm the owned-asset cache from db-sync if a loaded snapshot predates it
    /// (one-shot, gated on `address_assets_populated`). Mirrors
    /// `populate_address_balances`; subsequent blocks maintain it in
    /// `apply_block`. `reset()` warms inline, so this only fires on warm-resume
    /// from a pre-cache snapshot.
    pub async fn populate_address_assets(&mut self) {
        let already = self
            .history
            .last()
            .map(|s| s.address_assets_populated)
            .unwrap_or(false);
        if already {
            return;
        }
        let Some(snap_slot) = self.history.last().map(|s| s.slot) else {
            return;
        };
        let Some(db) = self.db().await else { return };
        let (last_tx_id, _) = match db.slot_info(snap_slot).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to fetch slot_info for asset cache: {e}");
                return;
            }
        };
        // Re-run the balance scan to recover the free per-address UTXO counts
        // (the loaded snapshot carries balances but not counts). Only on the rare
        // warm-resume from a pre-cache snapshot; `reset()` shares this via
        // `warm_asset_cache`.
        let balance_rows = match db.address_balances(last_tx_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to fetch balances for asset cache: {e}");
                return;
            }
        };
        let warmed = match Self::warm_asset_cache(db, last_tx_id, balance_rows).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to warm owned-asset cache: {e}");
                return;
            }
        };
        let snap = self.history.last_mut().unwrap();
        snap.asset_fp_to_ref = warmed.asset_fp_to_ref;
        snap.asset_meta = warmed.asset_meta;
        snap.address_assets = warmed.address_assets;
        snap.stake_assets = warmed.stake_assets;
        snap.address_utxos = warmed.address_utxos;
        snap.stake_utxos = warmed.stake_utxos;
        // `warmed.address_balances` is recomputed but the snapshot's is already
        // populated; leave it untouched.
        snap.address_assets_populated = true;
        tracing::info!(
            addresses = snap.address_assets.len(),
            stakes = snap.stake_assets.len(),
            assets = snap.asset_meta.len(),
            "owned-asset cache warmed from db-sync"
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

    /// One page of a cached (scan-bound) payment address's owned assets, newest
    /// first (asset ref DESC ≈ mint order), keyset-paginated after `cursor` (a
    /// ref previously returned, as i64). Returns `(ref, fingerprint, name)`.
    /// `None` if the address isn't cached — the caller falls back to the db
    /// query. Pure in-memory: pages the `OrdMap` and resolves names via the
    /// intern table, no db round-trip. Cheap enough to run under the read lock.
    pub fn cached_address_assets(
        &self,
        address: &[u8],
        cursor: Option<i64>,
        limit: i64,
    ) -> Option<Vec<(i64, String, Vec<u8>)>> {
        let snap = self.current()?;
        let map = snap.address_assets.get(address)?;
        let limit = limit.max(0) as usize;
        // Descending by ref, strictly below the cursor when set.
        let iter = match cursor {
            Some(c) => map.range(..(c.clamp(0, u32::MAX as i64) as u32)),
            None => map.range(u32::MIN..),
        };
        let mut out: Vec<(i64, String, Vec<u8>)> = Vec::with_capacity(limit.min(map.len()));
        for (&r, _qty) in iter.rev() {
            if out.len() >= limit {
                break;
            }
            if let Some((fp, name)) = snap.asset_meta.get(r as usize) {
                out.push((r as i64, fp.clone(), name.clone()));
            }
        }
        Some(out)
    }

    /// Distinct owned-asset count for a cached payment address (the `OrdMap`
    /// length), or `None` if not cached.
    pub fn cached_address_asset_count(&self, address: &[u8]) -> Option<u32> {
        let snap = self.current()?;
        snap.address_assets.get(address).map(|m| m.len() as u32)
    }

    /// One page of a cached (scan-bound) stake credential's owned assets — the
    /// union across all its payment addresses. `stake_cred` is the 28-byte
    /// credential (db-sync `hash_raw` with header stripped). Same paging/cursor
    /// semantics as `cached_address_assets`; `None` if the stake isn't cached.
    pub fn cached_stake_assets(
        &self,
        stake_cred: &[u8],
        cursor: Option<i64>,
        limit: i64,
    ) -> Option<Vec<(i64, String, Vec<u8>)>> {
        let snap = self.current()?;
        let map = snap.stake_assets.get(stake_cred)?;
        let limit = limit.max(0) as usize;
        let iter = match cursor {
            Some(c) => map.range(..(c.clamp(0, u32::MAX as i64) as u32)),
            None => map.range(u32::MIN..),
        };
        let mut out: Vec<(i64, String, Vec<u8>)> = Vec::with_capacity(limit.min(map.len()));
        for (&r, _qty) in iter.rev() {
            if out.len() >= limit {
                break;
            }
            if let Some((fp, name)) = snap.asset_meta.get(r as usize) {
                out.push((r as i64, fp.clone(), name.clone()));
            }
        }
        Some(out)
    }

    /// Distinct owned-asset count for a cached stake credential, or `None`.
    pub fn cached_stake_asset_count(&self, stake_cred: &[u8]) -> Option<u32> {
        let snap = self.current()?;
        snap.stake_assets.get(stake_cred).map(|m| m.len() as u32)
    }

    /// Build the owned-asset cache from the `address_balances` scan rows
    /// (`(bech32 address, lovelace, unspent_utxos)`). Both heavy sets are derived
    /// **from the free `unspent_utxos` counts** — no separate discovery scan
    /// (see plan: Part 2, L573): the heavy *addresses* are those over the
    /// threshold, the heavy *stakes* are those whose per-address counts sum over
    /// it. Holdings for exactly those sets are then fetched in one bulk pass each.
    /// Also returns the parsed `address_balances` field (the row parse is shared)
    /// and the per-cached-key UTXO counts that drive live demotion.
    async fn warm_asset_cache(
        db: &DbSync,
        last_tx_id: i64,
        balance_rows: Vec<(String, i64, i64)>,
    ) -> Result<WarmedCache, sqlx::Error> {
        use pallas::ledger::addresses::Address;

        let mut address_balances: HashMap<Vec<u8>, i64> = HashMap::new();
        let mut address_utxos: HashMap<Vec<u8>, u32> = HashMap::new();
        // Free signal: heavy addresses (≥ threshold) collected for the bulk
        // holdings fetch; stake counts summed across each credential's addresses.
        let mut heavy_addresses: Vec<String> = Vec::new();
        let mut stake_counts: HashMap<Vec<u8>, u32> = HashMap::new();
        for (bech32, balance, n_utxos) in balance_rows {
            let Ok(addr) = Address::from_bech32(&bech32) else {
                continue; // Byron / non-bech32 — never in feeds
            };
            let bytes = addr.to_vec();
            let n = u32::try_from(n_utxos).unwrap_or(u32::MAX);
            if let Some(cred) = stake_credential_from_address_bytes(&bytes) {
                *stake_counts.entry(cred).or_insert(0) += n;
            }
            if n_utxos >= MIN_UTXOS_TO_CACHE {
                heavy_addresses.push(bech32);
                address_utxos.insert(bytes.clone(), n);
            }
            address_balances.insert(bytes, balance);
        }

        // Heavy stake set + its cached counts, both derived from the same free
        // per-stake sums — no separate discovery scan (mirrors the address path).
        let mut heavy_creds: Vec<Vec<u8>> = Vec::new();
        let mut stake_utxos: HashMap<Vec<u8>, u32> = HashMap::new();
        for (cred, &n) in &stake_counts {
            if n as i64 >= MIN_UTXOS_TO_CACHE {
                heavy_creds.push(cred.clone());
                stake_utxos.insert(cred.clone(), n);
            }
        }

        let addr_rows = db
            .heavy_address_assets(last_tx_id, &heavy_addresses)
            .await?;
        let stake_rows = db.heavy_stake_assets(last_tx_id, &heavy_creds).await?;
        let mut asset_fp_to_ref: HashMap<String, u32> = HashMap::new();
        let mut asset_meta: Vector<(String, Vec<u8>)> = Vector::new();
        let mut address_assets: HashMap<Vec<u8>, OrdMap<u32, u64>> = HashMap::new();
        let mut stake_assets: HashMap<Vec<u8>, OrdMap<u32, u64>> = HashMap::new();
        build_cache_into(
            &mut asset_fp_to_ref,
            &mut asset_meta,
            &mut address_assets,
            addr_rows,
        );
        build_cache_into(
            &mut asset_fp_to_ref,
            &mut asset_meta,
            &mut stake_assets,
            stake_rows,
        );

        Ok(WarmedCache {
            address_balances,
            asset_fp_to_ref,
            asset_meta,
            address_assets,
            stake_assets,
            address_utxos,
            stake_utxos,
        })
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
        let pools = db.pools(last_tx_id).await?;
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

        tracing::info!("Fetching UTXO stakes...");
        let stakes = db.utxo_stakes(last_tx_id).await?;
        tracing::info!("{} stake addresses with UTXOs", stakes.len());

        let current_epoch = Self::epoch_for_slot(slot, genesis);
        tracing::info!("Fetching rewards (epoch {})...", current_epoch);
        let rewards = db.rewards(current_epoch, last_tx_id).await?;
        tracing::info!("{} stake addresses with rewards", rewards.len());

        tracing::info!("Fetching DRep metadata...");
        let dreps = db.drep_metadata(last_tx_id).await?;
        tracing::info!("{} DReps with metadata", dreps.len());

        tracing::info!("Fetching CIP-68 reference token decimals...");
        let cip68_rows = db.cip68_decimals(last_tx_id).await?;
        let mut decimals = HashMap::new();
        for (policy, name, d) in &cip68_rows {
            if *d > 0 && *d <= 255 {
                let fp = cip68::ft_fingerprint(policy, name);
                decimals.insert(fp, *d as u8);
                let rfp = cip68::rft_fingerprint(policy, name);
                decimals.insert(rfp, *d as u8);
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

        tracing::info!(
            "Warming owned-asset cache (>= {} UTXOs per address / per stake)...",
            MIN_UTXOS_TO_CACHE
        );
        let WarmedCache {
            address_balances,
            asset_fp_to_ref,
            asset_meta,
            address_assets,
            stake_assets,
            address_utxos,
            stake_utxos,
        } = Self::warm_asset_cache(db, last_tx_id, balance_rows).await?;
        tracing::info!(
            "owned-asset cache: {} addresses, {} stakes, {} distinct assets",
            address_assets.len(),
            stake_assets.len(),
            asset_meta.len()
        );

        self.history.clear();
        self.history.push(BlockSnapshot {
            slot,
            block_hash: Some(block_hash),
            last_epoch: Some(current_epoch),
            utxos: HashMap::new(),
            pools,
            pool_delegations,
            pool_delegators,
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
            asset_fp_to_ref,
            asset_meta,
            address_assets,
            stake_assets,
            address_utxos,
            stake_utxos,
            address_assets_populated: true,
        });
        self.feed_index = FeedIndex::new();

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
    pub fn apply_block(
        &mut self,
        slot: u64,
        block_hash: String,
        produced: Vec<((Vec<u8>, i16), TxOutput)>,
        consumed: &[((Vec<u8>, i16), TxOutput)],
        pool_delegation_changes: &[(Vec<u8>, Option<Vec<u8>>)],
        drep_delegation_changes: &[(Vec<u8>, Option<Vec<u8>>)],
        pool_updates: &[PoolUpdate],
        stake_changes: &[(Vec<u8>, i64)],
        withdrawal_changes: &[(Vec<u8>, i64)],
        // Fingerprint → asset name for assets in this block's produced outputs,
        // used to fill the intern table when a cached address gains a new asset
        // (the sink has the name; we never look it up in the hot path).
        produced_asset_names: &std::collections::HashMap<String, Vec<u8>>,
        epoch: u64,
        reward_deltas: Option<&HashMap<Vec<u8>, i64>>,
    ) {
        let prev = self.history.last().expect("state not initialized");

        let mut utxos = prev.utxos.clone();
        let mut address_balances = prev.address_balances.clone();
        // Owned-asset cache maintenance: only touched for cached (scan-bound)
        // addresses; structural sharing keeps the per-block clone cheap.
        let mut asset_fp_to_ref = prev.asset_fp_to_ref.clone();
        let mut asset_meta = prev.asset_meta.clone();
        let mut address_assets = prev.address_assets.clone();
        let mut stake_assets = prev.stake_assets.clone();
        // Per-cached-key unspent-UTXO counts: -1 per consumed output, +1 per
        // produced output, but only for keys already cached (no live promotion).
        // Drives demotion below.
        let mut address_utxos = prev.address_utxos.clone();
        let mut stake_utxos = prev.stake_utxos.clone();

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
            // Cached address / stake spends assets: decrement, drop at zero.
            cache_consume(
                &mut address_assets,
                &output.address,
                &output.assets,
                &asset_fp_to_ref,
            );
            cache_utxo_delta(&mut address_utxos, &output.address, -1);
            if let Some(cred) = stake_credential_from_address_bytes(&output.address) {
                cache_consume(&mut stake_assets, &cred, &output.assets, &asset_fp_to_ref);
                cache_utxo_delta(&mut stake_utxos, &cred, -1);
            }
        }
        for (key, output) in produced {
            let bal: i64 = output
                .lovelaces
                .try_into()
                .expect("lovelace value must fit i64");
            *address_balances.entry(output.address.clone()).or_insert(0) += bal;
            // Cached address / stake receives assets: intern (with the
            // block-provided name) and add the quantity.
            cache_produce(
                &mut address_assets,
                &output.address,
                &output.assets,
                &mut asset_fp_to_ref,
                &mut asset_meta,
                produced_asset_names,
            );
            cache_utxo_delta(&mut address_utxos, &output.address, 1);
            if let Some(cred) = stake_credential_from_address_bytes(&output.address) {
                cache_produce(
                    &mut stake_assets,
                    &cred,
                    &output.assets,
                    &mut asset_fp_to_ref,
                    &mut asset_meta,
                    produced_asset_names,
                );
                cache_utxo_delta(&mut stake_utxos, &cred, 1);
            }
            utxos.insert(key, output);
        }

        // Demote any cached key that fell below the threshold this block (drop it
        // from both the count map and the holdings cache — reads then fall back to
        // the now-fast db query). Bounded by the cached-set size (~hundreds).
        demote_below_threshold(&mut address_utxos, &mut address_assets);
        demote_below_threshold(&mut stake_utxos, &mut stake_assets);

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

        let pools = if pool_updates.is_empty() {
            prev.pools.clone()
        } else {
            let mut pools = prev.pools.clone();
            for (operator, pledge, cost, margin_num, margin_den) in pool_updates {
                let key = hex::encode(operator);
                let ticker = pools.get(&key).and_then(|p| p.ticker.clone());
                let mut pool = Pool::from_registration(
                    operator.clone(),
                    *pledge,
                    *cost,
                    *margin_num,
                    *margin_den,
                );
                pool.ticker = ticker;
                pools.insert(key, pool);
            }
            pools
        };

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

        let dreps = prev.dreps.clone();
        let decimals = prev.decimals.clone();
        let handle_by_address = prev.handle_by_address.clone();
        let address_by_handle = prev.address_by_handle.clone();
        let gov_action_titles = prev.gov_action_titles.clone();
        let address_balances_populated = prev.address_balances_populated;
        let address_assets_populated = prev.address_assets_populated;
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
            handle_by_address,
            address_by_handle,
            gov_action_titles,
            asset_fp_to_ref,
            asset_meta,
            address_assets,
            stake_assets,
            address_utxos,
            stake_utxos,
            address_assets_populated,
        });

        const MAX_HISTORY: usize = 2160;
        if self.history.len() > MAX_HISTORY {
            self.history.drain(..self.history.len() - MAX_HISTORY);
        }
    }

    fn apply_delegation_changes(
        prev_delegations: &HashMap<Vec<u8>, Vec<u8>>,
        prev_delegators: &HashMap<Vec<u8>, HashSet<Vec<u8>>>,
        changes: &[(Vec<u8>, Option<Vec<u8>>)],
    ) -> (
        HashMap<Vec<u8>, Vec<u8>>,
        HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    ) {
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
    pub fn save_snapshot(
        &self,
        path: &Path,
        depth: usize,
        network_magic: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let idx = self.history.len().saturating_sub(depth);
        let snap = &self.history[idx];
        let data = rmp_serde::to_vec(&(snap, &self.feed_index, network_magic))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, path)?;
        Ok(snap.slot)
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
        match rmp_serde::from_slice::<(BlockSnapshot, FeedIndex, u64)>(&data) {
            Ok((snap, fi, magic)) => {
                if magic != network_magic {
                    tracing::warn!(
                        "snapshot network mismatch: snapshot={}, expected={}",
                        magic,
                        network_magic
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
    ) -> (Option<String>, u64, Vec<(String, u64)>) {
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
    fn epoch_for_slot_mainnet() {
        let genesis = GenesisValues::mainnet();
        // Shelley start: epoch 208, slot 4492800
        assert_eq!(State::epoch_for_slot(4492800, &genesis), 208);
        // Known block at slot 181914346 is epoch 618
        assert_eq!(State::epoch_for_slot(181914346, &genesis), 618);
    }

    #[test]
    fn intern_asset_is_idempotent_and_keeps_first_name() {
        let mut fp_to_ref = HashMap::new();
        let mut meta = Vector::new();
        let r1 = intern_asset(&mut fp_to_ref, &mut meta, "fp", b"first");
        let r2 = intern_asset(&mut fp_to_ref, &mut meta, "fp", b"second");
        assert_eq!(r1, r2);
        assert_eq!(meta.len(), 1);
        assert_eq!(meta.get(r1 as usize).unwrap().1, b"first");
    }

    #[test]
    fn build_cache_into_interns_once_and_groups_by_key() {
        let mut fp_to_ref = HashMap::new();
        let mut meta = Vector::new();
        let mut target: HashMap<Vec<u8>, OrdMap<u32, u64>> = HashMap::new();
        let rows = vec![
            (vec![1u8], "a".to_string(), b"na".to_vec(), 3),
            (vec![1u8], "b".to_string(), b"nb".to_vec(), 7),
            (vec![2u8], "a".to_string(), b"na".to_vec(), 2),
        ];
        build_cache_into(&mut fp_to_ref, &mut meta, &mut target, rows);
        assert_eq!(meta.len(), 2); // "a" and "b" each interned once
        assert_eq!(target.get(&vec![1u8]).unwrap().len(), 2);
        assert_eq!(target.get(&vec![2u8]).unwrap().len(), 1);
        let ra = *fp_to_ref.get("a").unwrap();
        assert_eq!(target.get(&vec![2u8]).unwrap().get(&ra).copied(), Some(2));
    }

    #[test]
    fn cache_produce_accumulates_then_consume_drops_at_zero() {
        let mut fp_to_ref = HashMap::new();
        let mut meta = Vector::new();
        let mut cache: HashMap<Vec<u8>, OrdMap<u32, u64>> = HashMap::new();
        let names = std::collections::HashMap::new();
        let addr = vec![1u8, 2, 3];
        cache.insert(addr.clone(), OrdMap::new()); // only cached keys are maintained

        cache_produce(
            &mut cache,
            &addr,
            &[("a".to_string(), 10)],
            &mut fp_to_ref,
            &mut meta,
            &names,
        );
        cache_produce(
            &mut cache,
            &addr,
            &[("a".to_string(), 5)],
            &mut fp_to_ref,
            &mut meta,
            &names,
        );
        let r = *fp_to_ref.get("a").unwrap();
        assert_eq!(cache.get(&addr).unwrap().get(&r).copied(), Some(15));

        cache_consume(&mut cache, &addr, &[("a".to_string(), 15)], &fp_to_ref);
        assert!(cache.get(&addr).unwrap().get(&r).is_none());
        assert_eq!(cache.get(&addr).unwrap().len(), 0);
    }

    #[test]
    fn cache_produce_is_noop_for_uncached_key() {
        let mut fp_to_ref = HashMap::new();
        let mut meta = Vector::new();
        let mut cache: HashMap<Vec<u8>, OrdMap<u32, u64>> = HashMap::new();
        let names = std::collections::HashMap::new();
        let addr = vec![9u8];
        // No prior entry ⇒ not cached ⇒ no live promotion.
        cache_produce(
            &mut cache,
            &addr,
            &[("a".to_string(), 1)],
            &mut fp_to_ref,
            &mut meta,
            &names,
        );
        assert!(cache.get(&addr).is_none());
    }

    #[test]
    fn utxo_delta_maintains_count_and_saturates() {
        let mut counts: HashMap<Vec<u8>, u32> = HashMap::new();
        let addr = vec![7u8];
        counts.insert(addr.clone(), 3);
        cache_utxo_delta(&mut counts, &addr, 2);
        assert_eq!(counts.get(&addr).copied(), Some(5));
        cache_utxo_delta(&mut counts, &addr, -10); // saturates at 0, not underflow
        assert_eq!(counts.get(&addr).copied(), Some(0));
        // Uncached key is left untouched.
        cache_utxo_delta(&mut counts, &[42u8], -1);
        assert!(counts.get(&vec![42u8]).is_none());
    }

    #[test]
    fn demotion_drops_only_keys_below_threshold() {
        let mut counts: HashMap<Vec<u8>, u32> = HashMap::new();
        let mut cache: HashMap<Vec<u8>, OrdMap<u32, u64>> = HashMap::new();
        let heavy = vec![1u8];
        let light = vec![2u8];
        counts.insert(heavy.clone(), MIN_UTXOS_TO_CACHE as u32);
        counts.insert(light.clone(), MIN_UTXOS_TO_CACHE as u32 - 1);
        cache.insert(heavy.clone(), OrdMap::new());
        cache.insert(light.clone(), OrdMap::new());

        demote_below_threshold(&mut counts, &mut cache);

        assert!(counts.get(&heavy).is_some());
        assert!(cache.get(&heavy).is_some());
        assert!(counts.get(&light).is_none());
        assert!(cache.get(&light).is_none());
    }
}
