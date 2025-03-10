use sqlx::types::Decimal;

#[derive(sqlx::FromRow, Clone)]
pub struct Pool {
    pub hash_raw: Vec<u8>,
    pub vrf_key_hash: Vec<u8>,
    pub pledge: Decimal,
    pub margin: f64,
    pub fixed_cost: Decimal,
}

#[derive(Clone)]
pub struct TxOutput {
    pub lovelaces: Decimal,
    pub address: Vec<u8>,
}
