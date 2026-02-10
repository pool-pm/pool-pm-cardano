mod dbsync;

use imbl::{hashmap::HashMap, hashset::HashSet};
use sqlx::types::Decimal;
use url::Url;

use crate::model::{Pool, TxOutput};
use dbsync::DbSync;

pub struct BlockSnapshot {
    pub slot: u64,
    pub utxos: HashMap<(Vec<u8>, i16), TxOutput>,
    pub pools: HashMap<String, Pool>,
    pub delegations: HashMap<Vec<u8>, Vec<u8>>,
    pub delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    pub stakes: HashMap<Vec<u8>, Decimal>,
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

        tracing::info!("Fetching delegations...");
        let (delegations, delegators) = db.delegations(last_tx_id).await?;
        tracing::info!(
            "{} delegations in {} pools retrieved",
            delegations.len(),
            delegators.len()
        );

        self.history.clear();
        self.history.push(BlockSnapshot {
            slot,
            utxos: HashMap::new(),
            pools,
            delegations,
            delegators,
            stakes: HashMap::new(),
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
    ) {
        let prev = self.history.last().expect("state not initialized");

        let mut utxos = prev.utxos.clone();
        for key in consumed {
            utxos.remove(key);
        }
        for (key, output) in produced {
            utxos.insert(key, output);
        }

        self.history.push(BlockSnapshot {
            slot,
            utxos,
            pools: prev.pools.clone(),
            delegations: prev.delegations.clone(),
            delegators: prev.delegators.clone(),
            stakes: prev.stakes.clone(),
        });

        const MAX_HISTORY: usize = 2160;
        if self.history.len() > MAX_HISTORY {
            self.history.drain(..self.history.len() - MAX_HISTORY);
        }
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
                utxo.lovelaces.try_into().ok().unwrap_or(0),
            );
        }
        match self.db().await {
            Some(db) => match db.resolve_utxo(tx_hash, index).await {
                Ok(Some((address, value))) => {
                    return (Some(address), value.try_into().ok().unwrap_or(0));
                }
                Ok(None) => {
                    tracing::warn!(
                        tx_hash = hex::encode(tx_hash),
                        index,
                        "UTXO not found in memory or db-sync"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        tx_hash = hex::encode(tx_hash),
                        index,
                        error = %e,
                        "db-sync query failed"
                    );
                }
            },
            None => {
                tracing::warn!("db connection unavailable for UTXO resolution");
            }
        }
        (None, 0)
    }
}
