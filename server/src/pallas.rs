use pallas::ledger::primitives::{alonzo, conway};
use pallas::ledger::traverse::{MultiEraCert, MultiEraTx};

/// (stake_credential_bytes, Some(pool_hash_bytes)) for delegation,
/// (stake_credential_bytes, None) for deregistration.
pub type DelegationChange = (Vec<u8>, Option<Vec<u8>>);

fn stake_credential_bytes(cred: &pallas::ledger::primitives::StakeCredential) -> Vec<u8> {
    match cred {
        pallas::ledger::primitives::StakeCredential::AddrKeyhash(h) => h.as_ref().to_vec(),
        pallas::ledger::primitives::StakeCredential::ScriptHash(h) => h.as_ref().to_vec(),
    }
}

pub trait MultiEraTxExt {
    fn delegation_changes(&self) -> Vec<DelegationChange>;
}

impl MultiEraTxExt for MultiEraTx<'_> {
    fn delegation_changes(&self) -> Vec<DelegationChange> {
        let mut changes = Vec::new();
        for cert in self.certs() {
            match cert {
                MultiEraCert::AlonzoCompatible(c) => match &**c {
                    alonzo::Certificate::StakeDelegation(cred, pool) => {
                        changes.push((
                            stake_credential_bytes(cred),
                            Some(pool.as_ref().to_vec()),
                        ));
                    }
                    alonzo::Certificate::StakeDeregistration(cred) => {
                        changes.push((stake_credential_bytes(cred), None));
                    }
                    _ => {}
                },
                MultiEraCert::Conway(c) => match &**c {
                    conway::Certificate::StakeDelegation(cred, pool) => {
                        changes.push((
                            stake_credential_bytes(cred),
                            Some(pool.as_ref().to_vec()),
                        ));
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
}
