use futures::TryStreamExt;
use imbl::{hashmap::HashMap, hashset::HashSet};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use tokio::time::Duration;
use url::Url;

use crate::model::Pool;

pub struct DbSync {
    db: sqlx::Pool<sqlx::Postgres>,
}

impl DbSync {
    pub async fn new(url: &Url) -> Result<Self, sqlx::Error> {
        let options = PgConnectOptions::from_url(&url)?
            .log_slow_statements(log::LevelFilter::Warn, Duration::from_secs(15));

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

    pub async fn resolve_utxo(
        &self,
        tx_hash: &[u8],
        index: i16,
    ) -> Result<Option<(String, sqlx::types::Decimal)>, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT tx_out.address, tx_out.value
            FROM tx_out
            JOIN tx ON tx.id = tx_out.tx_id
            WHERE tx.hash = $1 AND tx_out.index = $2"#,
            tx_hash,
            index
        )
        .fetch_optional(&self.db)
        .await?;

        Ok(row.map(|r| (r.address, r.value)))
    }

    pub async fn pools(&self, last_tx_id: i64) -> Result<HashMap<String, Pool>, sqlx::Error> {
        Ok(sqlx::query_as!(
            Pool,
            r#"SELECT DISTINCT ON (hash_raw)
            hash_raw, vrf_key_hash, pledge, margin, fixed_cost,
            (SELECT ticker_name FROM off_chain_pool_data WHERE pool_id = pool_hash.id ORDER BY id DESC LIMIT 1) as ticker
            FROM pool_update
            JOIN pool_hash ON pool_hash.id=hash_id
            WHERE registered_tx_id <= $1
            GROUP BY hash_raw, vrf_key_hash, pool_update.id, pool_hash.id
            ORDER BY hash_raw, pool_update.id DESC"#,
            last_tx_id
        )
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(|pool| (hex::encode(&pool.hash_raw), pool))
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
