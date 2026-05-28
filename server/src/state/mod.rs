mod dbsync;
pub mod feed_index;

use imbl::{hashmap::HashMap, hashset::HashSet};
use std::path::Path;
use url::Url;

use crate::cip26;
use crate::cip68;
use crate::model::{parse_virtual_handle_address, DRep, Pool, TxOutput, HANDLE_POLICIES};
use crate::pallas::PoolUpdate;
use dbsync::DbSync;
pub use feed_index::FeedIndex;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
}

impl BlockSnapshot {
    /// Look up the shortest ADA Handle for an address, if any.
    pub fn handle_for(&self, address: &str) -> Option<String> {
        self.handle_by_address
            .get(address)
            .and_then(|handles| handles.iter().min_by_key(|h| h.len()).map(|h| h.clone()))
    }
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

    pub fn current(&self) -> Option<&BlockSnapshot> {
        self.history.last()
    }

    pub fn current_mut(&mut self) -> Option<&mut BlockSnapshot> {
        self.history.last_mut()
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
            dreps,
            stakes,
            rewards,
            decimals,
            handle_by_address,
            address_by_handle,
            gov_action_titles,
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
    pub fn apply_block(
        &mut self,
        slot: u64,
        block_hash: String,
        produced: Vec<((Vec<u8>, i16), TxOutput)>,
        consumed: &[(Vec<u8>, i16)],
        pool_delegation_changes: &[(Vec<u8>, Option<Vec<u8>>)],
        drep_delegation_changes: &[(Vec<u8>, Option<Vec<u8>>)],
        pool_updates: &[PoolUpdate],
        stake_changes: &[(Vec<u8>, i64)],
        withdrawal_changes: &[(Vec<u8>, i64)],
        epoch: u64,
        reward_deltas: Option<&HashMap<Vec<u8>, i64>>,
    ) {
        let prev = self.history.last().expect("state not initialized");

        let mut utxos = prev.utxos.clone();
        for key in consumed {
            utxos.remove(key);
        }
        for (key, output) in produced {
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
            handle_by_address,
            address_by_handle,
            gov_action_titles,
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

    /// Batch-resolve input addresses, lovelace, and asset fingerprints.
    /// Checks in-memory UTXOs first, falls back to db-sync for the rest.
    /// Returns (resolved_map, unspent_utxos_to_cache).
    pub async fn resolve_utxos_batch(
        &self,
        inputs: &[(Vec<u8>, i16)],
    ) -> (
        std::collections::HashMap<(Vec<u8>, i16), (String, u64, Vec<(String, u64)>)>,
        Vec<((Vec<u8>, i16), TxOutput)>,
    ) {
        let mut result = std::collections::HashMap::<
            (Vec<u8>, i16),
            (String, u64, Vec<(String, u64)>),
        >::with_capacity(inputs.len());
        let mut remaining = Vec::new();

        if let Some(snap) = self.current() {
            for (hash, index) in inputs {
                if let Some(utxo) = snap.utxos.get(&(hash.clone(), *index)) {
                    let addr = pallas::ledger::addresses::Address::from_bytes(&utxo.address)
                        .ok()
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    let lovelace: u64 = utxo
                        .lovelaces
                        .try_into()
                        .expect("lovelace value must fit u64");
                    result.insert(
                        (hash.clone(), *index),
                        (addr, lovelace, utxo.assets.clone()),
                    );
                } else {
                    remaining.push((hash.clone(), *index));
                }
            }
        } else {
            remaining.extend_from_slice(inputs);
        }

        let mut to_cache = Vec::new();

        if !remaining.is_empty() {
            if let Some(db) = self.db().await {
                if let Ok(db_result) = db.resolve_utxos_batch(&remaining).await {
                    for (key, (addr, lovelace, assets, unspent)) in db_result {
                        if unspent {
                            let address_bytes =
                                pallas::ledger::addresses::Address::from_bech32(&addr)
                                    .ok()
                                    .map(|a| a.to_vec())
                                    .unwrap_or_default();
                            to_cache.push((
                                key.clone(),
                                TxOutput {
                                    lovelaces: rust_decimal::Decimal::from(lovelace),
                                    address: address_bytes,
                                    assets: assets.clone(),
                                },
                            ));
                        }
                        result.insert(key, (addr, lovelace, assets));
                    }
                }
            }
        }

        (result, to_cache)
    }

    /// Fetch epoch reward deltas from db-sync for a new epoch.
    pub async fn epoch_reward_delta(&self, epoch: u64) -> Option<HashMap<Vec<u8>, i64>> {
        let db = self.db().await?;
        db.epoch_reward_delta(epoch).await.ok()
    }

    /// Fetch a page of a policy's assets (newest-first-minted) from db-sync.
    /// `cursor` is the last id of the previous page (None for the first page).
    pub async fn assets_by_policy(
        &self,
        policy: &[u8],
        cursor: Option<i64>,
        limit: i64,
    ) -> Option<Vec<(i64, String, Vec<u8>)>> {
        let db = self.db().await?;
        db.assets_by_policy(policy, cursor, limit).await.ok()
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
}
