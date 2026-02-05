use serde::Serialize;

#[derive(Clone, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    MempoolTx {
        hash: String,
        fee: u64,
        size: usize,
        inputs: Vec<TxInput>,
        outputs: Vec<TxOutputInfo>,
    },
    Block {
        slot: u64,
        hash: String,
        number: u64,
        timestamp: u64,
        tx_hashes: Vec<String>,
    },
    Rollback {
        slot: u64,
    },
}

#[derive(Clone, Serialize)]
pub struct TxInput {
    pub address: Option<String>,
    pub lovelace: u64,
}

#[derive(Clone, Serialize)]
pub struct TxOutputInfo {
    pub address: String,
    pub lovelace: u64,
    pub assets: Vec<AssetInfo>,
}

#[derive(Clone, Serialize)]
pub struct AssetInfo {
    pub fingerprint: String,
    pub quantity: u64,
}
