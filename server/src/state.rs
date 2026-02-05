use im::{hashmap::HashMap, hashset::HashSet};
use sqlx::types::Decimal;

use crate::model::{Pool, TxOutput};

pub struct BlockSnapshot {
    pub height: u64,
    pub slot: u64,
    pub utxos: HashMap<(Vec<u8>, i16), TxOutput>,
    pub pools: HashMap<String, Pool>,
    pub delegations: HashMap<Vec<u8>, Vec<u8>>,
    pub delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    pub stakes: HashMap<Vec<u8>, Decimal>,
}

pub struct State {
    history: Vec<BlockSnapshot>,
}

impl State {
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
        }
    }

    pub fn current(&self) -> Option<&BlockSnapshot> {
        self.history.last()
    }

    /// Initialize state from db-sync data at a given reset point.
    /// Replaces all history with a single snapshot.
    pub fn reset(
        &mut self,
        slot: u64,
        height: u64,
        utxos: HashMap<(Vec<u8>, i16), TxOutput>,
        pools: HashMap<String, Pool>,
        delegations: HashMap<Vec<u8>, Vec<u8>>,
        delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
        stakes: HashMap<Vec<u8>, Decimal>,
    ) {
        self.history.clear();
        self.history.push(BlockSnapshot {
            height,
            slot,
            utxos,
            pools,
            delegations,
            delegators,
            stakes,
        });
    }

    /// Apply a new block: clone current snapshot (O(1) structural sharing),
    /// apply UTXO changes, and push to history.
    pub fn apply_block(
        &mut self,
        slot: u64,
        height: u64,
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
            height,
            slot,
            utxos,
            pools: prev.pools.clone(),
            delegations: prev.delegations.clone(),
            delegators: prev.delegators.clone(),
            stakes: prev.stakes.clone(),
        });
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

    /// Look up a UTXO by (tx_hash, output_index) in the current snapshot.
    pub fn resolve_input(&self, tx_hash: &[u8], index: i16) -> Option<&TxOutput> {
        self.current()
            .and_then(|s| s.utxos.get(&(tx_hash.to_vec(), index)))
    }
}
