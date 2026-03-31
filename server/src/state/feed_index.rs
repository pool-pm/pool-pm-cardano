use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Serialize, Deserialize)]
pub struct BlockRef {
    pub slot: u64,
    pub hash: String,
    pub number: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DelegationEntry {
    pub slot: u64,
    pub block_hash: String,
    pub block_no: u64,
    pub tx_hash: String,
    /// Raw stake credential (28 bytes).
    pub cred: Vec<u8>,
    pub live_stake: i64,
    /// Raw target bytes: pool hash (28 bytes) or DRep bytes (tag + hash).
    pub from: Option<Vec<u8>>,
    pub to: Option<Vec<u8>>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct FeedIndex {
    pool_minted: HashMap<Vec<u8>, Vec<BlockRef>>,
    pool_stake_change: HashMap<Vec<u8>, Vec<BlockRef>>,
    delegation_events: Vec<DelegationEntry>,
    pool_delegation_index: HashMap<Vec<u8>, Vec<usize>>,
    drep_stake_change: HashMap<Vec<u8>, Vec<BlockRef>>,
    drep_delegation_events: Vec<DelegationEntry>,
    drep_delegation_index: HashMap<Vec<u8>, Vec<usize>>,
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
                .entry(from.clone())
                .or_default()
                .push(idx);
        }
        if let Some(ref to) = entry.to {
            self.pool_delegation_index
                .entry(to.clone())
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

    pub fn add_drep_stake_changes(&mut self, dreps: HashSet<Vec<u8>>, block_ref: BlockRef) {
        for drep_bytes in dreps {
            self.drep_stake_change
                .entry(drep_bytes)
                .or_default()
                .push(block_ref.clone());
        }
    }

    pub fn add_drep_delegation_event(&mut self, entry: DelegationEntry) {
        let idx = self.drep_delegation_events.len();
        if let Some(ref from) = entry.from {
            self.drep_delegation_index
                .entry(from.clone())
                .or_default()
                .push(idx);
        }
        if let Some(ref to) = entry.to {
            self.drep_delegation_index
                .entry(to.clone())
                .or_default()
                .push(idx);
        }
        self.drep_delegation_events.push(entry);
    }

    pub fn drep_stake_change_blocks(&self, drep_bytes: &[u8]) -> &[BlockRef] {
        self.drep_stake_change
            .get(drep_bytes)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn drep_delegation_entries(&self, drep_bytes: &[u8]) -> Vec<&DelegationEntry> {
        self.drep_delegation_index
            .get(drep_bytes)
            .map(|indices| {
                indices
                    .iter()
                    .map(|&i| &self.drep_delegation_events[i])
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

        for entries in self.drep_stake_change.values_mut() {
            let keep = entries.partition_point(|r| r.slot <= slot);
            entries.truncate(keep);
        }
        self.drep_stake_change.retain(|_, v| !v.is_empty());

        let keep = self
            .drep_delegation_events
            .partition_point(|e| e.slot <= slot);
        self.drep_delegation_events.truncate(keep);
        self.rebuild_drep_delegation_index();
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

        for entries in self.drep_stake_change.values_mut() {
            let start = entries.partition_point(|r| r.slot < boundary_slot);
            if start > 0 {
                entries.drain(..start);
            }
        }
        self.drep_stake_change.retain(|_, v| !v.is_empty());

        let start = self
            .drep_delegation_events
            .partition_point(|e| e.slot < boundary_slot);
        if start > 0 {
            self.drep_delegation_events.drain(..start);
            self.rebuild_drep_delegation_index();
        }
    }

    fn rebuild_delegation_index(&mut self) {
        self.pool_delegation_index.clear();
        for (idx, entry) in self.delegation_events.iter().enumerate() {
            if let Some(ref from) = entry.from {
                self.pool_delegation_index
                    .entry(from.clone())
                    .or_default()
                    .push(idx);
            }
            if let Some(ref to) = entry.to {
                self.pool_delegation_index
                    .entry(to.clone())
                    .or_default()
                    .push(idx);
            }
        }
    }

    fn rebuild_drep_delegation_index(&mut self) {
        self.drep_delegation_index.clear();
        for (idx, entry) in self.drep_delegation_events.iter().enumerate() {
            if let Some(ref from) = entry.from {
                self.drep_delegation_index
                    .entry(from.clone())
                    .or_default()
                    .push(idx);
            }
            if let Some(ref to) = entry.to {
                self.drep_delegation_index
                    .entry(to.clone())
                    .or_default()
                    .push(idx);
            }
        }
    }
}
