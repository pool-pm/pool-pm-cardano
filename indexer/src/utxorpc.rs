use oura::framework::ChainConfig;
use pallas::interop::utxorpc::spec::cardano::{
    certificate::Certificate, stake_credential, Block, Tx,
};

#[derive(Debug)]
pub struct Delegation {
    pub addr: Vec<u8>,
    pub pool_keyhash: Vec<u8>,
}

pub trait BlockExt {
    fn txs(&self) -> impl Iterator<Item = &Tx>;
    fn certificates(&self) -> impl Iterator<Item = &Certificate>;
    fn stake_delegations(&self, chain: &ChainConfig) -> Vec<Delegation>;
    fn stake_deregistrations(&self, chain: &ChainConfig) -> Vec<Vec<u8>>;
}

impl BlockExt for Block {
    fn txs(&self) -> impl Iterator<Item = &Tx> {
        self.body.iter().flat_map(|body| body.tx.iter())
    }

    fn certificates(&self) -> impl Iterator<Item = &Certificate> {
        self.txs()
            .flat_map(|tx| tx.certificates.iter().flat_map(|c| c.certificate.iter()))
    }

    fn stake_delegations(&self, chain: &ChainConfig) -> Vec<Delegation> {
        self.certificates()
            .flat_map(|c| match &c {
                Certificate::StakeDelegation(cert) => {
                    cert.stake_credential.as_ref().and_then(|cred| {
                        cred.stake_credential.as_ref().and_then(|cred| {
                            Some(Delegation {
                                addr: cred.addr(chain),
                                pool_keyhash: cert.pool_keyhash.to_vec(),
                            })
                        })
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn stake_deregistrations(&self, chain: &ChainConfig) -> Vec<Vec<u8>> {
        self.certificates()
            .flat_map(|c| match &c {
                Certificate::StakeDeregistration(cred) => cred
                    .stake_credential
                    .as_ref()
                    .and_then(|cred| Some(cred.addr(chain))),
                _ => None,
            })
            .collect()
    }
}

pub trait StakeCredentialExt {
    fn addr(&self, chain: &ChainConfig) -> Vec<u8>;
}

impl StakeCredentialExt for stake_credential::StakeCredential {
    // https://cips.cardano.org/cip/CIP-19
    fn addr(&self, chain: &ChainConfig) -> Vec<u8> {
        let (mut prefix, bytes) = match self {
            Self::AddrKeyHash(bytes) => (14 << 4, bytes),
            Self::ScriptHash(bytes) => (15 << 4, bytes),
        };
        if let ChainConfig::Mainnet = chain {
            prefix |= 1;
        }
        [vec![prefix], bytes.to_vec()].concat()
    }
}
