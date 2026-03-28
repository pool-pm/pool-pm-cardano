use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Serialize, Deserialize)]
pub struct BlockRef {
    pub slot: u64,
    pub hash: String,
    pub number: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DelegationTarget {
    pub raw: Vec<u8>,
    pub id: String,
    pub label: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DelegationEntry {
    pub slot: u64,
    pub block_hash: String,
    pub block_no: u64,
    pub block_pool_hash: Vec<u8>,
    pub block_pool_ticker: Option<String>,
    pub tx_hash: String,
    pub stake_address: String,
    pub stake_cred: Vec<u8>,
    pub live_stake: i64,
    pub from: Option<DelegationTarget>,
    pub to: Option<DelegationTarget>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct FeedIndex {
    pool_minted: HashMap<Vec<u8>, Vec<BlockRef>>,
    pool_stake_change: HashMap<Vec<u8>, Vec<BlockRef>>,
    delegation_events: Vec<DelegationEntry>,
    pool_delegation_index: HashMap<Vec<u8>, Vec<usize>>,
}

impl FeedIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_pool_minted(&mut self, pool_hash: Vec<u8>, block_ref: BlockRef) {
        self.pool_minted
            .entry(pool_hash)
            .or_default()
            .push(block_ref);
    }

    pub fn add_pool_stake_changes(&mut self, pools: HashSet<Vec<u8>>, block_ref: BlockRef) {
        for pool_hash in pools {
            self.pool_stake_change
                .entry(pool_hash)
                .or_default()
                .push(block_ref.clone());
        }
    }

    pub fn add_delegation_event(&mut self, entry: DelegationEntry) {
        let idx = self.delegation_events.len();
        if let Some(ref from) = entry.from {
            self.pool_delegation_index
                .entry(from.raw.clone())
                .or_default()
                .push(idx);
        }
        if let Some(ref to) = entry.to {
            self.pool_delegation_index
                .entry(to.raw.clone())
                .or_default()
                .push(idx);
        }
        self.delegation_events.push(entry);
    }

    pub fn pool_minted_blocks(&self, pool_hash: &[u8]) -> &[BlockRef] {
        self.pool_minted
            .get(pool_hash)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn pool_stake_change_blocks(&self, pool_hash: &[u8]) -> &[BlockRef] {
        self.pool_stake_change
            .get(pool_hash)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn pool_delegation_entries(&self, pool_hash: &[u8]) -> Vec<&DelegationEntry> {
        self.pool_delegation_index
            .get(pool_hash)
            .map(|indices| {
                indices
                    .iter()
                    .map(|&i| &self.delegation_events[i])
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn rollback(&mut self, slot: u64) {
        for entries in self.pool_minted.values_mut() {
            let keep = entries.partition_point(|r| r.slot <= slot);
            entries.truncate(keep);
        }
        self.pool_minted.retain(|_, v| !v.is_empty());

        for entries in self.pool_stake_change.values_mut() {
            let keep = entries.partition_point(|r| r.slot <= slot);
            entries.truncate(keep);
        }
        self.pool_stake_change.retain(|_, v| !v.is_empty());

        let keep = self.delegation_events.partition_point(|e| e.slot <= slot);
        self.delegation_events.truncate(keep);
        self.rebuild_delegation_index();
    }

    pub fn prune(&mut self, boundary_slot: u64) {
        for entries in self.pool_minted.values_mut() {
            let start = entries.partition_point(|r| r.slot < boundary_slot);
            if start > 0 {
                entries.drain(..start);
            }
        }
        self.pool_minted.retain(|_, v| !v.is_empty());

        for entries in self.pool_stake_change.values_mut() {
            let start = entries.partition_point(|r| r.slot < boundary_slot);
            if start > 0 {
                entries.drain(..start);
            }
        }
        self.pool_stake_change.retain(|_, v| !v.is_empty());

        let start = self
            .delegation_events
            .partition_point(|e| e.slot < boundary_slot);
        if start > 0 {
            self.delegation_events.drain(..start);
            self.rebuild_delegation_index();
        }
    }

    fn rebuild_delegation_index(&mut self) {
        self.pool_delegation_index.clear();
        for (idx, entry) in self.delegation_events.iter().enumerate() {
            if let Some(ref from) = entry.from {
                self.pool_delegation_index
                    .entry(from.raw.clone())
                    .or_default()
                    .push(idx);
            }
            if let Some(ref to) = entry.to {
                self.pool_delegation_index
                    .entry(to.raw.clone())
                    .or_default()
                    .push(idx);
            }
        }
    }
}
