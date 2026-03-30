use imbl::HashSet;
use pallas::ledger::addresses::{Address, ShelleyDelegationPart};

use crate::event::{BlockTx, Event};
use crate::state::BlockSnapshot;

#[derive(Clone)]
pub enum FeedFilter {
    Pool(Vec<u8>),
    DRep(Vec<u8>),
}

impl FeedFilter {
    pub fn from_path(id: &str) -> Option<Self> {
        let (hrp, data) = bech32::decode(id).ok()?;
        if data.len() != 28 {
            return None;
        }
        match hrp.as_str() {
            "pool" => Some(FeedFilter::Pool(data)),
            "drep" => Some(FeedFilter::DRep([&[0x00u8][..], &data].concat())),
            "drep_script" => Some(FeedFilter::DRep([&[0x01u8][..], &data].concat())),
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
                    apply_stake_change(&mut tx, delegators);
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
                    apply_stake_changes(&mut filtered, delegators);
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

/// Compute net stake change for a single tx relative to pool delegators.
/// Withdrawals from delegator stake addresses are subtracted because
/// rewards are already counted as pool stake — converting them to UTXOs
/// doesn't add new stake.
pub fn apply_stake_change(tx: &mut BlockTx, delegators: &HashSet<Vec<u8>>) {
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
    if net != 0 {
        tx.stake_change = Some(net);
    }
}

/// Compute net stake change per tx for pool delegators (outputs - inputs).
pub fn apply_stake_changes(txs: &mut [BlockTx], delegators: &HashSet<Vec<u8>>) {
    for tx in txs {
        apply_stake_change(tx, delegators);
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
