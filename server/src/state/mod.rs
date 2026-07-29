mod dbsync;
pub mod feed_index;

use imbl::{hashmap::HashMap, hashset::HashSet, ordmap::OrdMap};
use std::path::Path;
use std::sync::Arc;
use url::Url;

use crate::cip26;
use crate::model::{
    asset_fingerprint, parse_virtual_handle_address, DRep, DRepVotes, Delegation, Pool, TxOutput,
    HANDLE_POLICIES,
};
use crate::pallas::{
    stake_credential_from_address_bytes, stake_credential_from_bech32, PoolUpdate,
};
pub use dbsync::{DbSync, DelegationFill, FillBlock};
pub use feed_index::FeedIndex;

#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlockSnapshot {
    pub slot: u64,
    pub block_hash: Option<String>,
    pub last_epoch: Option<u64>,
    pub utxos: HashMap<(Vec<u8>, i16), TxOutput>,
    pub pools: HashMap<String, Pool>,
    /// Stake credential → the pool it backs + the slot its current run there began
    /// (see [`Delegation`]).
    pub pool_delegations: HashMap<Vec<u8>, Delegation>,
    pub pool_delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    /// Stake credential → the DRep it backs (tagged bytes) + its run's start slot.
    pub drep_delegations: HashMap<Vec<u8>, Delegation>,
    pub drep_delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    pub stakes: HashMap<Vec<u8>, i64>,
    pub rewards: HashMap<Vec<u8>, i64>,
    /// DRep bytes → DRep metadata (given_name from off-chain vote data)
    pub dreps: HashMap<Vec<u8>, DRep>,
    /// DRep bytes → governance votes cast (lifetime + current-epoch tally). Keyed
    /// independently of `dreps`, which only holds DReps that published off-chain metadata —
    /// a nameless DRep votes too. Seeded from db-sync at reset / resume, then maintained
    /// per block by `apply_block`.
    #[serde(default)]
    pub drep_vote_counts: HashMap<Vec<u8>, DRepVotes>,
    /// Asset fingerprint → decimals (non-zero only, from CIP-26 + CIP-68)
    pub decimals: HashMap<String, u8>,
    /// ADA Handle: address → list of handle names owned
    #[serde(default)]
    pub handle_by_address: HashMap<String, Vec<String>>,
    /// ADA Handle: handle name → owner address
    #[serde(default)]
    pub address_by_handle: HashMap<String, String>,
    /// ADA Handle: 28-byte stake credential → handle names owned across all of its payment
    /// addresses (a re-keying of `handle_by_address` by stake credential; the shortest wins at
    /// query time). Derived — `#[serde(skip)]` and rebuilt from `handle_by_address` on load/reset,
    /// so it adds nothing to the snapshot file and needs no format bump; maintained live per block
    /// alongside the other two maps and reverted with them on rollback (all on the snapshot).
    #[serde(skip)]
    pub handle_by_stake: HashMap<Vec<u8>, Vec<String>>,
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
    ///
    /// Skipped by the derive: (de)serialized manually in [`write_snapshot`] / [`load_snapshot`]
    /// so load can **intern each key as it streams** (never materializing 14.8M un-shared
    /// `Arc<AddrKey>`), and so keys go on the wire deref'd (no `Arc`, no serde `rc` feature).
    #[serde(skip)]
    pub asset_holdings: AssetHoldings,
    /// True iff `asset_holdings` was fully populated from db-sync (by `reset()` or
    /// `populate_asset_holdings`). False on snapshots saved before the field existed,
    /// so warm resume runs the one-time populate. Mirrors `address_balances_populated`.
    #[serde(default)]
    pub asset_holdings_populated: bool,
}

/// A **query** descriptor for one payment address: `(stake credential | None for an
/// enterprise/pointer address, payment-address bytes)`. Callers pass this to describe an
/// address; internally it's converted to an [`AddrKey`] for the interned holdings key.
/// Ordered by credential first so a stake's payment addresses are a contiguous range.
pub type AssetKey = (Option<Vec<u8>>, Vec<u8>);

/// The interned `(cred, addr)` half of a holdings key. All of one address's tokens share a
/// single `Arc<AddrKey>`, so the credential + address bytes (and their allocations) are
/// stored once instead of once per held token. Derived `Ord` reproduces the old
/// `(Option<Vec<u8>>, Vec<u8>)` ordering exactly — `None < Some`, then cred bytes, then addr
/// bytes (`Box<[u8]>` compares as the slice, same as `Vec<u8>`) — and `Arc<T>`'s by-value
/// `Ord` delegates to it, so the `(cred, addr, policy, name)` sort the prefix range scans
/// depend on is unchanged. `Hash`/`Eq` back the interner map.
#[derive(
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct AddrKey {
    pub cred: Option<Box<[u8]>>,
    pub addr: Box<[u8]>,
}

impl AddrKey {
    fn new(cred: Option<&[u8]>, addr: &[u8]) -> Self {
        AddrKey {
            cred: cred.map(Box::from),
            addr: Box::from(addr),
        }
    }
    fn from_query(q: &AssetKey) -> Self {
        AddrKey::new(q.0.as_deref(), &q.1)
    }
}

/// Interner mapping each distinct `(cred, addr)` to a shared `Arc<AddrKey>`. Lives in
/// [`State`] (not in [`BlockSnapshot`]) so the persisted snapshot stays free of pointers and
/// rollback needs no special handling — interned values are content-addressed and immutable,
/// so a rollback just drops entries (dropping `Arc` refs) and any stale interner entry is
/// harmlessly reused. Grows with distinct token-holding addresses ever seen; never evicted.
pub type AddrInterner = std::collections::HashMap<AddrKey, Arc<AddrKey>>;

/// Return the shared `Arc<AddrKey>` for an owned `AddrKey`, creating and caching it on first
/// use. The dedup point every mutation and the streaming snapshot load route through.
fn intern_owned(interner: &mut AddrInterner, k: AddrKey) -> Arc<AddrKey> {
    if let Some(a) = interner.get(&k) {
        return a.clone();
    }
    let arc = Arc::new(k.clone());
    interner.insert(k, arc.clone());
    arc
}

/// Return the shared `Arc<AddrKey>` for `(cred, addr)`. Every holdings *mutation* routes
/// through this so all of an address's tokens share one Arc.
fn intern_addr(interner: &mut AddrInterner, cred: Option<&[u8]>, addr: &[u8]) -> Arc<AddrKey> {
    intern_owned(interner, AddrKey::new(cred, addr))
}

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

/// A held token's leaf: its quantity plus the asset's first-mint time (unix seconds as `u32`
/// packing). The asset's first-mint **slot** rides in the top 30 bits (only for sorting the
/// owned grid — a slot is monotonic with time and we don't display it; the asset-info popup's
/// mint dates come from a separate db query). Stored as a `(lo, hi)` `u64` pair, **not** a
/// bare `u128`: two `u64`s keep the struct at align 8 / 16 bytes, whereas a `u128` (align 16)
/// would just re-pad the align-8 key back up — no saving. `mint_slot` is chain-derived, so the
/// snapshot stays independent of any db-sync's row ids.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Held {
    lo: u64,
    hi: u64,
}

/// Bits of the packed 128-bit leaf given to the quantity (low); the remaining 30 bits (high)
/// hold the first-mint slot. 98 bits ≫ any real token quantity (bounded well under 2^64), and
/// 30 bits of slot ≈ year 2054 on mainnet — plenty for a value only used to sort.
const QTY_BITS: u32 = 98;
const QTY_MASK: u128 = (1u128 << QTY_BITS) - 1;
/// 30-bit slot mask (`128 - QTY_BITS` high bits).
const MINT_SLOT_MASK: u32 = (1u32 << (128 - QTY_BITS)) - 1;

impl Held {
    pub fn new(qty: u128, mint_slot: u32) -> Self {
        Self::from_packed((qty & QTY_MASK) | (((mint_slot & MINT_SLOT_MASK) as u128) << QTY_BITS))
    }
    fn packed(&self) -> u128 {
        ((self.hi as u128) << 64) | self.lo as u128
    }
    fn from_packed(p: u128) -> Self {
        Held {
            lo: p as u64,
            hi: (p >> 64) as u64,
        }
    }
    pub fn qty(&self) -> u128 {
        self.packed() & QTY_MASK
    }
    /// First-mint slot (0 = unknown/not yet sourced). Monotonic with time; used only to sort.
    pub fn mint_slot(&self) -> u32 {
        (self.packed() >> QTY_BITS) as u32
    }
    fn set_qty(&mut self, qty: u128) {
        *self = Self::from_packed((self.packed() & !QTY_MASK) | (qty & QTY_MASK));
    }
    fn set_mint_slot(&mut self, mint_slot: u32) {
        *self = Self::from_packed(
            (self.packed() & QTY_MASK) | (((mint_slot & MINT_SLOT_MASK) as u128) << QTY_BITS),
        );
    }
}

// The packing only pays off if the leaf stays align-8 / 16 bytes: a bare `u128` (align 16)
// would re-pad the align-8 key back up to the old size. Two `u64`s keep both invariants.
const _: () = assert!(std::mem::size_of::<Held>() == 16 && std::mem::align_of::<Held>() == 8);

impl serde::Serialize for Held {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // (mint_slot, qty) — the quantity keeps the variable-length `Qty` encoding.
        (self.mint_slot(), Qty(self.qty())).serialize(s)
    }
}
impl<'de> serde::Deserialize<'de> for Held {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (mint_slot, qty): (u32, Qty) = serde::Deserialize::deserialize(d)?;
        Ok(Held::new(qty.0, mint_slot))
    }
}

/// A held token's asset id: `policy (28 bytes) ++ name`. Since `policy` is fixed-width the
/// packed bytes sort exactly as `(policy, name)` — and it's one allocation, not two.
pub type AssetId = Box<[u8]>;

/// Composite key of the flat [`AssetHoldings`] map: `(interned (cred, addr), policy ++
/// name)`, sorting `(cred, addr, policy, name)`. The `(cred, addr)` is an `Arc<AddrKey>`
/// shared across every token that address holds (see [`AddrInterner`]).
pub type HeldKey = (Arc<AddrKey>, AssetId);

/// Build a throwaway (non-interned) `HeldKey` lower bound for a range scan / point lookup:
/// a fresh `Arc<AddrKey>` for the address plus an empty `AssetId`. Cheap one-off alloc; the
/// map compares by value, so it need not be the interned Arc.
fn bound_key(addr: AddrKey) -> HeldKey {
    (Arc::new(addr), Box::default())
}

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

    /// Every `(policy, name, quantity, mint_time)` token currently held by a payment address —
    /// the rows the owned-assets grid renders, straight from memory (no db scan). Unsorted;
    /// the caller sorts (by quantity, then mint_time) and paginates.
    pub fn address_held_assets(&self, address: &[u8]) -> Vec<(Vec<u8>, Vec<u8>, u128, u32)> {
        let cred = stake_credential_from_address_bytes(address);
        addr_range(&self.asset_holdings, &(cred, address.to_vec()))
            .map(|((_, asset), h)| {
                let (policy, name) = split_asset(asset);
                (policy.to_vec(), name.to_vec(), h.qty(), h.mint_slot())
            })
            .collect()
    }

    /// Distinct `(policy, name, quantity, mint_time)` tokens held across every payment address
    /// sharing a stake credential — the same asset on two of the credential's addresses is one
    /// owned asset, with the quantities summed (mint_time is per-asset, so identical across the
    /// leaves). Unsorted; the caller paginates.
    pub fn stake_held_assets(&self, cred: &[u8]) -> Vec<(Vec<u8>, Vec<u8>, u128, u32)> {
        let mut sums: std::collections::HashMap<&[u8], (u128, u32)> =
            std::collections::HashMap::new();
        for ((_, asset), h) in cred_range(&self.asset_holdings, cred) {
            let e = sums.entry(asset).or_insert((0, h.mint_slot()));
            e.0 += h.qty();
        }
        sums.into_iter()
            .map(|(asset, (q, mint_time))| {
                let (policy, name) = split_asset(asset);
                (policy.to_vec(), name.to_vec(), q, mint_time)
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
        .get(&(Arc::new(AddrKey::from_query(key)), asset_id(policy, name)))
        .map(|h| h.qty())
        .unwrap_or(0)
}

/// Quantity of one `(policy, name)` token summed across every payment address sharing a
/// stake credential — the stake-level owned amount.
pub fn stake_token_qty(holdings: &AssetHoldings, cred: &[u8], policy: &[u8], name: &[u8]) -> u128 {
    let target = asset_id(policy, name);
    cred_range(holdings, cred)
        .filter_map(|((_, asset), h)| (asset == &target).then_some(h.qty()))
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

/// Open file-descriptor count (entries in `/proc/self/fd`). A cheap heartbeat alongside RSS:
/// an fd leak — e.g. reconnect churn or N2N replay `PeerClient`s not being dropped — shows up
/// as a steady climb toward `LimitNOFILE` well before exhaustion starts failing connections.
pub fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, |d| d.count())
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

    /// Detailed per-field memory accounting: for every map, the exact **content** bytes it
    /// owns — `inline` (the `(K, V)` pairs stored in the container's own nodes, i.e.
    /// `entries * size_of::<(K, V)>()`) plus `heap` (bytes each key/value points to:
    /// `Vec`/`String` capacities, `Box<[u8]>` lengths). This is walked, not guessed. It
    /// **excludes** container node overhead (imbl B-tree/HAMT pointers and partially-filled
    /// chunks, `std::HashMap` load-factor slack); the gap from the `total` here to RSS is
    /// that overhead + allocator slack + the versioned `history` diffs of older snapshots.
    /// O(total entries) — a one-time diagnostic after the snapshot is loaded or rebuilt,
    /// never on the hot path (unlike [`log_sizes`], which skips the big leaf walks).
    pub fn log_memory(&self, label: &str) {
        // Diagnostics only, and not cheap: summing content bytes walks all ~15M holdings
        // entries plus every other map — ~1s of a ~22s start. Skip the *computation* (not
        // just the output) unless DEBUG is on, i.e. the server was started with `-v`.
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        use std::mem::size_of;
        let mb = |b: usize| b / (1024 * 1024);

        // --- flat maps of raw-bytes keys to a scalar / small value ---
        // heap of the composite key + value, summed over entries; inline added per field.
        let sum_vec_i64 = |m: &HashMap<Vec<u8>, i64>| -> usize {
            m.len() * size_of::<(Vec<u8>, i64)>()
                + m.iter().map(|(k, _)| k.capacity()).sum::<usize>()
        };
        let stakes_b = sum_vec_i64(&self.stakes);
        let rewards_b = sum_vec_i64(&self.rewards);
        let addr_bal_b = sum_vec_i64(&self.address_balances);

        // delegations: Vec<u8> key -> Delegation { Box<[u8]> target, u64 since_slot }. The boxed
        // target keeps the pair at the same 48 bytes a bare `Vec<u8>` value cost (see `Delegation`).
        let sum_deleg = |m: &HashMap<Vec<u8>, Delegation>| -> usize {
            m.len() * size_of::<(Vec<u8>, Delegation)>()
                + m.iter()
                    .map(|(k, v)| k.capacity() + v.target.len())
                    .sum::<usize>()
        };
        let pool_deleg_b = sum_deleg(&self.pool_delegations);
        let drep_deleg_b = sum_deleg(&self.drep_delegations);

        // delegators: Vec<u8> key -> HashSet<Vec<u8>> members
        let sum_deleg_sets = |m: &HashMap<Vec<u8>, HashSet<Vec<u8>>>| -> (usize, usize) {
            let mut members = 0usize;
            let mut bytes = m.len() * size_of::<(Vec<u8>, HashSet<Vec<u8>>)>();
            for (k, set) in m.iter() {
                bytes += k.capacity() + set.len() * size_of::<Vec<u8>>();
                for member in set.iter() {
                    bytes += member.capacity();
                }
                members += set.len();
            }
            (bytes, members)
        };
        let (pool_delegators_b, pool_delegator_members) = sum_deleg_sets(&self.pool_delegators);
        let (drep_delegators_b, drep_delegator_members) = sum_deleg_sets(&self.drep_delegators);

        // utxos: (txid, ix) -> TxOutput { address, nested PolicyAssets }
        let mut utxos_b = self.utxos.len() * size_of::<((Vec<u8>, i16), TxOutput)>();
        for ((txid, _ix), out) in self.utxos.iter() {
            utxos_b += txid.capacity() + out.address.capacity();
            utxos_b += out.assets.capacity() * size_of::<(Vec<u8>, Vec<(Vec<u8>, u64)>)>();
            for (policy, names) in out.assets.iter() {
                utxos_b += policy.capacity() + names.capacity() * size_of::<(Vec<u8>, u64)>();
                for (name, _q) in names.iter() {
                    utxos_b += name.capacity();
                }
            }
        }

        // pools: String ticker/hash key -> Pool { hash_raw, ticker }
        let mut pools_b = self.pools.len() * size_of::<(String, Pool)>();
        for (k, p) in self.pools.iter() {
            pools_b += k.capacity() + p.hash_raw.capacity();
            pools_b += p.ticker.as_ref().map_or(0, |t| t.capacity());
        }

        // dreps: Vec<u8> -> DRep { hash_bytes, given_name }
        let mut dreps_b = self.dreps.len() * size_of::<(Vec<u8>, DRep)>();
        for (k, d) in self.dreps.iter() {
            dreps_b += k.capacity() + d.hash_bytes.capacity();
            dreps_b += d.given_name.as_ref().map_or(0, |n| n.capacity());
        }

        // string-keyed metadata maps
        let mut decimals_b = self.decimals.len() * size_of::<(String, u8)>();
        for (k, _) in self.decimals.iter() {
            decimals_b += k.capacity();
        }
        let mut handles_b = self.handle_by_address.len() * size_of::<(String, Vec<String>)>();
        for (k, v) in self.handle_by_address.iter() {
            handles_b += k.capacity() + v.capacity() * size_of::<String>();
            for h in v.iter() {
                handles_b += h.capacity();
            }
        }
        let mut addr_by_handle_b = self.address_by_handle.len() * size_of::<(String, String)>();
        for (k, v) in self.address_by_handle.iter() {
            addr_by_handle_b += k.capacity() + v.capacity();
        }
        let mut gov_titles_b = self.gov_action_titles.len() * size_of::<(String, String)>();
        for (k, v) in self.gov_action_titles.iter() {
            gov_titles_b += k.capacity() + v.capacity();
        }

        // asset_holdings — the dominant field; break it out (see log_memory_holdings).
        let (ah_entries, ah_inline, ah_assetid_heap, ah_held_inline) = self.holdings_memory();
        let ah_b = ah_inline + ah_assetid_heap;

        let total = stakes_b
            + rewards_b
            + addr_bal_b
            + pool_deleg_b
            + drep_deleg_b
            + pool_delegators_b
            + drep_delegators_b
            + utxos_b
            + pools_b
            + dreps_b
            + decimals_b
            + handles_b
            + addr_by_handle_b
            + gov_titles_b
            + ah_b;

        tracing::debug!(
            label,
            rss_mb = rss_mb(),
            total_content_mb = mb(total),
            utxos_mb = mb(utxos_b),
            asset_holdings_mb = mb(ah_b),
            stakes_mb = mb(stakes_b),
            rewards_mb = mb(rewards_b),
            address_balances_mb = mb(addr_bal_b),
            pool_delegations_mb = mb(pool_deleg_b),
            pool_delegators_mb = mb(pool_delegators_b),
            drep_delegations_mb = mb(drep_deleg_b),
            drep_delegators_mb = mb(drep_delegators_b),
            pools_mb = mb(pools_b),
            dreps_mb = mb(dreps_b),
            decimals_mb = mb(decimals_b),
            handles_mb = mb(handles_b),
            address_by_handle_mb = mb(addr_by_handle_b),
            gov_titles_mb = mb(gov_titles_b),
            pool_delegator_members,
            drep_delegator_members,
            "memory: snapshot fields (content bytes: inline + heap, excl. node overhead)",
        );

        // asset_holdings breakdown. The `(cred, addr)` bytes are now interned (one shared
        // `Arc<AddrKey>` per address), so per entry the key costs only the `Arc` pointer inline
        // + the `AssetId` (policy++name); the shared address bytes are accounted separately in
        // `memory: addr_interner`. `held_inline` is the value's share (`entries *
        // size_of::<Held>()`).
        tracing::debug!(
            label,
            entries = ah_entries,
            total_mb = mb(ah_b),
            inline_mb = mb(ah_inline),
            assetid_heap_mb = mb(ah_assetid_heap),
            held_inline_mb = mb(ah_held_inline),
            held_bytes = size_of::<Held>(),
            key_inline_bytes = size_of::<HeldKey>(),
            bytes_per_entry = if ah_entries > 0 { ah_b / ah_entries } else { 0 },
            "memory: asset_holdings (leaf = Held; key = (Arc<AddrKey>, policy++name))",
        );
    }

    /// Walk `asset_holdings` once, returning `(entries, inline_bytes, assetid_heap_bytes,
    /// held_inline_bytes)`. `inline` = `entries * size_of::<(HeldKey, Held)>()` (the pair
    /// stored in imbl nodes — now `Arc` pointer + `AssetId` + `Held`); `assetid_heap` = the
    /// per-entry `policy++name` bytes (the `(cred, addr)` bytes are interned/shared, counted in
    /// `memory: addr_interner`, not here); `held_inline` = the value's share (`entries *
    /// size_of::<Held>()`).
    fn holdings_memory(&self) -> (usize, usize, usize, usize) {
        use std::mem::size_of;
        let entries = self.asset_holdings.len();
        let mut assetid_heap = 0usize;
        for ((_arc, aid), _held) in self.asset_holdings.iter() {
            assetid_heap += aid.len();
        }
        let inline = entries * size_of::<(HeldKey, Held)>();
        let held_inline = entries * size_of::<Held>();
        (entries, inline, assetid_heap, held_inline)
    }
}

/// On-disk snapshot format version. Bump on any breaking change to a persisted field's
/// shape/semantics that rmp can't catch (it tolerates int-width changes) — a mismatch is
/// rejected on load so the state rebuilds from db-sync. v2: `asset_holdings` leaf went
/// from UTXO count to summed held quantity. v3: that leaf became a `u128` `Qty`. v4:
/// `asset_holdings` flattened to one `OrdMap<HeldKey, Qty>`. v5: force a rebuild to heal
/// `address_balances`/`asset_holdings` drift accumulated by the intra-block debit-drop bug
/// (produced now applied before consumed in `apply_block`). v6: the holdings leaf gained the
/// asset's `mint_time` (`Qty` → `Held`). v7: holdings key `(cred, addr)` became an interned
/// `Arc<AddrKey>` (wire shape unchanged per entry, but re-interned on load). v8: the `Held`
/// leaf became a packed 128-bit `(mint_slot:30 | qty:98)` and `mint_time` (unix seconds)
/// became `mint_slot` — different semantics, so old leaves must rebuild. v9: `FeedIndex`
/// gained `pool_votes`/`drep_votes` (governance vote-block index); rmp-serde encodes structs
/// positionally, so the extra fields shift the layout and old snapshots must rebuild.
const SNAPSHOT_FORMAT: u32 = 15;

/// The previous format: holdings grouped by address but one full `policy ++ name` per token,
/// byte strings encoded as msgpack arrays. Still *readable* ([`LegacyHoldingsSeed`]) so a
/// deploy resumes from the snapshot on disk instead of a multi-minute cold reset; never
/// written. Remove once every deployment has rolled over.
const SNAPSHOT_FORMAT_LEGACY_PER_TOKEN: u32 = 14;

/// Serializes [`BlockSnapshot::asset_holdings`] **grouped by address**: a msgpack map of
/// `AddrKey → [(AssetId, Held)]`, with the key `deref'd off its `Arc`` (the wire form carries
/// no `Arc`, so no serde `rc` feature).
///
/// Grouping is a load-time optimisation, and the reason the map is an `OrdMap` sorted by
/// `(cred, addr, policy, name)` pays off twice: an address's tokens are contiguous, so its
/// `(cred, addr)` goes on the wire **once per address (1.3M) instead of once per token
/// (14.8M)**. Measured on mainnet, the old per-token shape spent 11.4s of a 19.6s holdings
/// load just decoding — dominated by allocating a throwaway cred+addr per entry and looking
/// each one up in the interner. Grouping removes ~13.5M of those allocations and lookups,
/// and makes the file smaller as well. Pairs with [`HoldingsSeed`].
struct HoldingsSer<'a>(&'a AssetHoldings);

impl serde::Serialize for HoldingsSer<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        // msgpack needs every length up front, so the address count comes from one cheap
        // counting pass rather than a global index of all ~15M entry refs (that index was
        // ~350MB of transient pointers on every write; a snapshot happens every 50 blocks).
        let addresses = {
            let mut n = 0usize;
            let mut last: Option<&AddrKey> = None;
            for ((arc, _), _) in self.0.iter() {
                if last != Some(arc.as_ref()) {
                    n += 1;
                    last = Some(arc.as_ref());
                }
            }
            n
        };

        let mut m = s.serialize_map(Some(addresses))?;
        // Sorted order means one contiguous run per address, and within it one per policy, so
        // a single walk emits both levels with only the current address's tokens buffered
        // (~11 on average).
        let mut addr: Option<&AddrKey> = None;
        let mut tokens: Vec<(&AssetId, &Held)> = Vec::new();
        for ((arc, aid), held) in self.0.iter() {
            // Compare by value, never `Arc::ptr_eq` (prev/curr snapshots may hold distinct
            // Arcs for the same address).
            if addr != Some(arc.as_ref()) {
                if let Some(prev) = addr {
                    m.serialize_entry(prev, &PolicyGroups(&tokens))?;
                }
                addr = Some(arc.as_ref());
                tokens.clear();
            }
            tokens.push((aid, held));
        }
        if let Some(prev) = addr {
            m.serialize_entry(prev, &PolicyGroups(&tokens))?;
        }
        m.end()
    }
}

/// One address's tokens as `[(policy, [(name, Held)])]`: the 28-byte policy goes on the wire
/// once per (address, policy) run instead of once per token — 6.3M times instead of 14.8M on
/// mainnet, ~240MB less file. `AssetId` is `policy ++ name`, so the split is a slice, and the
/// reader concatenates them back into one allocation per entry ([`AssetIdSeed`]).
struct PolicyGroups<'a>(&'a [(&'a AssetId, &'a Held)]);

/// Length of a Cardano policy id (blake2b-224), the fixed prefix of every `AssetId`.
const POLICY_LEN: usize = 28;

fn split_asset_id(aid: &AssetId) -> (&[u8], &[u8]) {
    let at = POLICY_LEN.min(aid.len());
    (&aid[..at], &aid[at..])
}

impl serde::Serialize for PolicyGroups<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut runs = 0usize;
        let mut last: Option<&[u8]> = None;
        for (aid, _) in self.0 {
            let (policy, _) = split_asset_id(aid);
            if last != Some(policy) {
                runs += 1;
                last = Some(policy);
            }
        }
        let mut seq = s.serialize_seq(Some(runs))?;
        let mut i = 0;
        while i < self.0.len() {
            let (policy, _) = split_asset_id(self.0[i].0);
            let start = i;
            while i < self.0.len() && split_asset_id(self.0[i].0).0 == policy {
                i += 1;
            }
            seq.serialize_element(&(serde_bytes::Bytes::new(policy), Names(&self.0[start..i])))?;
        }
        seq.end()
    }
}

/// The `[(name, Held)]` half of a policy run.
struct Names<'a>(&'a [(&'a AssetId, &'a Held)]);

impl serde::Serialize for Names<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for (aid, held) in self.0 {
            let (_, name) = split_asset_id(aid);
            seq.serialize_element(&(serde_bytes::Bytes::new(name), held))?;
        }
        seq.end()
    }
}

/// Deserializes the holdings map produced by [`HoldingsSer`]: `address → [(policy,
/// [(name, Held)])]`. Each address is interned **once** and its tokens are inserted as they
/// stream, so the un-shared full-size map is never materialized (the warm-resume RSS spike a
/// naive `Arc`-per-entry decode would cause) and nothing is buffered per address or per policy.
/// Each entry costs exactly one allocation: [`AssetIdSeed`] writes `policy ++ name` straight
/// into it. Seeds `interner`.
struct HoldingsSeed<'a>(&'a mut AddrInterner);

/// Rebuilds one `AssetId` (`policy ++ name`) in a single allocation: the policy comes from the
/// enclosing run, the name straight off the wire.
struct AssetIdSeed<'a>(&'a [u8]);

impl<'de> serde::de::DeserializeSeed<'de> for AssetIdSeed<'_> {
    type Value = AssetId;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<AssetId, D::Error> {
        struct V<'a>(&'a [u8]);
        impl<'de> serde::de::Visitor<'de> for V<'_> {
            type Value = AssetId;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an asset name (bytes)")
            }
            fn visit_bytes<E: serde::de::Error>(self, name: &[u8]) -> Result<AssetId, E> {
                let mut aid = Vec::with_capacity(self.0.len() + name.len());
                aid.extend_from_slice(self.0);
                aid.extend_from_slice(name);
                Ok(aid.into_boxed_slice())
            }
            fn visit_byte_buf<E: serde::de::Error>(self, name: Vec<u8>) -> Result<AssetId, E> {
                self.visit_bytes(&name)
            }
        }
        d.deserialize_bytes(V(self.0))
    }
}

/// One `(name, Held)` pair, with the run's policy folded into the key.
struct TokenSeed<'a> {
    out: &'a mut AssetHoldings,
    addr: &'a Arc<AddrKey>,
    policy: &'a [u8],
}

impl<'de> serde::de::DeserializeSeed<'de> for TokenSeed<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        struct V<'a>(TokenSeed<'a>);
        impl<'de> serde::de::Visitor<'de> for V<'_> {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a (name, Held) pair")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
                use serde::de::Error;
                let aid = seq
                    .next_element_seed(AssetIdSeed(self.0.policy))?
                    .ok_or_else(|| A::Error::custom("missing asset name"))?;
                let held: Held = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("missing Held"))?;
                self.0.out.insert((self.0.addr.clone(), aid), held);
                Ok(())
            }
        }
        d.deserialize_seq(V(self))
    }
}

/// One `(policy, [(name, Held)])` run.
struct PolicyRunSeed<'a> {
    out: &'a mut AssetHoldings,
    addr: &'a Arc<AddrKey>,
}

impl<'de> serde::de::DeserializeSeed<'de> for PolicyRunSeed<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        struct V<'a>(PolicyRunSeed<'a>);
        impl<'de> serde::de::Visitor<'de> for V<'_> {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a (policy, tokens) run")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
                use serde::de::Error;
                let policy: serde_bytes::ByteBuf = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::custom("missing policy"))?;
                seq.next_element_seed(TokensSeed {
                    out: self.0.out,
                    addr: self.0.addr,
                    policy: &policy,
                })?
                .ok_or_else(|| A::Error::custom("missing token list"))
            }
        }
        d.deserialize_seq(V(self))
    }
}

/// The token list inside one policy run.
struct TokensSeed<'a> {
    out: &'a mut AssetHoldings,
    addr: &'a Arc<AddrKey>,
    policy: &'a [u8],
}

impl<'de> serde::de::DeserializeSeed<'de> for TokensSeed<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        struct V<'a>(TokensSeed<'a>);
        impl<'de> serde::de::Visitor<'de> for V<'_> {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of (name, Held)")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
                while seq
                    .next_element_seed(TokenSeed {
                        out: self.0.out,
                        addr: self.0.addr,
                        policy: self.0.policy,
                    })?
                    .is_some()
                {}
                Ok(())
            }
        }
        d.deserialize_seq(V(self))
    }
}

/// All of one address's policy runs.
struct AddrRunsSeed<'a> {
    out: &'a mut AssetHoldings,
    addr: Arc<AddrKey>,
}

impl<'de> serde::de::DeserializeSeed<'de> for AddrRunsSeed<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        struct V<'a>(AddrRunsSeed<'a>);
        impl<'de> serde::de::Visitor<'de> for V<'_> {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an address's policy runs")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
                while seq
                    .next_element_seed(PolicyRunSeed {
                        out: self.0.out,
                        addr: &self.0.addr,
                    })?
                    .is_some()
                {}
                Ok(())
            }
        }
        d.deserialize_seq(V(self))
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for HoldingsSeed<'_> {
    type Value = AssetHoldings;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<AssetHoldings, D::Error> {
        struct V<'a>(&'a mut AddrInterner);
        impl<'de> serde::de::Visitor<'de> for V<'_> {
            type Value = AssetHoldings;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of (cred, addr) → [(policy, [(name, Held)])]")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<AssetHoldings, A::Error> {
                let mut out: AssetHoldings = OrdMap::new();
                while let Some(addr_key) = map.next_key::<AddrKey>()? {
                    let addr = intern_owned(self.0, addr_key);
                    map.next_value_seed(AddrRunsSeed {
                        out: &mut out,
                        addr,
                    })?;
                }
                Ok(out)
            }
        }
        d.deserialize_map(V(self.0))
    }
}

/// Reads `SNAPSHOT_FORMAT` 14: grouped by address already, but each token carrying its whole
/// `policy ++ name`. Only needed until deployments have written a format-15 snapshot.
struct LegacyHoldingsSeed<'a>(&'a mut AddrInterner);

impl<'de> serde::de::DeserializeSeed<'de> for LegacyHoldingsSeed<'_> {
    type Value = AssetHoldings;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<AssetHoldings, D::Error> {
        struct V<'a>(&'a mut AddrInterner);
        impl<'de> serde::de::Visitor<'de> for V<'_> {
            type Value = AssetHoldings;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of (cred, addr) → [(policy++name, Held)]")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<AssetHoldings, A::Error> {
                let mut out: AssetHoldings = OrdMap::new();
                while let Some(addr_key) = map.next_key::<AddrKey>()? {
                    let addr = intern_owned(self.0, addr_key);
                    for (aid, held) in map.next_value::<Vec<(AssetId, Held)>>()? {
                        out.insert((addr.clone(), aid), held);
                    }
                }
                Ok(out)
            }
        }
        d.deserialize_map(V(self.0))
    }
}

/// Serialize the snapshot atomically (temp file + rename, so a crash mid-write leaves the
/// previous snapshot intact). Free fn so it can run on a `spawn_blocking` thread from owned
/// clones, without the `chain_state` lock or `&self`. Returns the persisted slot.
///
/// The file is a sequence of msgpack values: **`format`, `magic`** (first, so
/// [`load_snapshot`] can reject a stale/foreign snapshot before reading the multi-GB map),
/// then the `snapshot` (its `asset_holdings` skipped by the derive), then `asset_holdings`
/// (via [`HoldingsSer`]), then the `feed_index`.
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
    rmp_serde::encode::write(&mut wr, &SNAPSHOT_FORMAT)?;
    rmp_serde::encode::write(&mut wr, &network_magic)?;
    rmp_serde::encode::write(&mut wr, snap)?;
    rmp_serde::encode::write(&mut wr, &HoldingsSer(&snap.asset_holdings))?;
    rmp_serde::encode::write(&mut wr, feed_index)?;
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

    /// Apply one block's ADA Handle movements to the resolution maps, in place.
    ///
    /// `changes` are `(handle, new_owner_address)` for every handle NFT that appeared in the
    /// block's *produced* outputs — classic/CIP-68 (222) resolve to the holding address, virtual
    /// (000) to the address in the NFT's inline datum (resolved by the caller). Each handle is
    /// moved to its new owner; a mint is just a change with no prior entry.
    ///
    /// `consumed_handles` are the handle names whose NFT was *spent* this block. Any not
    /// re-produced (absent from `changes`) is a burn/revoke and is removed, so a spent-and-not-
    /// recreated handle stops resolving to its stale former owner.
    ///
    /// Rollback-safe: mutates only this snapshot's (imbl, copy-on-write) maps, so truncating
    /// history reverts it.
    pub fn apply_handle_updates(
        &mut self,
        changes: &[(String, String)],
        consumed_handles: &[String],
    ) {
        for (handle, new_addr) in changes {
            let old = self.address_by_handle.get(handle).cloned();
            if old.as_deref() == Some(new_addr.as_str()) {
                continue; // already resolves here
            }
            if let Some(old) = &old {
                self.detach_handle(old, handle);
            }
            self.handle_by_address
                .entry(new_addr.clone())
                .or_default()
                .push(handle.clone());
            self.address_by_handle
                .insert(handle.clone(), new_addr.clone());
            // Stake-credential index: move the handle only if the owner's credential changed
            // (a move between two addresses of the same stake key leaves it in place).
            let old_cred = old.as_deref().and_then(stake_credential_from_bech32);
            let new_cred = stake_credential_from_bech32(new_addr);
            if old_cred != new_cred {
                if let Some(oc) = &old_cred {
                    self.detach_stake_handle(oc, handle);
                }
                if let Some(nc) = new_cred {
                    self.attach_stake_handle(nc, handle);
                }
            }
        }
        // Burns: a handle NFT spent this block and not re-produced anywhere in it.
        let produced: std::collections::HashSet<&str> =
            changes.iter().map(|(h, _)| h.as_str()).collect();
        for handle in consumed_handles {
            if produced.contains(handle.as_str()) {
                continue; // moved, not burned
            }
            if let Some(old) = self.address_by_handle.remove(handle) {
                self.detach_handle(&old, handle);
                if let Some(oc) = stake_credential_from_bech32(&old) {
                    self.detach_stake_handle(&oc, handle);
                }
            }
        }
    }

    /// Shortest ADA Handle owned across every payment address of a stake credential, if any.
    pub fn handle_for_stake(&self, cred: &[u8]) -> Option<String> {
        self.handle_by_stake
            .get(cred)
            .and_then(|handles| handles.iter().min_by_key(|h| h.len()).cloned())
    }

    /// Detach `handle` from `addr`'s list in `handle_by_address`, dropping the entry when empty.
    fn detach_handle(&mut self, addr: &str, handle: &str) {
        if let Some(list) = self.handle_by_address.get_mut(addr) {
            list.retain(|h| h != handle);
            if list.is_empty() {
                self.handle_by_address.remove(addr);
            }
        }
    }

    /// Detach `handle` from `cred`'s list in `handle_by_stake`, dropping the entry when empty.
    fn detach_stake_handle(&mut self, cred: &[u8], handle: &str) {
        if let Some(list) = self.handle_by_stake.get_mut(cred) {
            list.retain(|h| h != handle);
            if list.is_empty() {
                self.handle_by_stake.remove(cred);
            }
        }
    }

    /// Attach `handle` to `cred`'s list in `handle_by_stake` (deduped).
    fn attach_stake_handle(&mut self, cred: Vec<u8>, handle: &str) {
        let list = self.handle_by_stake.entry(cred).or_default();
        if !list.iter().any(|h| h == handle) {
            list.push(handle.to_string());
        }
    }
}

/// Build the stake-credential → handles index from `handle_by_address` (at reset/load). A handle's
/// stake credential is that of its bech32 owner address; enterprise/pointer addresses (no stake
/// part) contribute nothing.
fn build_handle_by_stake(
    handle_by_address: &HashMap<String, Vec<String>>,
) -> HashMap<Vec<u8>, Vec<String>> {
    let mut out: HashMap<Vec<u8>, Vec<String>> = HashMap::new();
    for (addr, handles) in handle_by_address.iter() {
        if let Some(cred) = stake_credential_from_bech32(addr) {
            out.entry(cred).or_default().extend(handles.iter().cloned());
        }
    }
    out
}

/// A `(policy bytes, name bytes)` token — the unit a live assets-grid tile is keyed by;
/// its CIP-14 fingerprint is derived on demand via `asset_fingerprint`.
pub type Token = (Vec<u8>, Vec<u8>);

/// The global per-address holdings map ([`BlockSnapshot::asset_holdings`]'s type). A
/// connection caches the previous block's handle (O(1)) and diffs the current one for
/// its live grid tiles.
pub type AssetHoldings = OrdMap<HeldKey, Held>;

/// Apply a single token's `±qty` to one `(address, policy, name)` entry in the flat
/// holdings map. An entry is pruned when it hits 0, so the keys stay exactly the tokens
/// currently held. On credit, a fresh entry records `mint_time` (the asset's first-mint
/// time); an existing entry keeps its recorded time. Live tile deltas are *not* emitted
/// here — each open assets page derives them by diffing snapshots; this only maintains the map.
fn bump_one(
    holdings: &mut AssetHoldings,
    addr: &Arc<AddrKey>,
    policy: &[u8],
    name: &[u8],
    qty: u128,
    add: bool,
    mint_slot: u32,
) {
    let hkey = (addr.clone(), asset_id(policy, name));
    if add {
        let h = holdings.entry(hkey).or_default();
        h.set_qty(h.qty() + qty);
        // Record the asset's first-mint slot on the first credit to this leaf; keep any
        // slot already recorded (all leaves of one asset share the same mint slot).
        if h.mint_slot() == 0 {
            h.set_mint_slot(mint_slot);
        }
    } else {
        // produced/consumed amounts balance exactly; saturate as a guard against any stray
        // underflow rather than panic, and prune the entry at 0.
        let drop = match holdings.get_mut(&hkey) {
            Some(h) => {
                h.set_qty(h.qty().saturating_sub(qty));
                h.qty() == 0
            }
            None => false,
        };
        if drop {
            holdings.remove(&hkey);
        }
    }
}

/// Apply one UTXO's policy-grouped assets to the global holdings map (`add` = produced,
/// else consumed), computing the UTXO's stake credential once. On credit, each asset's
/// first-mint slot is looked up in `mint_slots` (per-block asset→slot map), falling back to
/// `block_slot` for a freshly minted asset. Maintained for *every* address; live tile deltas
/// are derived per connection by diffing, not emitted here.
fn apply_utxo_assets(
    address: &[u8],
    assets: &crate::model::PolicyAssets,
    add: bool,
    holdings: &mut AssetHoldings,
    interner: &mut AddrInterner,
    mint_slots: &std::collections::HashMap<AssetId, u32>,
    block_slot: u32,
) {
    if assets.is_empty() {
        return;
    }
    let cred = stake_credential_from_address_bytes(address);
    // Intern once per output; every token of this address shares the resulting Arc.
    let addr = intern_addr(interner, cred.as_deref(), address);
    for (policy, names) in assets {
        for (name, qty) in names {
            let mt = if add {
                mint_slots
                    .get(&asset_id(policy, name))
                    .copied()
                    .unwrap_or(block_slot)
            } else {
                0
            };
            bump_one(holdings, &addr, policy, name, *qty as u128, add, mt);
        }
    }
}

/// The credential's held tokens — the contiguous `(Some(cred), …)` key prefix of the flat
/// map, yielding `(&HeldKey, &Held)`. Shared by the count/grid/diff helpers.
fn cred_range<'a>(
    holdings: &'a AssetHoldings,
    cred: &'a [u8],
) -> impl Iterator<Item = (&'a HeldKey, &'a Held)> {
    let start = bound_key(AddrKey::new(Some(cred), &[]));
    holdings
        .range(start..)
        .take_while(move |((k, _), _)| k.cred.as_deref() == Some(cred))
}

/// One payment address's held tokens — the contiguous `((cred, addr), …)` key prefix.
fn addr_range<'a>(
    holdings: &'a AssetHoldings,
    key: &'a AssetKey,
) -> impl Iterator<Item = (&'a HeldKey, &'a Held)> {
    let target = AddrKey::from_query(key);
    let start = bound_key(target.clone());
    holdings
        .range(start..)
        .take_while(move |((k, _), _)| k.as_ref() == &target)
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
    let target = AddrKey::from_query(key);
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for d in prev.diff(curr) {
        match d {
            DiffItem::Add((k, asset), _) if k.as_ref() == &target => {
                let (policy, name) = split_asset(asset);
                added.push((policy.to_vec(), name.to_vec()));
            }
            DiffItem::Remove((k, asset), _) if k.as_ref() == &target => {
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
        if k.cred.as_deref() == Some(cred) {
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
    HashMap<Vec<u8>, Delegation>,
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
    /// Governance votes cast in this block per DRep (tagged key → count; a tx voting on
    /// several actions counts once per action). Bumps `drep_vote_counts`.
    pub drep_votes: &'a std::collections::HashMap<Vec<u8>, u32>,
}

pub struct State {
    history: Vec<BlockSnapshot>,
    db_url: Url,
    /// One connection pool **per tokio runtime**. sqlx binds a connection to the runtime that
    /// created it, and this process has several: gasket gives every stage its own
    /// current-thread runtime (`fullfil_stage`), the startup populates use a temporary one,
    /// and axum has another. A single shared pool therefore hands a connection to a runtime
    /// whose reactor never polls it — the acquire then blocks for the whole `acquire_timeout`
    /// (sqlx's default is 30s) before giving up and opening a fresh one. That was exactly the
    /// 30s stall the first block of every catch-up paid, with one key to resolve.
    db: std::sync::Mutex<std::collections::HashMap<tokio::runtime::Id, DbSync>>,
    pub feed_index: FeedIndex,
    // In-memory cursors for the live off-chain metadata refresh (pool tickers / DRep
    // names). Not in BlockSnapshot, so the persisted snapshot stays independent of
    // db-sync ids. Seeded to the current max at `reset`; left at 0 on warm resume so
    // the first post-catch-up block backfills (and a rollback resets them to 0).
    pub pool_meta_cursor: i64,
    pub drep_meta_cursor: i64,
    /// Interns the `(cred, addr)` half of every holdings key so an address's tokens share one
    /// `Arc<AddrKey>`. Not persisted (rebuilt at `reset` / re-interned on snapshot load);
    /// rollback-safe because it only ever grows and its entries are content-addressed.
    addr_interner: AddrInterner,
    /// Per-pool / per-DRep **active (epoch) stake**, the stable denominator for the stake-change
    /// significance threshold used by both the feed-index build filter (`sink`) and the query
    /// render filter (`server`). Kept in `State`, not `BlockSnapshot`: active stake is fixed for
    /// an epoch, and a rollback never crosses a 5-day epoch boundary, so it needs no history.
    /// Refreshed from db-sync at startup and on every epoch boundary (`populate_active_stakes`);
    /// a subject missing here yields threshold 0 (everything renders — the safe fallback).
    pub pool_active_stake: std::collections::HashMap<Vec<u8>, u64>,
    pub drep_active_stake: std::collections::HashMap<Vec<u8>, u64>,
}

/// Stake-change significance threshold: a block is surfaced on a pool/DRep feed when a single tx
/// moves more than `active_stake / STAKE_CHANGE_DIVISOR` of the subject's stake (0.1%).
pub const STAKE_CHANGE_DIVISOR: u64 = 1_000;

impl State {
    pub fn new(db_url: Url) -> Self {
        Self {
            history: Vec::new(),
            db_url,
            db: std::sync::Mutex::new(std::collections::HashMap::new()),
            feed_index: FeedIndex::new(),
            pool_meta_cursor: 0,
            drep_meta_cursor: 0,
            addr_interner: AddrInterner::new(),
            pool_active_stake: std::collections::HashMap::new(),
            drep_active_stake: std::collections::HashMap::new(),
        }
    }

    /// The active-stake significance threshold for a pool (`active_stake / STAKE_CHANGE_DIVISOR`).
    /// 0 when the pool isn't in the active-stake map (surfaces everything — the safe fallback).
    pub fn pool_stake_threshold(&self, pool: &[u8]) -> u64 {
        self.pool_active_stake.get(pool).copied().unwrap_or(0) / STAKE_CHANGE_DIVISOR
    }

    /// The active-stake significance threshold for a DRep. See [`Self::pool_stake_threshold`].
    pub fn drep_stake_threshold(&self, drep: &[u8]) -> u64 {
        self.drep_active_stake.get(drep).copied().unwrap_or(0) / STAKE_CHANGE_DIVISOR
    }

    /// Refresh `pool_active_stake` / `drep_active_stake` from db-sync for `epoch`. Two aggregate
    /// queries (~3k pools + ~900 DReps); called at startup and on each epoch boundary. On query
    /// error the previous map is kept (a stale-by-one-epoch denominator is harmless).
    pub async fn populate_active_stakes(&mut self, epoch: u64) {
        // `db_handle()` only returns an already-initialized connection; make sure it's initialized
        // first (like every other populate), then take an owned clone so we can assign self's maps
        // without holding a borrow of self.
        if self.db().await.is_none() {
            tracing::warn!(epoch, "active stakes: no db connection");
            return;
        }
        let Some(db) = self.db_handle() else { return };
        // Keep the previous (still-valid) map on an empty result or error — never wipe it, or the
        // significance threshold collapses to 0 and every tiny change leaks through.
        match db.pool_active_stakes(epoch).await {
            Ok(rows) if !rows.is_empty() => {
                self.pool_active_stake = rows
                    .into_iter()
                    .map(|(h, a)| (h, a.max(0) as u64))
                    .collect();
            }
            Ok(_) => tracing::warn!(
                epoch,
                "active stakes: pool query returned 0 rows; kept prior"
            ),
            Err(e) => {
                tracing::warn!(epoch, error = %e, "active stakes: pool query failed; kept prior")
            }
        }
        match db.drep_active_stakes(epoch).await {
            Ok(rows) if !rows.is_empty() => {
                self.drep_active_stake = rows
                    .into_iter()
                    .map(|(h, a)| (h, a.max(0) as u64))
                    .collect();
            }
            Ok(_) => tracing::warn!(
                epoch,
                "active stakes: drep query returned 0 rows; kept prior"
            ),
            Err(e) => {
                tracing::warn!(epoch, error = %e, "active stakes: drep query failed; kept prior")
            }
        }
        // Totals as a sanity check: pools should sum to ~all delegated stake (~22B ADA), dreps to
        // the DRep-delegated subset. Reported in ADA (lovelace / 1e6).
        let sum_pool: u128 = self.pool_active_stake.values().map(|&v| v as u128).sum();
        let sum_drep: u128 = self.drep_active_stake.values().map(|&v| v as u128).sum();
        tracing::info!(
            pools = self.pool_active_stake.len(),
            dreps = self.drep_active_stake.len(),
            total_pool_active_stake_ada = (sum_pool / 1_000_000) as u64,
            total_drep_active_stake_ada = (sum_drep / 1_000_000) as u64,
            epoch,
            "active stakes populated from db-sync"
        );
    }

    /// Detailed per-field memory breakdown of everything held in memory: the tip snapshot's
    /// maps (via [`BlockSnapshot::log_memory`]), the [`FeedIndex`], and the `history` depth.
    /// The per-field bytes are for the **tip** snapshot; the older `history` snapshots share
    /// structure (imbl O(1) clone) and add only their per-block diffs, so they are reported
    /// as a depth, not summed. Call once after the snapshot is loaded or rebuilt.
    pub fn log_memory(&self, label: &str) {
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return; // see `BlockSnapshot::log_memory`: the walk itself is the cost
        }
        if let Some(snap) = self.history.last() {
            snap.log_memory(label);
        }
        self.feed_index.log_memory(label);
        // The interned `(cred, addr)` pool: distinct addresses holding tokens, plus the shared
        // cred + addr bytes stored once each (vs once per held token before interning).
        {
            let mb = |b: usize| b / (1024 * 1024);
            let mut addr_bytes = 0usize;
            for k in self.addr_interner.keys() {
                addr_bytes += k.cred.as_ref().map_or(0, |c| c.len()) + k.addr.len();
            }
            // Per distinct: the AddrKey struct + its ArcInner control block, both interned once.
            let inline = self.addr_interner.len()
                * (std::mem::size_of::<AddrKey>() + 2 * std::mem::size_of::<usize>());
            tracing::debug!(
                label,
                distinct_addrs = self.addr_interner.len(),
                shared_addr_bytes_mb = mb(addr_bytes),
                inline_mb = mb(inline),
                "memory: addr_interner (shared (cred, addr) — counted once, not per token)",
            );
        }
        tracing::debug!(
            label,
            history_snapshots = self.history.len(),
            "memory: state (per-field bytes are the tip snapshot; history entries share \
             structure and add only per-block diffs)",
        );
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
        let (holdings, interner) = {
            let Some(db) = self.db().await else { return };
            let mint_times = match Self::fetch_asset_mint_times(&db).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("failed to fetch asset mint times: {e}");
                    return;
                }
            };
            match Self::fetch_asset_holdings(&db, last_tx_id, &mint_times).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("failed to fetch asset holdings: {e}");
                    return;
                }
            }
        };
        self.addr_interner = interner;
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
        pool_delegations: &HashMap<Vec<u8>, Delegation>,
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

    /// Backfill `Delegation::since_slot` from db-sync when resuming from a snapshot saved
    /// before the field existed (every slot 0). `reset` gets it inline from the delegation
    /// queries; thereafter `apply_delegation_changes` maintains it. Gated on all-zero, so a
    /// populated snapshot is left untouched — and it re-runs the two full delegation queries,
    /// so it only ever fires on that one transitional resume.
    pub async fn populate_delegation_slots(&mut self) {
        let needs = self
            .history
            .last()
            .map(|s| {
                !s.pool_delegations.is_empty()
                    && s.pool_delegations.values().all(|d| d.since_slot == 0)
            })
            .unwrap_or(false);
        if !needs {
            return;
        }
        let Some(snap_slot) = self.history.last().map(|s| s.slot) else {
            return;
        };
        let Some(db) = self.db().await else { return };
        let last_tx_id = match db.slot_info(snap_slot).await {
            Ok((id, _)) => id,
            Err(e) => {
                tracing::warn!("failed to resolve slot {snap_slot} for delegation slots: {e}");
                return;
            }
        };
        let pool = match db.pool_delegations(last_tx_id).await {
            Ok((d, _)) => d,
            Err(e) => {
                tracing::warn!("failed to fetch pool delegation slots: {e}");
                return;
            }
        };
        let drep = match db.drep_delegations(last_tx_id).await {
            Ok((d, _)) => d,
            Err(e) => {
                tracing::warn!("failed to fetch drep delegation slots: {e}");
                return;
            }
        };
        let Some(snap) = self.history.last_mut() else {
            return;
        };
        // Only the slot is taken from db-sync: the target and the delegator sets stay as the
        // snapshot has them (it is at the chain tip, db-sync may lag a few seconds).
        let apply = |current: &mut HashMap<Vec<u8>, Delegation>,
                     fresh: &HashMap<Vec<u8>, Delegation>| {
            let keys: Vec<Vec<u8>> = current.keys().cloned().collect();
            for key in keys {
                let Some(slot) = fresh.get(&key).map(|d| d.since_slot) else {
                    continue;
                };
                if let Some(d) = current.get_mut(&key) {
                    d.since_slot = slot;
                }
            }
        };
        apply(&mut snap.pool_delegations, &pool);
        apply(&mut snap.drep_delegations, &drep);
        tracing::info!(
            pool_delegations = pool.len(),
            drep_delegations = drep.len(),
            "delegation start slots backfilled from db-sync"
        );
    }

    /// Backfill `BlockSnapshot::drep_vote_counts` from db-sync when resuming from a snapshot
    /// saved before the field existed (empty map). `reset` fetches it inline; thereafter
    /// `apply_block` maintains it. Bounded by the snapshot's slot so votes in blocks the sink
    /// is about to replay aren't counted twice.
    pub async fn populate_drep_vote_counts(&mut self, epoch: u64) {
        let needs = self
            .history
            .last()
            .map(|s| s.drep_vote_counts.is_empty())
            .unwrap_or(false);
        if !needs {
            return;
        }
        let Some(snap_slot) = self.history.last().map(|s| s.slot) else {
            return;
        };
        let Some(db) = self.db().await else { return };
        let last_tx_id = match db.slot_info(snap_slot).await {
            Ok((id, _)) => id,
            Err(e) => {
                tracing::warn!("failed to resolve slot {snap_slot} for drep vote counts: {e}");
                return;
            }
        };
        let counts = match db.drep_vote_counts(last_tx_id, epoch).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to fetch drep vote counts: {e}");
                return;
            }
        };
        let Some(snap) = self.history.last_mut() else {
            return;
        };
        for (drep, (total, epoch_votes)) in counts {
            snap.drep_vote_counts.insert(
                drep,
                DRepVotes {
                    total,
                    epoch,
                    epoch_votes,
                },
            );
        }
        tracing::info!(
            dreps = snap.drep_vote_counts.len(),
            "drep vote counts backfilled from db-sync"
        );
    }

    /// Populate ADA Handle cache from db-sync if empty.
    pub async fn populate_handles(&mut self) {
        let is_empty = self
            .history
            .last()
            .map(|s| s.address_by_handle.is_empty())
            .unwrap_or(true);
        // Fetch + resolve from db-sync only when the loaded snapshot has no handles yet; a warm
        // snapshot with handles keeps them (they're maintained live per block since).
        if is_empty {
            let policies: Vec<&[u8]> = HANDLE_POLICIES.iter().map(|p| p.as_slice()).collect();
            if let Some(db) = self.db().await {
                match db.handles(&policies).await {
                    Ok(rows) => {
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
                    Err(e) => tracing::warn!("failed to fetch handles: {e}"),
                }
            }
        }
        // (Re)build the derived stake-credential index from handle_by_address — it's
        // `#[serde(skip)]`, so it's empty right after a snapshot load; also covers the
        // freshly-populated case above. One cheap pass over the resolved handles.
        if let Some(snap) = self.history.last_mut() {
            snap.handle_by_stake = build_handle_by_stake(&snap.handle_by_address);
        }
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

    /// The pool for the *current* runtime, created on first use. `DbSync` is a cheap handle
    /// (an `Arc`ed `PgPool`) so callers get an owned clone, and creation doesn't connect —
    /// hence no await, and [`Self::db_handle`] can do exactly the same thing synchronously.
    pub(crate) async fn db(&self) -> Option<DbSync> {
        self.db_handle()
    }

    /// Synchronous, lock-friendly clone of the db handle for callers that
    /// want to run a db query *without* holding the `chain_state` lock for
    /// its duration (avoiding head-of-line blocking when one slow query —
    /// e.g. a whale's `assets_count` — would otherwise stall every other
    /// reader behind the sink's pending writer).
    ///
    /// Creates this runtime's pool if it doesn't have one yet, so it is `Some` for any caller
    /// with a valid db URL — a handler must never be silently starved of the db because some
    /// other runtime happened to open the pool first.
    pub fn db_handle(&self) -> Option<DbSync> {
        let id = tokio::runtime::Handle::try_current().ok()?.id();
        let mut pools = self.db.lock().ok()?;
        if let Some(db) = pools.get(&id) {
            return Some(db.clone());
        }
        // Creating a pool is just building the config (`connect_lazy_with`), so this stays
        // sync and cheap; the first query on it opens a connection on *this* runtime.
        let db = DbSync::new(&self.db_url).ok()?;
        Some(pools.entry(id).or_insert(db).clone())
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

    /// Build the `ident → first-mint slot (u32)` map from db-sync — the source of the
    /// holdings-leaf `mint_slot`. Streamed (never one big `Vec`); a heavy one-time cold-start
    /// aggregate (~minutes), so warm resume reads mint slots from the snapshot instead.
    async fn fetch_asset_mint_times(
        db: &DbSync,
    ) -> Result<std::collections::HashMap<i64, u32>, sqlx::Error> {
        let mut map: std::collections::HashMap<i64, u32> = std::collections::HashMap::new();
        let mut rows: u64 = 0;
        db.asset_mint_times_for_each(|ident, first_mint| {
            rows += 1;
            // Slots are positive and fit u32 for centuries (the leaf keeps the low 30 bits).
            map.insert(ident, first_mint.max(0) as u32);
            if rows.is_multiple_of(2_000_000) {
                tracing::info!(
                    rss_mb = rss_mb(),
                    rows,
                    "asset mint times: loading (streaming)"
                );
            }
        })
        .await?;
        tracing::debug!(
            rss_mb = rss_mb(),
            assets = map.len(),
            "asset mint times loaded from db-sync"
        );
        Ok(map)
    }

    /// Build the global [`BlockSnapshot::asset_holdings`] map from db-sync's
    /// `(bech32 address, fingerprint, unspent-UTXO count)` stream (ordered by address),
    /// computing each address's stake credential once. Streamed row-by-row so the full
    /// ~15M-row result never materializes as one big `Vec`. The heavy cold-start query;
    /// warm resume deserializes the map instead.
    async fn fetch_asset_holdings(
        db: &DbSync,
        last_tx_id: i64,
        mint_times: &std::collections::HashMap<i64, u32>,
    ) -> Result<(AssetHoldings, AddrInterner), sqlx::Error> {
        use pallas::ledger::addresses::Address;
        let mut holdings: AssetHoldings = OrdMap::new();
        let mut interner: AddrInterner = AddrInterner::new();
        // The query is ordered by address, so decode + intern each `(cred, addr)` once when the
        // address changes and reuse the shared `Arc` across all of that address's rows.
        let mut cur_addr: Option<String> = None;
        let mut cur_arc: Option<Arc<AddrKey>> = None;
        let mut rows: u64 = 0;
        db.asset_holdings_for_each(last_tx_id, |addr, policy, name, count, ident| {
            rows += 1;
            if cur_addr.as_deref() != Some(addr.as_str()) {
                cur_arc = Address::from_bech32(&addr).ok().map(|a| {
                    let bytes = a.to_vec();
                    let cred = stake_credential_from_address_bytes(&bytes);
                    intern_addr(&mut interner, cred.as_deref(), &bytes)
                });
                cur_addr = Some(addr);
            }
            // `count` is the summed quantity as text; parse to u128 (saturates only at the
            // absurd u128 ceiling, far beyond any real token — so no precision is lost).
            let qty = count.parse::<u128>().unwrap_or(u128::MAX);
            if let (Some(arc), true) = (&cur_arc, qty > 0) {
                let mint_time = mint_times.get(&ident).copied().unwrap_or(0);
                holdings.insert(
                    (arc.clone(), asset_id(&policy, &name)),
                    Held::new(qty, mint_time),
                );
            }
            if rows.is_multiple_of(1_000_000) {
                tracing::info!(
                    rss_mb = rss_mb(),
                    rows,
                    entries = holdings.len(),
                    interned = interner.len(),
                    "asset holdings: building (streaming)"
                );
            }
        })
        .await?;
        tracing::debug!(
            rss_mb = rss_mb(),
            rows,
            entries = holdings.len(),
            interned = interner.len(),
            "asset holdings built from db-sync"
        );
        Ok((holdings, interner))
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
        // `reset` runs on whatever runtime called it; `db()` gives that runtime its own pool.
        let db = match self.db().await {
            Some(db) => db,
            None => DbSync::new(&self.db_url)?,
        };

        let (last_tx_id, block_hash) = db.slot_info(slot).await?;
        tracing::debug!(
            rss_mb = rss_mb(),
            "reset: start (rebuilding state from db-sync)"
        );

        tracing::info!("Fetching pools...");
        let pools = db.pools(last_tx_id, slot as i64).await?;
        tracing::info!("{} pools retrieved", pools.len());

        tracing::info!("Fetching pool delegations...");
        let (pool_delegations, pool_delegators) = db.pool_delegations(last_tx_id).await?;
        tracing::debug!(
            rss_mb = rss_mb(),
            "{} pool delegations in {} pools retrieved",
            pool_delegations.len(),
            pool_delegators.len()
        );

        tracing::info!("Fetching DRep delegations...");
        let (drep_delegations, drep_delegators) = db.drep_delegations(last_tx_id).await?;
        tracing::debug!(
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
        tracing::debug!(
            rss_mb = rss_mb(),
            "{} stake addresses with rewards",
            rewards.len()
        );

        tracing::info!("Fetching DRep metadata...");
        let dreps = db.drep_metadata(last_tx_id, 0).await?;
        tracing::info!("{} DReps with metadata", dreps.len());

        tracing::info!("Fetching DRep vote counts...");
        let drep_vote_counts: HashMap<Vec<u8>, DRepVotes> = db
            .drep_vote_counts(last_tx_id, current_epoch)
            .await?
            .into_iter()
            .map(|(k, (total, epoch_votes))| {
                (
                    k,
                    DRepVotes {
                        total,
                        epoch: current_epoch,
                        epoch_votes,
                    },
                )
            })
            .collect();
        tracing::info!("{} DReps with governance votes", drep_vote_counts.len());

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
        tracing::debug!(
            rss_mb = rss_mb(),
            "{} address-balance rows fetched (transient Vec)",
            balance_rows.len()
        );
        let (address_balances, stakes) = Self::balances_and_stakes(balance_rows);
        tracing::debug!(
            rss_mb = rss_mb(),
            "{} addresses with UTXOs, {} stake credentials",
            address_balances.len(),
            stakes.len()
        );

        let total_staked = Self::sum_delegated_stake(&pool_delegations, &stakes, &rewards);

        tracing::info!("Fetching asset first-mint times...");
        let mint_times = Self::fetch_asset_mint_times(&db).await?;
        tracing::info!("Fetching per-address asset holdings...");
        let (asset_holdings, addr_interner) =
            Self::fetch_asset_holdings(&db, last_tx_id, &mint_times).await?;
        drop(mint_times);
        self.addr_interner = addr_interner;

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
            drep_vote_counts,
            stakes,
            rewards,
            decimals,
            handle_by_stake: build_handle_by_stake(&handle_by_address),
            handle_by_address,
            address_by_handle,
            gov_action_titles,
            asset_holdings,
            asset_holdings_populated: true,
        });
        self.feed_index = FeedIndex::new();
        self.pool_meta_cursor = pool_meta_cursor;
        self.drep_meta_cursor = drep_meta_cursor;
        // Active-stake denominators for the feed-index significance filter — must be in place
        // before the 5-day catch-up rebuilds the index through `apply_block`.
        self.populate_active_stakes(current_epoch).await;

        if let Some(snap) = self.history.last() {
            snap.log_sizes("reset complete");
        }
        self.log_memory("reset complete");
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
            drep_votes,
        } = update;

        let prev = self.history.last().expect("state not initialized");
        // Own the interner for the duration (disjoint field from `self.history`, which `prev`
        // borrows); restored below once the holdings mutations are done.
        let mut interner = std::mem::take(&mut self.addr_interner);

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
        // Per-block asset → first-mint slot for crediting new leaves. By ledger conservation
        // every output asset either comes from an input this block (carry its recorded slot
        // from that input's prior leaf) or is minted this block (fall back to this block's
        // `slot`). So seed the map from the consumed inputs' prior leaves; anything absent is a
        // new mint.
        let block_slot = slot as u32;
        let mut mint_slots: std::collections::HashMap<AssetId, u32> =
            std::collections::HashMap::new();
        for (_, output) in consumed {
            if output.assets.is_empty() {
                continue;
            }
            let cred = stake_credential_from_address_bytes(&output.address);
            let addr = Arc::new(AddrKey::new(cred.as_deref(), &output.address));
            for (policy, names) in &output.assets {
                for (name, _) in names {
                    let aid = asset_id(policy, name);
                    if let Some(h) = prev.asset_holdings.get(&(addr.clone(), aid.clone())) {
                        mint_slots.entry(aid).or_insert(h.mint_slot());
                    }
                }
            }
        }

        for (key, output) in produced {
            let bal: i64 = output
                .lovelaces
                .try_into()
                .expect("lovelace value must fit i64");
            *address_balances.entry(output.address.clone()).or_insert(0) += bal;
            apply_utxo_assets(
                &output.address,
                &output.assets,
                true,
                &mut asset_holdings,
                &mut interner,
                &mint_slots,
                block_slot,
            );
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
            apply_utxo_assets(
                &output.address,
                &output.assets,
                false,
                &mut asset_holdings,
                &mut interner,
                &mint_slots,
                block_slot,
            );
        }
        // Holdings mutations done — hand the interner back to `self`.
        self.addr_interner = interner;

        let (pool_delegations, pool_delegators) = Self::apply_delegation_changes(
            &prev.pool_delegations,
            &prev.pool_delegators,
            pool_delegation_changes,
            slot,
        );

        let (drep_delegations, drep_delegators) = Self::apply_delegation_changes(
            &prev.drep_delegations,
            &prev.drep_delegators,
            drep_delegation_changes,
            slot,
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
        // Governance votes cast in this block: bump each voter's lifetime total and its
        // current-epoch tally (self-resetting via the epoch stamp — see `DRepVotes`).
        let drep_vote_counts = if drep_votes.is_empty() {
            prev.drep_vote_counts.clone()
        } else {
            let mut counts = prev.drep_vote_counts.clone();
            for (drep, n) in drep_votes {
                counts.entry(drep.clone()).or_default().add(*n, epoch);
            }
            counts
        };
        let decimals = prev.decimals.clone();
        let handle_by_address = prev.handle_by_address.clone();
        let address_by_handle = prev.address_by_handle.clone();
        // Cloned forward (imbl O(1)); the sink's apply_handle_updates then applies this block's
        // moves/burns to all three handle maps on the new snapshot.
        let handle_by_stake = prev.handle_by_stake.clone();
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
            drep_vote_counts,
            stakes,
            rewards,
            decimals,
            address_balances,
            address_balances_populated,
            asset_holdings,
            asset_holdings_populated,
            handle_by_address,
            address_by_handle,
            handle_by_stake,
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
        prev_pool_delegations: &HashMap<Vec<u8>, Delegation>,
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

    /// Apply this block's delegation certs to the forward map and its reverse index.
    ///
    /// `slot` is the block's slot, used to stamp [`Delegation::since_slot`] when a credential
    /// *starts* a run with a target: re-delegating to the target you're already on carries the
    /// old slot over (that's not a new run), while switching targets — or returning after a
    /// deregistration, which arrives as `(cred, None)` and drops the entry — restamps it.
    fn apply_delegation_changes(
        prev_delegations: &HashMap<Vec<u8>, Delegation>,
        prev_delegators: &HashMap<Vec<u8>, HashSet<Vec<u8>>>,
        changes: &[(Vec<u8>, Option<Vec<u8>>)],
        slot: u64,
    ) -> DelegationIndex {
        if changes.is_empty() {
            return (prev_delegations.clone(), prev_delegators.clone());
        }
        let mut delegations = prev_delegations.clone();
        let mut delegators = prev_delegators.clone();
        for (stake_addr, maybe_target) in changes {
            let old = delegations.remove(stake_addr);
            if let Some(old) = &old {
                if let Some(set) = delegators.get_mut(old.target.as_ref()) {
                    set.remove(stake_addr);
                }
            }
            if let Some(target) = maybe_target {
                let since_slot = match &old {
                    Some(old) if old.target.as_ref() == target.as_slice() => old.since_slot,
                    _ => slot,
                };
                delegations.insert(stake_addr.clone(), Delegation::new(target, since_slot));
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
        let db = DbSync::new(db_url).ok()?;
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
    /// Install a loaded snapshot + its interner (built by [`load_snapshot`], which already
    /// interned each holdings key as it streamed — nothing to rebuild here).
    pub fn restore_from_snapshot(&mut self, snapshot: BlockSnapshot, interner: AddrInterner) {
        self.addr_interner = interner;
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

    /// Load `(snapshot, feed_index, interner)` from disk. Validates format + magic **before**
    /// reading the multi-GB holdings map, so a stale/foreign snapshot is rejected cheaply
    /// (no ~10 GB deserialize-then-drop, whose freed pages glibc would retain).
    ///
    /// Deserializes **straight from a buffered file reader** rather than reading the whole
    /// file into a `Vec<u8>` first: that byte buffer would be a multi-GB transient stacked
    /// on the structures being built (the startup RSS spike). The `BufReader` streams it, and
    /// [`HoldingsSeed`] interns each holdings key as it arrives (no un-shared full map).
    pub fn load_snapshot(
        path: &Path,
        network_magic: u64,
    ) -> Option<(BlockSnapshot, FeedIndex, AddrInterner)> {
        use serde::de::DeserializeSeed;
        use serde::Deserialize;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!("failed to read snapshot from {}: {}", path.display(), e);
                return None;
            }
        };
        // Timed: on mainnet this is the single longest step of a warm start (a multi-GB
        // msgpack decode of ~15M holdings entries), and without an elapsed figure the gap
        // to the next log line reads like whichever db populate happens to log next.
        let started = std::time::Instant::now();
        let file_mb = file
            .metadata()
            .map(|m| m.len() / (1024 * 1024))
            .unwrap_or(0);
        tracing::info!(file_mb, "loading snapshot from {}...", path.display());
        // A big read buffer: the default 8 KB would mean ~450k syscalls over a multi-GB
        // snapshot. Disk space is cheap here, start-up latency isn't.
        const READ_BUF: usize = 8 * 1024 * 1024;
        let rd = std::io::BufReader::with_capacity(READ_BUF, file);
        let mut de = rmp_serde::Deserializer::new(rd);

        // Read the guards first — cheap to reject before touching the big map.
        let format = match u32::deserialize(&mut de) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("failed to read snapshot format: {} — rebuilding", e);
                return None;
            }
        };
        let policy_grouped = match format {
            SNAPSHOT_FORMAT => true,
            SNAPSHOT_FORMAT_LEGACY_PER_TOKEN => {
                tracing::info!(
                    "snapshot is format {} (a policy id per token) — reading it once; the next \
                     periodic write stores format {}",
                    SNAPSHOT_FORMAT_LEGACY_PER_TOKEN,
                    SNAPSHOT_FORMAT
                );
                false
            }
            _ => {
                tracing::warn!(
                    "snapshot format mismatch: snapshot={}, expected={} — rebuilding from db-sync",
                    format,
                    SNAPSHOT_FORMAT
                );
                return None;
            }
        };
        match u64::deserialize(&mut de) {
            Ok(magic) if magic == network_magic => {}
            Ok(magic) => {
                tracing::warn!(
                    "snapshot network mismatch: snapshot={}, expected={}",
                    magic,
                    network_magic
                );
                return None;
            }
            Err(e) => {
                tracing::warn!("failed to read snapshot magic: {}", e);
                return None;
            }
        }

        let mut interner = AddrInterner::new();
        // Per-section timings: the three parts have very different shapes (the fields are
        // several million small map inserts, the holdings ~15M inserts plus interning), so a
        // single total can't tell you which one to attack.
        let (mut fields_ms, mut holdings_ms, mut index_ms) = (0u64, 0u64, 0u64);
        let result = (|| -> Result<(BlockSnapshot, FeedIndex), rmp_serde::decode::Error> {
            let t = std::time::Instant::now();
            let mut snap = BlockSnapshot::deserialize(&mut de)?; // asset_holdings skipped → empty
            fields_ms = t.elapsed().as_millis() as u64;

            let t = std::time::Instant::now();
            snap.asset_holdings = if policy_grouped {
                HoldingsSeed(&mut interner).deserialize(&mut de)?
            } else {
                LegacyHoldingsSeed(&mut interner).deserialize(&mut de)?
            };
            holdings_ms = t.elapsed().as_millis() as u64;

            let t = std::time::Instant::now();
            let fi = FeedIndex::deserialize(&mut de)?;
            index_ms = t.elapsed().as_millis() as u64;
            Ok((snap, fi))
        })();
        match result {
            Ok((snap, fi)) => {
                tracing::info!(
                    file_mb,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    fields_ms,
                    holdings_ms,
                    index_ms,
                    holdings = snap.asset_holdings.len(),
                    slot = snap.slot,
                    "snapshot loaded"
                );
                Some((snap, fi, interner))
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

    /// Test helper: a fresh (non-interned) `Arc<AddrKey>` for an `AssetKey`. Value equality is
    /// what the map compares, so a per-call Arc is fine for both mutation and lookup.
    fn ak(key: &AssetKey) -> Arc<AddrKey> {
        Arc::new(AddrKey::from_query(key))
    }

    // ---- ADA Handle live resolution (`BlockSnapshot::apply_handle_updates`) ----

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// A snapshot seeded with the given `handle → address` resolutions (both maps in sync).
    fn snap_with_handles(entries: &[(&str, &str)]) -> BlockSnapshot {
        let mut snap = BlockSnapshot::default();
        for (handle, addr) in entries {
            snap.address_by_handle.insert(s(handle), s(addr));
            snap.handle_by_address
                .entry(s(addr))
                .or_default()
                .push(s(handle));
        }
        snap
    }

    #[test]
    fn handle_mint_adds_new_resolution() {
        let mut snap = BlockSnapshot::default();
        snap.apply_handle_updates(&[(s("alice"), s("addrA"))], &[]);
        assert_eq!(snap.address_by_handle.get("alice"), Some(&s("addrA")));
        assert_eq!(snap.handle_for("addrA"), Some(s("alice")));
    }

    #[test]
    fn handle_move_reassigns_owner() {
        // classic / CIP-68 / virtual all reduce to (handle, new_owner) here; the datum
        // resolution that distinguishes them happens in the sink and is tested in model.rs.
        let mut snap = snap_with_handles(&[("alice", "addrA")]);
        snap.apply_handle_updates(&[(s("alice"), s("addrB"))], &[s("alice")]);
        assert_eq!(snap.handle_for("addrB"), Some(s("alice")));
        // Old owner drops out entirely (it held only this handle).
        assert_eq!(snap.handle_for("addrA"), None);
        assert!(!snap.handle_by_address.contains_key("addrA"));
    }

    #[test]
    fn handle_move_keeps_other_handles_at_old_address() {
        let mut snap = snap_with_handles(&[("alice", "addrA"), ("bob", "addrA")]);
        snap.apply_handle_updates(&[(s("alice"), s("addrB"))], &[s("alice")]);
        assert_eq!(snap.handle_by_address.get("addrA"), Some(&vec![s("bob")]));
        assert_eq!(snap.handle_for("addrB"), Some(s("alice")));
    }

    #[test]
    fn handle_no_op_when_owner_unchanged() {
        // Re-produced at the same address (e.g. a datum update that didn't move the owner):
        // no change, and no duplicate in the address's list.
        let mut snap = snap_with_handles(&[("alice", "addrA")]);
        snap.apply_handle_updates(&[(s("alice"), s("addrA"))], &[s("alice")]);
        assert_eq!(snap.handle_by_address.get("addrA"), Some(&vec![s("alice")]));
    }

    #[test]
    fn handle_burn_removes_resolution() {
        // Spent this block and not re-produced anywhere → burn/revoke.
        let mut snap = snap_with_handles(&[("alice", "addrA")]);
        snap.apply_handle_updates(&[], &[s("alice")]);
        assert_eq!(snap.address_by_handle.get("alice"), None);
        assert_eq!(snap.handle_for("addrA"), None);
        assert!(!snap.handle_by_address.contains_key("addrA"));
    }

    #[test]
    fn handle_spent_and_reproduced_is_a_move_not_a_burn() {
        // Consumed at addrA and produced at addrB in the same block: the produced side wins,
        // so it is a move — the burn pass must not then delete it.
        let mut snap = snap_with_handles(&[("alice", "addrA")]);
        snap.apply_handle_updates(&[(s("alice"), s("addrB"))], &[s("alice")]);
        assert_eq!(snap.handle_for("addrB"), Some(s("alice")));
        assert_eq!(snap.handle_for("addrA"), None);
    }

    // ---- Stake-credential handle index (`handle_by_stake` / `handle_for_stake`) ----

    /// A mainnet base address (type 0) with the given payment- and stake-hash fill bytes, so tests
    /// control which addresses share a stake credential. Its stake credential is `scred(stake)`.
    fn base_addr(payment: u8, stake: u8) -> String {
        let mut bytes = vec![0x01u8];
        bytes.extend(std::iter::repeat(payment).take(28));
        bytes.extend(std::iter::repeat(stake).take(28));
        bech32::encode::<bech32::Bech32>(bech32::Hrp::parse("addr").unwrap(), &bytes).unwrap()
    }
    fn scred(stake: u8) -> Vec<u8> {
        vec![stake; 28]
    }
    /// Seed all three handle maps through the real update path (mints).
    fn snap_with_stake_handles(entries: &[(&str, &str)]) -> BlockSnapshot {
        let mut snap = BlockSnapshot::default();
        let changes: Vec<(String, String)> = entries.iter().map(|(h, a)| (s(h), s(a))).collect();
        snap.apply_handle_updates(&changes, &[]);
        snap
    }

    #[test]
    fn stake_index_groups_addresses_of_one_credential_shortest_wins() {
        // Two different payment addresses, same stake key (stake byte 9): their handles unite
        // under that one credential; handle_for_stake returns the shortest.
        let snap =
            snap_with_stake_handles(&[("alice", &base_addr(1, 9)), ("bob", &base_addr(2, 9))]);
        assert_eq!(snap.handle_for_stake(&scred(9)), Some(s("bob"))); // "bob" < "alice"
    }

    #[test]
    fn stake_index_move_across_credentials() {
        let mut snap = snap_with_stake_handles(&[("alice", &base_addr(1, 9))]);
        // Move to a different stake key (byte 7).
        snap.apply_handle_updates(&[(s("alice"), base_addr(2, 7))], &[s("alice")]);
        assert_eq!(snap.handle_for_stake(&scred(9)), None);
        assert_eq!(snap.handle_for_stake(&scred(7)), Some(s("alice")));
    }

    #[test]
    fn stake_index_move_within_same_credential_keeps_it() {
        // Move between two payment addresses of the SAME stake key: the stake resolution is
        // unchanged (even though the payment address changed).
        let mut snap = snap_with_stake_handles(&[("alice", &base_addr(1, 9))]);
        snap.apply_handle_updates(&[(s("alice"), base_addr(2, 9))], &[s("alice")]);
        assert_eq!(snap.handle_for_stake(&scred(9)), Some(s("alice")));
    }

    #[test]
    fn stake_index_burn_removes() {
        let mut snap = snap_with_stake_handles(&[("alice", &base_addr(1, 9))]);
        snap.apply_handle_updates(&[], &[s("alice")]);
        assert_eq!(snap.handle_for_stake(&scred(9)), None);
    }

    #[test]
    fn stake_index_rebuilt_from_address_map() {
        // build_handle_by_stake (used at reset/load) groups handle_by_address by credential.
        let mut hba: HashMap<String, Vec<String>> = HashMap::new();
        hba.insert(base_addr(1, 9), vec![s("alice")]);
        hba.insert(base_addr(2, 9), vec![s("bob")]);
        hba.insert(base_addr(3, 5), vec![s("carol")]);
        // Enterprise/handle-less addresses without a stake part are skipped.
        hba.insert(s("not-an-address"), vec![s("ghost")]);
        let snap = BlockSnapshot {
            handle_by_stake: build_handle_by_stake(&hba),
            handle_by_address: hba,
            ..BlockSnapshot::default()
        };
        assert_eq!(snap.handle_for_stake(&scred(9)), Some(s("bob")));
        assert_eq!(snap.handle_for_stake(&scred(5)), Some(s("carol")));
    }

    #[test]
    fn bump_one_maintains_and_prunes() {
        let mut holdings: AssetHoldings = OrdMap::new();
        let key: AssetKey = (None, b"addr".to_vec());
        let policy = vec![0xaau8; 28];
        let name = b"TOKEN".to_vec();
        let arc = ak(&key);
        let hkey = (arc.clone(), asset_id(&policy, &name));

        bump_one(&mut holdings, &arc, &policy, &name, 1, true, 1000);
        assert_eq!(holdings[&hkey].qty(), 1);
        bump_one(&mut holdings, &arc, &policy, &name, 1, true, 1000);
        assert_eq!(holdings[&hkey].qty(), 2);
        bump_one(&mut holdings, &arc, &policy, &name, 1, false, 0);
        assert_eq!(holdings[&hkey].qty(), 1);

        // Last UTXO spent → the entry hits 0 and is pruned, so the map's keys stay exactly
        // the tokens currently held by ≥1 UTXO.
        bump_one(&mut holdings, &arc, &policy, &name, 1, false, 0);
        assert!(holdings.is_empty());

        // Spending a token the map never had is a no-op.
        bump_one(&mut holdings, &arc, &policy, &name, 1, false, 0);
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
        bump_one(&mut s1, &ak(&key1), &policy, &name, 1, true, 1000);

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
        bump_one(&mut s2, &ak(&key2), &policy, &name, 1, true, 1000);
        assert_eq!(stake_tile_diff(&s1, &s2, &cred), (vec![], vec![]));

        // s2 → s3: addr1 drops it; addr2 still holds → union unchanged.
        let mut s3 = s2.clone();
        bump_one(&mut s3, &ak(&key1), &policy, &name, 1, false, 0);
        assert_eq!(stake_tile_diff(&s2, &s3, &cred), (vec![], vec![]));

        // s3 → s4: addr2 drops it; nobody holds → union loses it.
        let mut s4 = s3.clone();
        bump_one(&mut s4, &ak(&key2), &policy, &name, 1, false, 0);
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
        prev_delegations.insert(cred_a.clone(), Delegation::new(&pool, 0));

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

    /// `Delegation::since_slot` marks the start of a credential's *current uninterrupted run*
    /// with a target: re-delegating to the same target carries it over, switching away — or a
    /// deregistration followed by a return — restamps it to the block that started the new run.
    #[test]
    fn delegation_since_slot_tracks_the_current_run() {
        let cred = vec![0xaa; 28];
        let pool_p = vec![0x01; 28];
        let pool_q = vec![0x02; 28];
        let empty_delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>> = HashMap::new();
        let since = |m: &HashMap<Vec<u8>, Delegation>| m.get(&cred).map(|d| d.since_slot);
        let target = |m: &HashMap<Vec<u8>, Delegation>| m.get(&cred).map(|d| d.target.to_vec());

        // First delegation at slot 100: the run starts here.
        let (d1, r1) = State::apply_delegation_changes(
            &HashMap::new(),
            &empty_delegators,
            &[(cred.clone(), Some(pool_p.clone()))],
            100,
        );
        assert_eq!(since(&d1), Some(100));
        assert_eq!(target(&d1), Some(pool_p.clone()));

        // Re-delegating to the SAME pool at slot 200 is not a new run — the clock doesn't move.
        let (d2, r2) =
            State::apply_delegation_changes(&d1, &r1, &[(cred.clone(), Some(pool_p.clone()))], 200);
        assert_eq!(since(&d2), Some(100));

        // Switching to another pool at slot 300 starts a new run.
        let (d3, r3) =
            State::apply_delegation_changes(&d2, &r2, &[(cred.clone(), Some(pool_q.clone()))], 300);
        assert_eq!(since(&d3), Some(300));
        assert_eq!(target(&d3), Some(pool_q.clone()));

        // Coming back to the first pool at slot 400 restamps too (the run was broken).
        let (d4, r4) =
            State::apply_delegation_changes(&d3, &r3, &[(cred.clone(), Some(pool_p.clone()))], 400);
        assert_eq!(since(&d4), Some(400));

        // A deregistration arrives as `(cred, None)`: the entry is dropped…
        let (d5, r5) = State::apply_delegation_changes(&d4, &r4, &[(cred.clone(), None)], 500);
        assert_eq!(since(&d5), None);
        // …so re-delegating to the very same pool afterwards starts a fresh run.
        let (d6, _) =
            State::apply_delegation_changes(&d5, &r5, &[(cred.clone(), Some(pool_p.clone()))], 600);
        assert_eq!(since(&d6), Some(600));

        // The reverse index follows the target, and the old set is left empty (not pruned).
        assert!(d6.len() == 1 && r4.get(&pool_q).unwrap().is_empty());
    }

    /// `db_handle()` is what every SSE handler uses to reach the db, from the axum runtime,
    /// inside await-free guard scopes. Pools are per-runtime, so it has to be able to *create*
    /// one — when it could only look one up, every db-backed detail (an asset's mint date,
    /// owner, policy, quantity; feed replay; search) silently vanished from the pages while
    /// the in-memory parts still rendered. Creating a pool doesn't connect, so this needs no
    /// database.
    #[test]
    fn db_handle_creates_a_pool_for_a_runtime_that_never_awaited_db() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = State::new(Url::parse("postgresql:///pool_pm_test").unwrap());
            assert!(
                state.db_handle().is_some(),
                "db_handle must open this runtime's pool, not wait for another one to do it"
            );
            // And it's the same pool on the second call, not a new one per query.
            let (a, b) = (state.db_handle(), state.db_handle());
            assert!(a.is_some() && b.is_some());
            assert_eq!(state.db.lock().unwrap().len(), 1);
        });
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
        let mut interner = AddrInterner::new();
        apply_utxo_assets(
            &addr1,
            &p1n1(10),
            true,
            &mut initial.asset_holdings,
            &mut interner,
            &std::collections::HashMap::new(),
            0,
        );
        apply_utxo_assets(
            &addr2,
            &p1n1(3),
            true,
            &mut initial.asset_holdings,
            &mut interner,
            &std::collections::HashMap::new(),
            0,
        );
        initial.stakes.insert(cred_a.clone(), 100);
        initial.stakes.insert(cred_b.clone(), 50);
        initial.rewards.insert(cred_a.clone(), 5);
        initial.rewards.insert(cred_b.clone(), 2);
        // cred_a has been on pool_p since slot 42 — the block below moves it to pool_q, which
        // must restamp the run to the block's slot.
        initial
            .pool_delegations
            .insert(cred_a.clone(), Delegation::new(&pool_p, 42));
        initial
            .pool_delegators
            .insert(pool_p.clone(), set(&[&cred_a]));
        initial
            .drep_delegations
            .insert(cred_a.clone(), Delegation::new(&drep_x, 42));
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
        state.restore_from_snapshot(initial.clone(), AddrInterner::new());

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
                                                   // drep_x votes twice in this block (one tx, two governance actions).
        let mut drep_votes = std::collections::HashMap::new();
        drep_votes.insert(drep_x.clone(), 2u32);

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
            drep_votes: &drep_votes,
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
        // addr2's token carries its original mint_slot (0); addr3's is a new mint stamped
        // with this block's slot 200 (policy2/name2 was in no consumed input).
        assert_eq!(
            cur.address_held_assets(&addr2),
            vec![(policy1.clone(), name1.clone(), 3u128, 0)]
        );
        assert_eq!(
            cur.address_held_assets(&addr3),
            vec![(policy2.clone(), name2.clone(), 7u128, 200)]
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

        // Pool delegations / delegators / live stake. Both credentials start a run with
        // pool_q in this block, so both are stamped with its slot (cred_a's old slot 42 is
        // dropped — it switched pools).
        assert_eq!(
            cur.pool_delegations.get(&cred_a),
            Some(&Delegation::new(&pool_q, 200))
        );
        assert_eq!(
            cur.pool_delegations.get(&cred_c),
            Some(&Delegation::new(&pool_q, 200))
        );
        let q_del = cur.pool_delegators.get(&pool_q).unwrap();
        assert!(q_del.len() == 2 && q_del.contains(&cred_a) && q_del.contains(&cred_c));
        assert!(cur.pool_delegators.get(&pool_p).unwrap().is_empty());
        assert_eq!(State::pool_live_stake(cur, &pool_q), Some(338)); // (125+3)+(200+10)
        assert_eq!(State::pool_live_stake(cur, &pool_p), Some(0));

        // DRep delegations / delegators / live stake.
        assert_eq!(cur.drep_delegations.get(&cred_a), None);
        assert_eq!(
            cur.drep_delegations.get(&cred_b),
            Some(&Delegation::new(&drep_x, 200))
        );
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

        // Governance votes: both counted, and the epoch tally is stamped with this block's
        // epoch — so it reads 2 for epoch 11 and 0 for any other (a DRep that hasn't voted
        // yet in the current epoch), while the lifetime total keeps growing.
        let votes = cur.drep_vote_counts.get(&drep_x).unwrap();
        assert_eq!(votes.total, 2);
        assert_eq!(votes.votes_in(11), 2);
        assert_eq!(votes.votes_in(12), 0);
        assert!(cur.drep_vote_counts.get(&drep_y).is_none());

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
        state.restore_from_snapshot(initial, AddrInterner::new());

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
            drep_votes: &std::collections::HashMap::new(),
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

    /// Live mint-time: a transferred asset carries its recorded first-mint time from the input
    /// it was spent from; an asset minted in the block (no matching input) gets the block time.
    #[test]
    fn apply_block_mint_time_carries_on_transfer_and_stamps_new_mints() {
        use rust_decimal::Decimal;

        let addr_a = [&[0x00u8][..], &[0x51; 28], &[0x61; 28]].concat();
        let addr_b = [&[0x00u8][..], &[0x52; 28], &[0x62; 28]].concat();
        let policy = vec![0x11u8; 28];
        let tok = b"TOK".to_vec(); // pre-existing, minted long ago (time 111)
        let new = b"NEW".to_vec(); // minted in this block
        let held = |addr: &[u8], name: &[u8], q: u64| TxOutput {
            lovelaces: Decimal::from(1_000_000u64),
            address: addr.to_vec(),
            assets: vec![(policy.clone(), vec![(name.to_vec(), q)])],
        };
        let leaf = |addr: &[u8], name: &[u8]| -> HeldKey {
            let cred = stake_credential_from_address_bytes(addr);
            (
                Arc::new(AddrKey::new(cred.as_deref(), addr)),
                asset_id(&policy, name),
            )
        };
        let (txh_old, txh_new) = (vec![0xa0u8; 32], vec![0xa1u8; 32]);

        // Initial: addr_a holds TOK (qty 1), first minted at time 111.
        let mut initial = BlockSnapshot {
            slot: 100,
            block_hash: Some("h".into()),
            last_epoch: Some(1),
            address_balances_populated: true,
            asset_holdings_populated: true,
            ..BlockSnapshot::default()
        };
        initial
            .utxos
            .insert((txh_old.clone(), 0), held(&addr_a, &tok, 1));
        initial
            .asset_holdings
            .insert(leaf(&addr_a, &tok), Held::new(1, 111));
        let mut state = State::new(Url::parse("postgresql:///test").unwrap());
        state.restore_from_snapshot(initial, AddrInterner::new());

        // Block at slot 200: A spends its TOK to B (transfer) and mints NEW to itself.
        let produced = vec![
            ((txh_new.clone(), 0i16), held(&addr_b, &tok, 1)),
            ((txh_new.clone(), 1i16), held(&addr_a, &new, 5)),
        ];
        let consumed = vec![((txh_old.clone(), 0i16), held(&addr_a, &tok, 1))];

        state.apply_block(BlockUpdate {
            slot: 200,
            block_hash: "h2".into(),
            epoch: 1,
            produced,
            consumed: &consumed,
            pool_delegation_changes: &[],
            drep_delegation_changes: &[],
            pool_updates: &[],
            pool_retirements: &[],
            issuer_pool_hash: None,
            stake_changes: &[],
            withdrawal_changes: &[],
            reward_deltas: None,
            drep_active_until: None,
            drep_votes: &std::collections::HashMap::new(),
        });

        let cur = state.current().unwrap();
        // TOK moved to B keeps its original mint slot (111), not the block slot.
        assert_eq!(
            cur.asset_holdings
                .get(&leaf(&addr_b, &tok))
                .unwrap()
                .mint_slot(),
            111
        );
        // A no longer holds TOK.
        assert!(cur.asset_holdings.get(&leaf(&addr_a, &tok)).is_none());
        // NEW was minted this block → stamped with the block's slot (200).
        let new_leaf = cur.asset_holdings.get(&leaf(&addr_a, &new)).unwrap();
        assert_eq!(new_leaf.qty(), 5);
        assert_eq!(new_leaf.mint_slot(), 200);
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
            Arc::new(AddrKey::new(Some(&[0xcc; 28]), &[0x01; 57])),
            asset_id(&[0x11; 28], b"BIG"),
        );
        snap.asset_holdings
            .insert(hkey.clone(), Held::new(big, 12_345_678));
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

        // Correct magic → the entire snapshot round-trips, including the >u64 holding, and the
        // interned key is shared (one Arc for the address, seeded into the returned interner).
        let (loaded, loaded_fi, interner) = State::load_snapshot(&path, magic).unwrap();
        assert!(loaded == snap, "snapshot did not round-trip exactly");
        let loaded_held = loaded.asset_holdings.get(&hkey).unwrap();
        assert_eq!(loaded_held.qty(), big);
        assert_eq!(loaded_held.mint_slot(), 12_345_678);
        assert_eq!(loaded_fi.pool_minted_blocks(&[0x01; 28]).len(), 1);
        assert_eq!(interner.len(), 1, "the one address interned once");

        // A snapshot tagged with a different SNAPSHOT_FORMAT → rejected (forces rebuild). The
        // format is the first value in the file, so a bad one is rejected before the map reads.
        let mut bad = Vec::new();
        rmp_serde::encode::write(&mut bad, &(SNAPSHOT_FORMAT + 1)).unwrap();
        std::fs::write(&path, &bad).unwrap();
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
        let mut interner = AddrInterner::new();
        // addr_x: pa/na x10, pb/nb x5 ; addr_y: pa/na x3 (shared) ; addr_e: pa/na x100 (excluded).
        apply_utxo_assets(
            &addr_x,
            &vec![
                (pa.clone(), vec![(na.clone(), 10)]),
                (pb.clone(), vec![(nb.clone(), 5)]),
            ],
            true,
            &mut snap.asset_holdings,
            &mut interner,
            &std::collections::HashMap::new(),
            0,
        );
        apply_utxo_assets(
            &addr_y,
            &vec![(pa.clone(), vec![(na.clone(), 3)])],
            true,
            &mut snap.asset_holdings,
            &mut interner,
            &std::collections::HashMap::new(),
            0,
        );
        apply_utxo_assets(
            &addr_e,
            &vec![(pa.clone(), vec![(na.clone(), 100)])],
            true,
            &mut snap.asset_holdings,
            &mut interner,
            &std::collections::HashMap::new(),
            0,
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
                (pa.clone(), na.clone(), 13u128, 0),
                (pb.clone(), nb.clone(), 5u128, 0)
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

#[cfg(test)]
mod snapshot_bench {
    /// Ad-hoc loader benchmark against a real snapshot — `SNAPSHOT_BENCH=/path cargo test
    /// --release snapshot_load_timing -- --nocapture --ignored`. Ignored by default: it
    /// needs a multi-GB file and roughly its size again in RAM.
    /// Parse the holdings stream exactly as the real loader does, but throw every entry
    /// away — no interning, no `OrdMap`. The gap to `holdings_ms` is what rebuilding the
    /// structure costs, i.e. the part a different *file format* could not fix.
    struct CountingSeed(usize, usize, usize);
    impl<'de> serde::de::DeserializeSeed<'de> for &mut CountingSeed {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            struct V<'a>(&'a mut CountingSeed);
            impl<'de> serde::de::Visitor<'de> for V<'_> {
                type Value = ();
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("the holdings map")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<(), A::Error> {
                    // The real loader's shape (address → token list) minus interning and the
                    // `OrdMap`. Also counts addresses and (address, policy) runs — the latter
                    // is what a further "group by policy" step would collapse.
                    while let Some((_addr, tokens)) =
                        map.next_entry::<super::AddrKey, Vec<(super::AssetId, super::Held)>>()?
                    {
                        self.0 .0 += tokens.len();
                        self.0 .1 += 1;
                        let mut last: Option<Vec<u8>> = None;
                        for (aid, _) in &tokens {
                            let policy = &aid[..28.min(aid.len())];
                            if last.as_deref() != Some(policy) {
                                self.0 .2 += 1;
                                last = Some(policy.to_vec());
                            }
                        }
                    }
                    Ok(())
                }
            }
            d.deserialize_map(V(self))
        }
    }

    /// A/B the wire shape end to end: read whatever snapshot is on disk, write it back in
    /// the current format, and time loading *that*. With a pre-grouped (format 13) file as
    /// input this measures exactly what grouping bought.
    /// `SNAPSHOT_BENCH=/path cargo test --release snapshot_regroup -- --nocapture --ignored`
    #[test]
    #[ignore]
    fn snapshot_regroup_and_compare() {
        let Ok(path) = std::env::var("SNAPSHOT_BENCH") else {
            return;
        };
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::FmtSubscriber::builder()
                .with_max_level(tracing::Level::INFO)
                .finish(),
        );
        let magic: u64 = 764824073;
        let src = std::path::Path::new(&path);
        let t = std::time::Instant::now();
        let (snap, fi, _) = super::State::load_snapshot(src, magic).expect("load source");
        let before = t.elapsed();
        let before_mb = std::fs::metadata(src).map(|m| m.len() >> 20).unwrap_or(0);

        let out = std::path::Path::new("/tmp/snapshot-regrouped.bin");
        let t = std::time::Instant::now();
        super::write_snapshot(out, &snap, &fi, magic).expect("write");
        let write = t.elapsed();
        drop(snap);

        let t = std::time::Instant::now();
        let (snap2, _, interner2) = super::State::load_snapshot(out, magic).expect("load written");
        let after = t.elapsed();
        let after_mb = std::fs::metadata(out).map(|m| m.len() >> 20).unwrap_or(0);
        println!(
            "source: {before_mb} MB in {before:?} | rewritten: {after_mb} MB in {after:?} \
             (write {write:?}) | holdings {} | addrs {}",
            snap2.asset_holdings.len(),
            interner2.len()
        );
        // Left on disk on purpose: `snapshot_load_timing` can then time it under the same
        // conditions as the source file, which is the only fair A/B.
    }

    /// Decode-only timing: how much of the load is parsing bytes vs building the maps.
    #[test]
    #[ignore]
    fn snapshot_decode_only_timing() {
        use serde::de::DeserializeSeed;
        use serde::Deserialize;
        let Ok(path) = std::env::var("SNAPSHOT_BENCH") else {
            return;
        };
        let file = std::fs::File::open(&path).expect("open");
        let mut de =
            rmp_serde::Deserializer::new(std::io::BufReader::with_capacity(8 * 1024 * 1024, file));
        let t = std::time::Instant::now();
        let _format = u32::deserialize(&mut de).unwrap();
        let _magic = u64::deserialize(&mut de).unwrap();
        let snap = super::BlockSnapshot::deserialize(&mut de).unwrap();
        let fields = t.elapsed();
        let t = std::time::Instant::now();
        let mut counter = CountingSeed(0, 0, 0);
        (&mut counter).deserialize(&mut de).unwrap();
        println!(
            "decode-only: fields (with maps) {:?} | holdings parse, no map {:?} | entries {} \
             | addresses {} | (addr,policy) groups {} | balances {}",
            fields,
            t.elapsed(),
            counter.0,
            counter.1,
            counter.2,
            snap.address_balances.len()
        );
    }

    #[test]
    #[ignore]
    fn snapshot_load_timing() {
        let Ok(path) = std::env::var("SNAPSHOT_BENCH") else {
            return;
        };
        let magic: u64 = std::env::var("SNAPSHOT_MAGIC")
            .ok()
            .and_then(|m| m.parse().ok())
            .unwrap_or(764824073);
        // The section timings are logged by `load_snapshot`; give them somewhere to go.
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::FmtSubscriber::builder()
                .with_max_level(tracing::Level::INFO)
                .finish(),
        );
        let t = std::time::Instant::now();
        let loaded = super::State::load_snapshot(std::path::Path::new(&path), magic);
        let (snap, _, interner) = loaded.expect("snapshot should load");
        println!(
            "total {:?} | holdings {} | addrs {} | balances {} | stakes {}",
            t.elapsed(),
            snap.asset_holdings.len(),
            interner.len(),
            snap.address_balances.len(),
            snap.stakes.len()
        );
    }
}
