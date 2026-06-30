use imbl::HashSet;
use pallas::ledger::addresses::{Address, ShelleyDelegationPart};

use crate::event::{BlockTx, Event};
use crate::model::{drep_bech32_id, pool_bech32_id};
use crate::state::BlockSnapshot;

#[derive(Clone)]
pub enum FeedFilter {
    Pool(Vec<u8>),
    DRep(Vec<u8>),
    /// Full 29-byte reward-address payload (network/type header + 28-byte
    /// credential). The credential is `payload[1..]`; the 29 bytes double as
    /// db-sync `stake_address.hash_raw`.
    Stake(Vec<u8>),
    /// A specific payment address (bech32 `addr1…`), matched exactly against tx
    /// input/output addresses.
    Address(String),
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
            // Reward address: 1 header byte + 28-byte credential
            "stake" | "stake_test" if data.len() == 29 => Some(FeedFilter::Stake(data)),
            // Payment address: matched exactly by its bech32 string
            "addr" | "addr_test" => Some(FeedFilter::Address(id.to_string())),
            _ => None,
        }
    }

    pub fn feed_id(&self) -> String {
        match self {
            FeedFilter::Pool(hash) => pool_bech32_id(hash),
            FeedFilter::DRep(bytes) => drep_bech32_id(bytes),
            FeedFilter::Stake(payload) => {
                // Reward-address header low nibble is the network id (1 = mainnet).
                let hrp = if payload.first().is_some_and(|b| b & 0x0f == 1) {
                    "stake"
                } else {
                    "stake_test"
                };
                bech32::encode::<bech32::Bech32>(bech32::Hrp::parse(hrp).unwrap(), payload)
                    .unwrap_or_default()
            }
            FeedFilter::Address(addr) => addr.clone(),
        }
    }

    pub fn drep_bytes(&self) -> Option<&Vec<u8>> {
        match self {
            FeedFilter::DRep(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The full 29-byte reward-address payload for a stake feed (also db-sync
    /// `stake_address.hash_raw`); `None` for pool/drep feeds.
    pub fn stake_payload(&self) -> Option<&Vec<u8>> {
        match self {
            FeedFilter::Stake(payload) => Some(payload),
            _ => None,
        }
    }

    /// The bech32 payment address for an address feed; `None` otherwise.
    pub fn address(&self) -> Option<&str> {
        match self {
            FeedFilter::Address(addr) => Some(addr),
            _ => None,
        }
    }

    /// The set of stake credentials whose transactions belong to this feed,
    /// resolved against the current snapshot. Pool/drep feeds use their delegator
    /// set; a stake feed is just its own single credential.
    pub fn current_delegators(&self, snap: &BlockSnapshot) -> HashSet<Vec<u8>> {
        match self {
            FeedFilter::Pool(hash) => snap.pool_delegators.get(hash).cloned().unwrap_or_default(),
            FeedFilter::DRep(bytes) => snap.drep_delegators.get(bytes).cloned().unwrap_or_default(),
            FeedFilter::Stake(payload) => HashSet::unit(payload[1..].to_vec()),
            // Address feeds match by exact address, not a credential set.
            FeedFilter::Address(_) => HashSet::new(),
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
    // A withdrawal is a tx input from the reward account: count its stake credential so the
    // tx matches the stake's own feed and the feeds of any pool/drep it delegates to
    // (`stake_credential` can't resolve a reward `stake1…` address, so the synthetic
    // withdrawal input added in `extract_tx` wouldn't otherwise contribute one). Zero-amount
    // withdrawals are kept too — they're a legitimate script-validation pattern.
    for (cred, _amount) in &tx.withdrawals {
        creds.push(cred.clone());
    }
    creds
}

impl FeedFilter {
    pub fn matches_tx(&self, tx: &BlockTx, delegators: &HashSet<Vec<u8>>) -> bool {
        if let FeedFilter::Address(addr) = self {
            return tx.outputs.iter().any(|o| o.address == *addr)
                || tx
                    .inputs
                    .iter()
                    .any(|i| i.address.as_deref() == Some(addr.as_str()));
        }
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
            // ReplayCursor is replay-only (sent directly, never broadcast through here),
            // but pass non-tx events through unchanged.
            Event::Rollback { .. }
            | Event::MempoolPrune { .. }
            | Event::ReplayCursor { .. }
            | Event::Reward { .. } => Some(event.clone()),
        }
    }
}

/// Compute net stake change for a single tx relative to feed delegators.
/// Combines UTXO changes (outputs - inputs - withdrawals) with delegation
/// impact (live_stake gained/lost from delegation certificate changes).
pub fn apply_stake_change(tx: &mut BlockTx, delegators: &HashSet<Vec<u8>>, filter: &FeedFilter) {
    // Address feeds: net is simply outputs to the address minus inputs from it.
    if let FeedFilter::Address(addr) = filter {
        let mut net: i64 = 0;
        for output in &tx.outputs {
            if output.address == *addr {
                net += output.lovelace as i64;
            }
        }
        for input in &tx.inputs {
            if input.address.as_deref() == Some(addr.as_str()) {
                net -= input.lovelace as i64;
            }
        }
        if net != 0 {
            tx.stake_change = Some(net);
        }
        return;
    }

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
    // Delegation impact (live_stake gained/lost from delegation certificates)
    // applies only to pool/drep feeds. A stake address's own balance doesn't
    // change when it re-delegates, so skip it for stake feeds.
    let pool_or_drep = match filter {
        FeedFilter::Pool(_) => Some(false),
        FeedFilter::DRep(_) => Some(true),
        FeedFilter::Stake(_) => None,
        FeedFilter::Address(_) => None, // handled by the early return above
    };
    if let Some(is_drep) = pool_or_drep {
        let feed_id = filter.feed_id();
        for deleg in &tx.delegations {
            let (to_id, from_id) = if is_drep {
                (deleg.to_drep_id.as_deref(), deleg.from_drep_id.as_deref())
            } else {
                (deleg.to_pool_id.as_deref(), deleg.from_pool_id.as_deref())
            };
            if to_id == Some(feed_id.as_str()) && from_id != Some(feed_id.as_str()) {
                net += deleg.live_stake;
            } else if from_id == Some(feed_id.as_str()) && to_id != Some(feed_id.as_str()) {
                net -= deleg.live_stake;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pallas::stake_address_from_cred_bytes;

    fn encode(hrp: &str, data: &[u8]) -> String {
        bech32::encode::<bech32::Bech32>(bech32::Hrp::parse(hrp).unwrap(), data).unwrap()
    }

    /// Every subject kind parses from its bech32/path form into the right `FeedFilter`
    /// variant, and `feed_id` is the exact inverse (so request routing round-trips).
    #[test]
    fn from_path_and_feed_id_roundtrip() {
        // Pool.
        let pool_hash = vec![0xaau8; 28];
        let pool_id = pool_bech32_id(&pool_hash);
        assert!(
            matches!(FeedFilter::from_path(&pool_id), Some(FeedFilter::Pool(ref h)) if *h == pool_hash)
        );
        assert_eq!(FeedFilter::from_path(&pool_id).unwrap().feed_id(), pool_id);

        // DRep (key) and DRep (script).
        let drep_key = [&[0x00u8][..], &[0xd1u8; 28]].concat();
        let drep_id = drep_bech32_id(&drep_key);
        assert!(
            matches!(FeedFilter::from_path(&drep_id), Some(FeedFilter::DRep(ref b)) if *b == drep_key)
        );
        assert_eq!(FeedFilter::from_path(&drep_id).unwrap().feed_id(), drep_id);
        let drep_script = [&[0x01u8][..], &[0xd1u8; 28]].concat();
        let ds_id = drep_bech32_id(&drep_script);
        assert!(
            matches!(FeedFilter::from_path(&ds_id), Some(FeedFilter::DRep(ref b)) if *b == drep_script)
        );
        assert_eq!(FeedFilter::from_path(&ds_id).unwrap().feed_id(), ds_id);

        // Predefined DReps (no bech32).
        assert!(
            matches!(FeedFilter::from_path("drep_always_abstain"), Some(FeedFilter::DRep(ref b)) if *b == vec![0x02])
        );
        assert!(
            matches!(FeedFilter::from_path("drep_always_no_confidence"), Some(FeedFilter::DRep(ref b)) if *b == vec![0x03])
        );

        // Stake (mainnet + testnet) via the reward-address bech32.
        let cred = vec![0x42u8; 28];
        let stake_mainnet = stake_address_from_cred_bytes(&cred, true);
        match FeedFilter::from_path(&stake_mainnet) {
            Some(FeedFilter::Stake(p)) => {
                assert_eq!(p.len(), 29);
                assert_eq!(&p[1..], cred.as_slice());
            }
            _ => panic!("stake1 did not parse"),
        }
        assert_eq!(
            FeedFilter::from_path(&stake_mainnet).unwrap().feed_id(),
            stake_mainnet
        );
        let stake_test = stake_address_from_cred_bytes(&cred, false);
        assert_eq!(
            FeedFilter::from_path(&stake_test).unwrap().feed_id(),
            stake_test
        );

        // Payment address: kept verbatim.
        let addr = encode("addr", &[&[0x01u8][..], &[0x33u8; 56]].concat());
        assert!(
            matches!(FeedFilter::from_path(&addr), Some(FeedFilter::Address(ref a)) if *a == addr)
        );
        assert_eq!(FeedFilter::from_path(&addr).unwrap().feed_id(), addr);
    }

    #[test]
    fn from_path_rejects_garbage_and_wrong_shapes() {
        assert!(FeedFilter::from_path("not bech32!").is_none());
        assert!(FeedFilter::from_path("").is_none());
        // Valid bech32 but unknown hrp.
        assert!(FeedFilter::from_path(&encode("xyz", &[0u8; 10])).is_none());
        // Right hrp, wrong payload length.
        assert!(FeedFilter::from_path(&encode("pool", &[0u8; 27])).is_none());
        assert!(FeedFilter::from_path(&encode("stake", &[0xe1u8; 28])).is_none());
        // 28, needs 29
    }
}
