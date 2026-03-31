use serde::Serialize;

mod string {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}

mod string_i64 {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}

mod opt_string_i64 {
    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => serializer.serialize_str(&v.to_string()),
            None => serializer.serialize_none(),
        }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Vec<String>>,
    /// Net stake change in lovelace for pool feed stake-change blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    #[serde(with = "opt_string_i64")]
    pub stake_change: Option<i64>,
    /// Pre-extracted stake credentials from input/output addresses.
    #[serde(skip)]
    pub stake_credentials: Vec<Vec<u8>>,
    /// Withdrawals: (stake_credential, lovelace). Used for stake_change computation.
    #[serde(skip)]
    pub withdrawals: Vec<(Vec<u8>, u64)>,
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
    #[serde(with = "string_i64")]
    pub live_stake: i64,
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
    MempoolPrune {
        removed: Vec<String>,
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
