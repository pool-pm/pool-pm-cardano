use pallas::crypto::hash::Hasher;
use pallas::ledger::primitives::{alonzo, conway, Metadatum, StakeCredential};
use pallas::ledger::traverse::{MultiEraCert, MultiEraTx};

use crate::event::CatalystInfo;

/// CIP-20 transaction message standard (label 674 → `{ "msg": [lines] }`).
const CIP20_MESSAGE: u64 = 674;
/// CIP-36/CIP-15 Catalyst voting registration and its separate witness label.
/// Surfaced as a structured `CatalystInfo` (see `extract_catalyst`), not a text line.
const CATALYST_REGISTRATION: u64 = 61284;
const CATALYST_WITNESS: u64 = 61285;
/// SundaeSwap on-chain governance tally (the number is the first digits of π).
const SUNDAE_GOVERNANCE: u64 = 31415;
const SUNDAE_LABEL: &str = "SundaeSwap governance";

/// Display lines for a tx's metadata, one per label, ordered by label value: the
/// CIP-20 message text for 674, a Catalyst-registration badge for the CIP-36
/// registration (its witness label is folded in), and a generic "metadata N" for
/// any other label. `None` if the tx carries no metadata. Stateless — derived
/// purely from the tx's auxiliary data.
pub fn extract_tx_metadata(tx: &MultiEraTx<'_>) -> Option<Vec<String>> {
    let metadata = tx.metadata();
    let mut entries: Vec<(u64, &Metadatum)> = metadata.collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort_by_key(|(label, _)| *label);
    let lines = metadata_lines(&entries);
    if lines.is_empty() {
        None
    } else {
        Some(lines)
    }
}

/// Pure label → display-line mapping. `entries` must be sorted by label. Split out
/// from `extract_tx_metadata` so the ordering/labeling rules are unit-testable.
fn metadata_lines(entries: &[(u64, &Metadatum)]) -> Vec<String> {
    let mut lines = Vec::new();
    for (label, datum) in entries {
        match *label {
            CIP20_MESSAGE => match cip20_message_lines(datum) {
                Some(msg) => lines.extend(msg),
                None => lines.push(format!("metadata {label}")),
            },
            // Catalyst registration (+witness) is surfaced structurally, not as text.
            CATALYST_REGISTRATION | CATALYST_WITNESS => {}
            SUNDAE_GOVERNANCE => lines.push(SUNDAE_LABEL.to_string()),
            _ => lines.push(format!("metadata {label}")),
        }
    }
    lines
}

/// Extract a CIP-36/CIP-15 Catalyst voting registration (label 61284). The
/// registrant's stake address is `blake2b-224(staking vkey)` (field `2`) built into
/// a reward address. `live_stake` is left `None` (filled by the stake-feed walk).
pub fn extract_catalyst(tx: &MultiEraTx<'_>, mainnet: bool) -> Option<CatalystInfo> {
    let metadata = tx.metadata();
    let Metadatum::Map(entries) = metadata.find(CATALYST_REGISTRATION)? else {
        return None;
    };
    // Field 2 = the staking public key (32-byte ed25519 vkey).
    let vkey = entries.iter().find_map(|(k, v)| match (k, v) {
        (Metadatum::Int(i), Metadatum::Bytes(b)) if i128::from(*i) == 2 => Some(b),
        _ => None,
    })?;
    let cred = Hasher::<224>::hash(vkey);
    Some(CatalystInfo {
        stake_address: stake_address_from_cred_bytes(cred.as_ref(), mainnet),
        live_stake: None,
    })
}

/// CIP-20: the `msg` field of label 674 is an array of text lines.
fn cip20_message_lines(datum: &Metadatum) -> Option<Vec<String>> {
    let Metadatum::Map(entries) = datum else {
        return None;
    };
    for (key, value) in entries.iter() {
        if let Metadatum::Text(k) = key {
            if k == "msg" {
                if let Metadatum::Array(items) = value {
                    let lines: Vec<String> = items
                        .iter()
                        .filter_map(|item| match item {
                            Metadatum::Text(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    if !lines.is_empty() {
                        return Some(lines);
                    }
                }
            }
        }
    }
    None
}

pub type DrepDelegationChange = (Vec<u8>, Option<Vec<u8>>);

/// (operator_hash, pledge, cost, margin_numerator, margin_denominator)
pub type PoolUpdate = (Vec<u8>, u64, u64, u64, u64);

/// (full StakeCredential, Some(pool_hash)) for delegation,
/// (full StakeCredential, None) for deregistration.
pub type PoolDelegationCert = (StakeCredential, Option<Vec<u8>>);

pub fn stake_credential_bytes(cred: &StakeCredential) -> Vec<u8> {
    match cred {
        StakeCredential::AddrKeyhash(h) => h.as_ref().to_vec(),
        StakeCredential::ScriptHash(h) => h.as_ref().to_vec(),
    }
}

/// Extract the 28-byte stake credential from raw address bytes.
/// Works for base addresses (types 0-3) and reward addresses (types 14-15).
pub fn stake_credential_from_address_bytes(addr: &[u8]) -> Option<Vec<u8>> {
    if addr.is_empty() {
        return None;
    }
    let addr_type = addr[0] >> 4;
    match addr_type {
        // Base addresses: 1 header + 28 payment + 28 stake
        0..=3 if addr.len() >= 57 => Some(addr[29..57].to_vec()),
        // Reward addresses: 1 header + 28 stake
        14 | 15 if addr.len() >= 29 => Some(addr[1..29].to_vec()),
        _ => None,
    }
}

pub fn stake_address_bech32(cred: &StakeCredential, mainnet: bool) -> String {
    use bech32::{Bech32, Hrp};
    let (header, hrp) = match (cred, mainnet) {
        (StakeCredential::AddrKeyhash(_), true) => (0xe1u8, "stake"),
        (StakeCredential::AddrKeyhash(_), false) => (0xe0u8, "stake_test"),
        (StakeCredential::ScriptHash(_), true) => (0xf1u8, "stake"),
        (StakeCredential::ScriptHash(_), false) => (0xf0u8, "stake_test"),
    };
    let hash = stake_credential_bytes(cred);
    let mut payload = Vec::with_capacity(29);
    payload.push(header);
    payload.extend_from_slice(&hash);
    bech32::encode::<Bech32>(Hrp::parse(hrp).unwrap(), &payload).unwrap()
}

/// Build a bech32 stake address from raw 28-byte credential bytes.
/// Assumes key-based credential (0xe1/0xe0 header). Used when we only
/// have the raw bytes (e.g. from drep_delegation_changes) without the
/// StakeCredential enum.
pub fn stake_address_from_cred_bytes(cred: &[u8], mainnet: bool) -> String {
    use bech32::{Bech32, Hrp};
    let (header, hrp) = if mainnet {
        (0xe1u8, "stake")
    } else {
        (0xe0u8, "stake_test")
    };
    let mut payload = Vec::with_capacity(29);
    payload.push(header);
    payload.extend_from_slice(cred);
    bech32::encode::<Bech32>(Hrp::parse(hrp).unwrap(), &payload).unwrap()
}

pub fn drep_to_bytes(drep: &conway::DRep) -> Vec<u8> {
    match drep {
        conway::DRep::Key(h) => [&[0x00], h.as_ref()].concat(),
        conway::DRep::Script(h) => [&[0x01], h.as_ref()].concat(),
        conway::DRep::Abstain => vec![0x02],
        conway::DRep::NoConfidence => vec![0x03],
    }
}

/// Extracted voting procedure: (voter, gov_action_id, vote).
pub type ExtractedVote = (conway::Voter, conway::GovActionId, conway::Vote);

pub trait MultiEraTxExt {
    /// Pool delegation certificates with full StakeCredential preserved.
    fn pool_delegation_certs(&self) -> Vec<PoolDelegationCert>;

    fn drep_delegation_changes(&self) -> Vec<DrepDelegationChange>;

    /// Pool registration certificates (used for both new pools and parameter updates).
    fn pool_updates(&self) -> Vec<PoolUpdate>;

    /// Pool retirement certificates as `(operator, retiring_epoch)`.
    fn pool_retirements(&self) -> Vec<(Vec<u8>, u64)>;

    /// Governance voting procedures from Conway-era transactions.
    fn voting_procedures(&self) -> Vec<ExtractedVote>;
}

impl MultiEraTxExt for MultiEraTx<'_> {
    fn pool_delegation_certs(&self) -> Vec<PoolDelegationCert> {
        let mut certs = Vec::new();
        for cert in self.certs() {
            match cert {
                MultiEraCert::AlonzoCompatible(c) => match &**c {
                    alonzo::Certificate::StakeDelegation(cred, pool) => {
                        certs.push((cred.clone(), Some(pool.as_ref().to_vec())));
                    }
                    alonzo::Certificate::StakeDeregistration(cred) => {
                        certs.push((cred.clone(), None));
                    }
                    _ => {}
                },
                MultiEraCert::Conway(c) => match &**c {
                    conway::Certificate::StakeDelegation(cred, pool)
                    | conway::Certificate::StakeVoteDeleg(cred, pool, _)
                    | conway::Certificate::StakeRegDeleg(cred, pool, _)
                    | conway::Certificate::StakeVoteRegDeleg(cred, pool, _, _) => {
                        certs.push((cred.clone(), Some(pool.as_ref().to_vec())));
                    }
                    conway::Certificate::StakeDeregistration(cred)
                    | conway::Certificate::UnReg(cred, _) => {
                        certs.push((cred.clone(), None));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        certs
    }

    fn drep_delegation_changes(&self) -> Vec<DrepDelegationChange> {
        let mut changes = Vec::new();
        for cert in self.certs() {
            if let MultiEraCert::Conway(c) = cert {
                match &**c {
                    conway::Certificate::VoteDeleg(cred, drep)
                    | conway::Certificate::StakeVoteDeleg(cred, _, drep)
                    | conway::Certificate::VoteRegDeleg(cred, drep, _)
                    | conway::Certificate::StakeVoteRegDeleg(cred, _, drep, _) => {
                        changes.push((stake_credential_bytes(cred), Some(drep_to_bytes(drep))));
                    }
                    conway::Certificate::StakeDeregistration(cred)
                    | conway::Certificate::UnReg(cred, _) => {
                        changes.push((stake_credential_bytes(cred), None));
                    }
                    _ => {}
                }
            }
        }
        changes
    }

    fn pool_updates(&self) -> Vec<PoolUpdate> {
        self.certs()
            .iter()
            .filter_map(|cert| match cert {
                MultiEraCert::AlonzoCompatible(c) => match &***c {
                    alonzo::Certificate::PoolRegistration {
                        operator,
                        pledge,
                        cost,
                        margin,
                        ..
                    } => Some((operator, pledge, cost, margin)),
                    _ => None,
                },
                MultiEraCert::Conway(c) => match &***c {
                    conway::Certificate::PoolRegistration {
                        operator,
                        pledge,
                        cost,
                        margin,
                        ..
                    } => Some((operator, pledge, cost, margin)),
                    _ => None,
                },
                _ => None,
            })
            .map(|(operator, pledge, cost, margin)| {
                (
                    operator.as_ref().to_vec(),
                    *pledge,
                    *cost,
                    margin.numerator,
                    margin.denominator,
                )
            })
            .collect()
    }

    fn pool_retirements(&self) -> Vec<(Vec<u8>, u64)> {
        self.certs()
            .iter()
            .filter_map(|cert| match cert {
                MultiEraCert::AlonzoCompatible(c) => match &***c {
                    alonzo::Certificate::PoolRetirement(operator, epoch) => {
                        Some((operator.as_ref().to_vec(), *epoch))
                    }
                    _ => None,
                },
                MultiEraCert::Conway(c) => match &***c {
                    conway::Certificate::PoolRetirement(operator, epoch) => {
                        Some((operator.as_ref().to_vec(), *epoch))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn voting_procedures(&self) -> Vec<ExtractedVote> {
        let mut votes = Vec::new();
        if let MultiEraTx::Conway(tx) = self {
            if let Some(ref procedures) = tx.transaction_body.voting_procedures {
                for (voter, actions) in procedures.iter() {
                    for (action_id, procedure) in actions.iter() {
                        votes.push((voter.clone(), action_id.clone(), procedure.vote.clone()));
                    }
                }
            }
        }
        votes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Metadatum {
        Metadatum::Text(s.to_string())
    }

    /// A label whose datum content is irrelevant to the line it produces.
    fn opaque() -> Metadatum {
        Metadatum::Int(0.into())
    }

    fn cip20(lines: &[&str]) -> Metadatum {
        let items = lines.iter().map(|s| text(s)).collect();
        Metadatum::Map(vec![(text("msg"), Metadatum::Array(items))].into())
    }

    #[test]
    fn orders_by_label_and_labels_each_kind() {
        let msg = cip20(&["hi", "there"]);
        let nft = opaque();
        let sundae = opaque();
        // Deliberately unsorted; extract sorts, but metadata_lines documents sorted input.
        let entries = [
            (CIP20_MESSAGE, &msg),
            (SUNDAE_GOVERNANCE, &sundae),
            (CATALYST_WITNESS, &opaque()),
            (CATALYST_REGISTRATION, &opaque()),
            (721u64, &nft),
        ];
        let mut sorted = entries;
        sorted.sort_by_key(|(l, _)| *l);
        let lines = metadata_lines(&sorted);
        // 674 (msg) < 721 < 31415 (Sundae) < 61284/61285 (Catalyst → structured, no line)
        assert_eq!(
            lines,
            vec![
                "hi".to_string(),
                "there".to_string(),
                "metadata 721".to_string(),
                SUNDAE_LABEL.to_string(),
            ]
        );
    }

    #[test]
    fn catalyst_labels_produce_no_text_line() {
        // Both Catalyst labels are surfaced structurally (CatalystInfo), never as text.
        let lines = metadata_lines(&[
            (CATALYST_REGISTRATION, &opaque()),
            (CATALYST_WITNESS, &opaque()),
        ]);
        assert!(lines.is_empty());
    }

    #[test]
    fn unparseable_674_falls_back_to_generic() {
        let lines = metadata_lines(&[(CIP20_MESSAGE, &opaque())]);
        assert_eq!(lines, vec!["metadata 674"]);
    }
}
