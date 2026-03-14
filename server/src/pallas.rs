use pallas::ledger::primitives::{alonzo, conway, StakeCredential};
use pallas::ledger::traverse::{MultiEraCert, MultiEraTx};

/// (stake_credential_bytes, Some(target_bytes)) for delegation,
/// (stake_credential_bytes, None) for deregistration.
pub type PoolDelegationChange = (Vec<u8>, Option<Vec<u8>>);
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
        (StakeCredential::AddrKeyhash(_), true) => (0xe0u8, "stake"),
        (StakeCredential::AddrKeyhash(_), false) => (0xe1u8, "stake_test"),
        (StakeCredential::ScriptHash(_), true) => (0xf0u8, "stake"),
        (StakeCredential::ScriptHash(_), false) => (0xf1u8, "stake_test"),
    };
    let hash = stake_credential_bytes(cred);
    let mut payload = Vec::with_capacity(29);
    payload.push(header);
    payload.extend_from_slice(&hash);
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

pub trait MultiEraTxExt {
    /// Pool delegation certificates with full StakeCredential preserved.
    fn pool_delegation_certs(&self) -> Vec<PoolDelegationCert>;

    /// Pool delegation changes as raw bytes (for state updates).
    fn pool_delegation_changes(&self) -> Vec<PoolDelegationChange> {
        self.pool_delegation_certs()
            .iter()
            .map(|(cred, pool)| (stake_credential_bytes(cred), pool.clone()))
            .collect()
    }

    fn drep_delegation_changes(&self) -> Vec<DrepDelegationChange>;

    /// Pool registration certificates (used for both new pools and parameter updates).
    fn pool_updates(&self) -> Vec<PoolUpdate>;
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
}
