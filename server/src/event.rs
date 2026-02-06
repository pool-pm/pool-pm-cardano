use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct BlockTx {
    pub hash: String,
    pub fee: u64,
    pub size: usize,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutputInfo>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    MempoolTx(BlockTx),
    Block {
        slot: u64,
        hash: String,
        number: u64,
        timestamp: u64,
        txs: Vec<BlockTx>,
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
