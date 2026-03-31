use imbl::HashSet;
use pallas::ledger::addresses::{Address, ShelleyDelegationPart};

use crate::event::{BlockTx, Event};
use crate::model::{drep_bech32_id, pool_bech32_id};
use crate::state::BlockSnapshot;

#[derive(Clone)]
pub enum FeedFilter {
    Pool(Vec<u8>),
    DRep(Vec<u8>),
}

impl FeedFilter {
    pub fn from_path(id: &str) -> Option<Self> {
        // Special DReps (no bech32 encoding)
        match id {
            "drep_always_abstain" => return Some(FeedFilter::DRep(vec![0x02])),
            "drep_always_no_confidence" => return Some(FeedFilter::DRep(vec![0x03])),
            _ => {}
        }
        let (hrp, data) = bech32::decode(id).ok()?;
        match hrp.as_str() {
            "pool" if data.len() == 28 => Some(FeedFilter::Pool(data)),
            "drep" if data.len() == 28 => Some(FeedFilter::DRep([&[0x00u8][..], &data].concat())),
            "drep" if data.len() == 29 => {
                // CIP-129: first byte is credential type (0x22=key, 0x23=script)
                let tag = if data[0] == 0x23 { 0x01u8 } else { 0x00 };
                Some(FeedFilter::DRep([&[tag][..], &data[1..]].concat()))
            }
            "drep_script" if data.len() == 28 => {
                Some(FeedFilter::DRep([&[0x01u8][..], &data].concat()))
            }
            _ => None,
        }
    }

    pub fn feed_id(&self) -> String {
        match self {
            FeedFilter::Pool(hash) => pool_bech32_id(hash),
            FeedFilter::DRep(bytes) => drep_bech32_id(bytes),
        }
    }

    pub fn drep_bytes(&self) -> Option<&Vec<u8>> {
        match self {
            FeedFilter::DRep(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub fn delegators<'a>(&self, snap: &'a BlockSnapshot) -> Option<&'a HashSet<Vec<u8>>> {
        match self {
            FeedFilter::Pool(hash) => snap.pool_delegators.get(hash),
            FeedFilter::DRep(bytes) => snap.drep_delegators.get(bytes),
        }
    }
}

pub fn extract_stake_credentials(tx: &BlockTx) -> Vec<Vec<u8>> {
    let mut creds = Vec::new();
    for input in &tx.inputs {
        if let Some(ref addr) = input.address {
            if let Some(cred) = stake_credential(addr) {
                creds.push(cred);
            }
        }
    }
    for output in &tx.outputs {
        if let Some(cred) = stake_credential(&output.address) {
            creds.push(cred);
        }
    }
    creds
}

impl FeedFilter {
    fn matches_tx(&self, tx: &BlockTx, delegators: &HashSet<Vec<u8>>) -> bool {
        tx.stake_credentials
            .iter()
            .any(|cred| delegators.contains(cred))
    }

    fn matches_block(&self, pool_id: &Option<String>) -> bool {
        if let FeedFilter::Pool(pool_hash) = self {
            if let Some(id) = pool_id {
                if let Ok((_, data)) = bech32::decode(id) {
                    return data == *pool_hash;
                }
            }
        }
        false
    }

    pub fn filter_event(&self, event: &Event, delegators: &HashSet<Vec<u8>>) -> Option<Event> {
        match event {
            Event::MempoolTx(tx) => {
                if self.matches_tx(tx, delegators) {
                    let mut tx = tx.clone();
                    apply_stake_change(&mut tx, delegators, self);
                    Some(Event::MempoolTx(tx))
                } else {
                    None
                }
            }
            Event::Block {
                slot,
                hash,
                number,
                timestamp,
                pool_id,
                pool_ticker,
                txs,
            } => {
                // Block minted by this pool — include with all txs
                if self.matches_block(pool_id) {
                    return Some(event.clone());
                }

                let mut filtered: Vec<BlockTx> = txs
                    .iter()
                    .filter(|tx| self.matches_tx(tx, delegators))
                    .cloned()
                    .collect();
                if filtered.is_empty() {
                    None
                } else {
                    apply_stake_changes(&mut filtered, delegators, self);
                    Some(Event::Block {
                        slot: *slot,
                        hash: hash.clone(),
                        number: *number,
                        timestamp: *timestamp,
                        pool_id: pool_id.clone(),
                        pool_ticker: pool_ticker.clone(),
                        txs: filtered,
                    })
                }
            }
            Event::Rollback { .. } | Event::MempoolPrune { .. } => Some(event.clone()),
        }
    }
}

/// Compute net stake change for a single tx relative to feed delegators.
/// Combines UTXO changes (outputs - inputs - withdrawals) with delegation
/// impact (live_stake gained/lost from delegation certificate changes).
pub fn apply_stake_change(tx: &mut BlockTx, delegators: &HashSet<Vec<u8>>, filter: &FeedFilter) {
    let mut net: i64 = 0;
    for output in &tx.outputs {
        if let Some(cred) = stake_credential(&output.address) {
            if delegators.contains(&cred) {
                net += output.lovelace as i64;
            }
        }
    }
    for input in &tx.inputs {
        if let Some(ref addr) = input.address {
            if let Some(cred) = stake_credential(addr) {
                if delegators.contains(&cred) {
                    net -= input.lovelace as i64;
                }
            }
        }
    }
    for (cred, amount) in &tx.withdrawals {
        if delegators.contains(cred) {
            net -= *amount as i64;
        }
    }
    // Add delegation impact: live_stake gained/lost from delegation certificates
    let feed_id = filter.feed_id();
    for deleg in &tx.delegations {
        let (to_id, from_id) = match filter {
            FeedFilter::Pool(_) => (deleg.to_pool_id.as_deref(), deleg.from_pool_id.as_deref()),
            FeedFilter::DRep(_) => (deleg.to_drep_id.as_deref(), deleg.from_drep_id.as_deref()),
        };
        if to_id == Some(feed_id.as_str()) && from_id != Some(feed_id.as_str()) {
            net += deleg.live_stake;
        } else if from_id == Some(feed_id.as_str()) && to_id != Some(feed_id.as_str()) {
            net -= deleg.live_stake;
        }
    }
    if net != 0 {
        tx.stake_change = Some(net);
    }
}

/// Apply stake change to multiple txs.
pub fn apply_stake_changes(
    txs: &mut [BlockTx],
    delegators: &HashSet<Vec<u8>>,
    filter: &FeedFilter,
) {
    for tx in txs {
        apply_stake_change(tx, delegators, filter);
    }
}

pub fn stake_credential(addr: &str) -> Option<Vec<u8>> {
    let addr = Address::from_bech32(addr).ok()?;
    match addr {
        Address::Shelley(shelley) => match shelley.delegation() {
            ShelleyDelegationPart::Key(h) | ShelleyDelegationPart::Script(h) => {
                Some(h.as_ref().to_vec())
            }
            _ => None,
        },
        _ => None,
    }
}
