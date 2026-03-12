mod dbsync;

use imbl::{hashmap::HashMap, hashset::HashSet};
use url::Url;

use crate::model::{Pool, TxOutput};
use crate::pallas::PoolUpdate;
use dbsync::DbSync;

pub struct BlockSnapshot {
    pub slot: u64,
    pub utxos: HashMap<(Vec<u8>, i16), TxOutput>,
    pub pools: HashMap<String, Pool>,
    pub pool_delegations: HashMap<Vec<u8>, Vec<u8>>,
    pub pool_delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    pub drep_delegations: HashMap<Vec<u8>, Vec<u8>>,
    pub drep_delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
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
    /// Fetches pools and delegations from db-sync, replaces all history
    /// with a single snapshot.
    pub async fn reset(&mut self, slot: u64) -> Result<(), sqlx::Error> {
        let db = self
            .db
            .get_or_try_init(|| async { DbSync::new(&self.db_url).await })
            .await?;

        let last_tx_id = db.last_slot_tx_id(slot).await?;

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

        self.history.clear();
        self.history.push(BlockSnapshot {
            slot,
            utxos: HashMap::new(),
            pools,
            pool_delegations,
            pool_delegators,
            drep_delegations,
            drep_delegators,
        });

        Ok(())
    }

    /// Apply a new block: clone current snapshot (O(1) structural sharing),
    /// apply UTXO changes, and push to history.
    pub fn apply_block(
        &mut self,
        slot: u64,
        produced: Vec<((Vec<u8>, i16), TxOutput)>,
        consumed: &[(Vec<u8>, i16)],
        pool_delegation_changes: &[(Vec<u8>, Option<Vec<u8>>)],
        drep_delegation_changes: &[(Vec<u8>, Option<Vec<u8>>)],
        pool_updates: &[PoolUpdate],
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

        self.history.push(BlockSnapshot {
            slot,
            utxos,
            pools,
            pool_delegations,
            pool_delegators,
            drep_delegations,
            drep_delegators,
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

    /// Rollback to the given slot: drop all snapshots after it.
    pub fn rollback(&mut self, slot: u64) {
        let keep = self
            .history
            .iter()
            .rposition(|s| s.slot <= slot)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.history.truncate(keep);
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
