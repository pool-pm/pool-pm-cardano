use pallas::ledger::primitives::{alonzo, conway};
use pallas::ledger::traverse::{MultiEraCert, MultiEraTx};

/// (stake_credential_bytes, Some(target_bytes)) for delegation,
/// (stake_credential_bytes, None) for deregistration.
pub type PoolDelegationChange = (Vec<u8>, Option<Vec<u8>>);
pub type DrepDelegationChange = (Vec<u8>, Option<Vec<u8>>);

fn stake_credential_bytes(cred: &pallas::ledger::primitives::StakeCredential) -> Vec<u8> {
    match cred {
        pallas::ledger::primitives::StakeCredential::AddrKeyhash(h) => h.as_ref().to_vec(),
        pallas::ledger::primitives::StakeCredential::ScriptHash(h) => h.as_ref().to_vec(),
    }
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
    fn pool_delegation_changes(&self) -> Vec<PoolDelegationChange>;
    fn drep_delegation_changes(&self) -> Vec<DrepDelegationChange>;
}

impl MultiEraTxExt for MultiEraTx<'_> {
    fn pool_delegation_changes(&self) -> Vec<PoolDelegationChange> {
        let mut changes = Vec::new();
        for cert in self.certs() {
            match cert {
                MultiEraCert::AlonzoCompatible(c) => match &**c {
                    alonzo::Certificate::StakeDelegation(cred, pool) => {
                        changes.push((stake_credential_bytes(cred), Some(pool.as_ref().to_vec())));
                    }
                    alonzo::Certificate::StakeDeregistration(cred) => {
                        changes.push((stake_credential_bytes(cred), None));
                    }
                    _ => {}
                },
                MultiEraCert::Conway(c) => match &**c {
                    conway::Certificate::StakeDelegation(cred, pool)
                    | conway::Certificate::StakeVoteDeleg(cred, pool, _)
                    | conway::Certificate::StakeRegDeleg(cred, pool, _)
                    | conway::Certificate::StakeVoteRegDeleg(cred, pool, _, _) => {
                        changes.push((stake_credential_bytes(cred), Some(pool.as_ref().to_vec())));
                    }
                    conway::Certificate::StakeDeregistration(cred)
                    | conway::Certificate::UnReg(cred, _) => {
                        changes.push((stake_credential_bytes(cred), None));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        changes
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
}
