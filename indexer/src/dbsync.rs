use futures::TryStreamExt;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    types::Decimal,
    ConnectOptions,
};
use std::collections::{HashMap, HashSet};
use tokio::time::Duration;
use url::Url;

use crate::model::{Pool, TxOutput};

pub struct DbSync {
    db: sqlx::Pool<sqlx::Postgres>,
}

impl DbSync {
    pub async fn new(url: &Url) -> Result<Self, sqlx::Error> {
        let options = PgConnectOptions::from_url(&url)?
            .log_slow_statements(log::LevelFilter::Warn, Duration::from_secs(10));

        let db = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;

        Ok(Self { db })
    }

    pub async fn last_slot_tx_id(&self, slot: u64) -> Result<i64, sqlx::Error> {
        let tx = sqlx::query!(
            r#"SELECT tx.id FROM tx
            JOIN block ON block.id=tx.block_id
            WHERE block.slot_no <= $1
            ORDER BY tx.id DESC
            LIMIT 1"#,
            slot as i64
        )
        .fetch_one(&self.db)
        .await?;

        Ok(tx.id)
    }

    pub async fn utxos(
        &self,
        last_tx_id: i64,
    ) -> Result<(HashMap<(Vec<u8>, i16), TxOutput>, HashMap<Vec<u8>, Decimal>), sqlx::Error> {
        let mut rows = sqlx::query!(
            r#"SELECT tx.hash, tx_out.index, tx_out.value, stake_address.hash_raw
            FROM tx_out
            JOIN tx ON tx.id=tx_id
            JOIN stake_address ON stake_address.id=stake_address_id
            WHERE consumed_by_tx_id IS NULL
            OR consumed_by_tx_id > $1
            "#,
            last_tx_id
        )
        .fetch(&self.db);

        let mut utxos: HashMap<(Vec<u8>, i16), TxOutput> = HashMap::new();
        let mut stakes: HashMap<Vec<u8>, Decimal> = HashMap::new();

        while let Some(row) = rows.try_next().await? {
            utxos.insert(
                (row.hash, row.index),
                TxOutput {
                    lovelaces: row.value,
                    address: row.hash_raw.clone(),
                },
            );
            *stakes.entry(row.hash_raw).or_default() += row.value;
        }

        Ok((utxos, stakes))
    }

    pub async fn pools(&self, last_tx_id: i64) -> Result<HashMap<String, Pool>, sqlx::Error> {
        Ok(sqlx::query_as!(
            Pool,
            r#"SELECT DISTINCT ON (hash_raw)
            hash_raw, vrf_key_hash, pledge, margin, fixed_cost
            FROM pool_update
            JOIN pool_hash ON pool_hash.id=hash_id
            WHERE registered_tx_id <= $1
            GROUP BY hash_raw, vrf_key_hash, pool_update.id
            ORDER BY hash_raw, pool_update.id DESC"#,
            last_tx_id
        )
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(|pool| (hex::encode(&pool.vrf_key_hash), pool))
        .collect())
    }

    pub async fn delegations(
        &self,
        last_tx_id: i64,
    ) -> Result<
        (
            HashMap<Vec<u8>, Vec<u8>>,
            HashMap<Vec<u8>, HashSet<Vec<u8>>>,
        ),
        sqlx::Error,
    > {
        let mut rows = sqlx::query!(
            r#"SELECT stake_address.hash_raw as stake_address, pool_hash.hash_raw as pool_id FROM
                (SELECT DISTINCT ON (addr_id) *
                    FROM delegation
                    WHERE tx_id <= $1
                    ORDER BY addr_id, id DESC
            ) delegation
            JOIN stake_address ON stake_address.id = delegation.addr_id
            JOIN pool_hash ON pool_hash.id = delegation.pool_hash_id
            WHERE NOT EXISTS
                (SELECT TRUE
                    FROM stake_deregistration
                    WHERE stake_deregistration.tx_id <= $1
                    AND stake_deregistration.addr_id = delegation.addr_id
                    AND stake_deregistration.tx_id >= delegation.tx_id
                )"#,
            last_tx_id
        )
        .fetch(&self.db);

        let mut delegations: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>> = HashMap::new();
        while let Some(row) = rows.try_next().await? {
            delegations.insert(row.stake_address.clone(), row.pool_id.clone());
            delegators
                .entry(row.pool_id)
                .or_default()
                .insert(row.stake_address);
        }

        Ok((delegations, delegators))
    }
}
