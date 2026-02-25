use serde::Serialize;

mod string {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}

#[derive(Clone, Serialize)]
pub struct BlockTx {
    pub hash: String,
    #[serde(with = "string")]
    pub fee: u64,
    pub size: usize,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutputInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub delegations: Vec<DelegationInfo>,
    /// Pre-extracted stake credentials from input/output addresses.
    #[serde(skip)]
    pub stake_credentials: Vec<Vec<u8>>,
}

#[derive(Clone, Serialize)]
pub struct DelegationInfo {
    pub stake_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_ticker: Option<String>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        pool_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pool_ticker: Option<String>,
        txs: Vec<BlockTx>,
    },
    Rollback {
        slot: u64,
    },
}

#[derive(Clone, Serialize)]
pub struct TxInput {
    pub tx_hash: String,
    pub index: i16,
    pub address: Option<String>,
    #[serde(with = "string")]
    pub lovelace: u64,
}

#[derive(Clone, Serialize)]
pub struct TxOutputInfo {
    pub address: String,
    #[serde(with = "string")]
    pub lovelace: u64,
    pub assets: Vec<AssetInfo>,
}

#[derive(Clone, Serialize)]
pub struct AssetInfo {
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(with = "string")]
    pub quantity: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tk: Option<String>,
}
