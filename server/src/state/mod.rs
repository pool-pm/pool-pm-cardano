mod dbsync;

use imbl::{hashmap::HashMap, hashset::HashSet};
use std::path::Path;
use url::Url;

use crate::model::{Pool, TxOutput};
use crate::pallas::PoolUpdate;
use dbsync::DbSync;

#[derive(serde::Serialize, serde::Deserialize)]
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
}

pub struct State {
    history: Vec<BlockSnapshot>,
    db_url: Url,
    db: tokio::sync::OnceCell<DbSync>,
}

impl State {
    pub fn new(db_url: Url) -> Self {
        Self {
            history: Vec::new(),
            db_url,
            db: tokio::sync::OnceCell::new(),
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

    /// Initialize state from db-sync data at a given reset point.
    /// Fetches pools, delegations, stakes, and rewards from db-sync,
    /// replaces all history with a single snapshot.
    pub async fn reset(&mut self, slot: u64, genesis: &oura::framework::GenesisValues) -> Result<(), sqlx::Error> {
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
            stakes,
            rewards,
        });

        Ok(())
    }

    pub fn epoch_for_slot(slot: u64, genesis: &oura::framework::GenesisValues) -> u64 {
        // shelley_known_slot is in Byron slot numbering; byron_epoch_length is in seconds
        let shelley_start_epoch = genesis.shelley_known_slot
            * genesis.byron_slot_length as u64
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
            stakes,
            rewards,
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

    /// Fetch epoch reward deltas from db-sync for a new epoch.
    pub async fn epoch_reward_delta(&self, epoch: u64) -> Option<HashMap<Vec<u8>, i64>> {
        let db = self.db().await?;
        db.epoch_reward_delta(epoch).await.ok()
    }

    /// Batch-resolve input addresses and lovelace from db-sync.
    pub async fn resolve_utxos_batch(
        &self,
        inputs: &[(Vec<u8>, i16)],
    ) -> std::collections::HashMap<(Vec<u8>, i16), (String, u64)> {
        if let Some(db) = self.db().await {
            db.resolve_utxos_batch(inputs)
                .await
                .unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        }
    }

    /// All blocks minted by a pool since a slot boundary.
    pub async fn pool_blocks_since(&self, pool_hash: &[u8], boundary_slot: u64) -> Vec<(u64, String, u64)> {
        if let Some(db) = self.db().await {
            db.pool_blocks_since(pool_hash, boundary_slot as i64)
                .await
                .unwrap_or_default()
        } else {
            vec![]
        }
    }

    /// Distinct blocks containing delegation changes TO a pool since a slot.
    pub async fn pool_delegation_blocks_since(
        &self,
        pool_hash: &[u8],
        boundary_slot: u64,
    ) -> Vec<(u64, String, u64, Option<Vec<u8>>, Option<String>)> {
        if let Some(db) = self.db().await {
            db.pool_delegation_blocks_since(pool_hash, boundary_slot as i64)
                .await
                .unwrap_or_default()
        } else {
            vec![]
        }
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
        !self.history.is_empty()
    }

    /// Restore state from a previously saved snapshot.
    pub fn restore_from_snapshot(&mut self, snapshot: BlockSnapshot) {
        self.history.clear();
        self.history.push(snapshot);
    }

    /// Save a snapshot to disk. Picks the snapshot `depth` blocks back from tip.
    /// Writes atomically via tmp file + rename.
    pub fn save_snapshot(&self, path: &Path, depth: usize) -> Result<u64, Box<dyn std::error::Error>> {
        let idx = self.history.len().saturating_sub(depth);
        let snap = &self.history[idx];
        let data = rmp_serde::to_vec(snap)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, path)?;
        Ok(snap.slot)
    }

    /// Load a snapshot from disk. Returns None on any error.
    pub fn load_snapshot(path: &Path) -> Option<BlockSnapshot> {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!("failed to read snapshot from {}: {}", path.display(), e);
                return None;
            }
        };
        tracing::info!("loading snapshot from {}...", path.display());
        match rmp_serde::from_slice(&data) {
            Ok(snap) => Some(snap),
            Err(e) => {
                tracing::warn!("failed to deserialize snapshot: {}", e);
                None
            }
        }
    }

    /// Resolve an input by (tx_hash, output_index): check in-memory UTXOs first,
    /// then fall back to db-sync. Returns (address, lovelace).
    pub async fn resolve_input(&self, tx_hash: &[u8], index: i16) -> (Option<String>, u64) {
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
            );
        }
        if let Some(db) = self.db().await {
            if let Ok(Some((address, value))) = db.resolve_utxo(tx_hash, index).await {
                return (
                    Some(address),
                    value.try_into().expect("lovelace value must fit u64"),
                );
            }
        }
        (None, 0)
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
