use futures::TryStreamExt;
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::collections::HashMap;

use crate::model::Pool;

pub struct DbSync {
    db: sqlx::Pool<sqlx::Postgres>,
}

impl DbSync {
    pub async fn new(url: &String) -> Result<Self, sqlx::Error> {
        let db = PgPoolOptions::new().max_connections(8).connect(url).await?;

        Ok(Self { db })
    }

    pub async fn max_tx_id(&self, slot: u64) -> Result<i64, sqlx::Error> {
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

    pub async fn pools(&self, max_tx_id: i64) -> Result<HashMap<String, Pool>, sqlx::Error> {
        Ok(sqlx::query_as!(Pool,
            r#"SELECT DISTINCT ON (hash_raw) 
            hash_raw, vrf_key_hash, pledge, margin, fixed_cost                                                                   
            FROM pool_update                                                                                                                               
            JOIN pool_hash ON pool_hash.id=hash_id    
            WHERE registered_tx_id <= $1
            GROUP BY hash_raw, vrf_key_hash, pool_update.id                                                                                                
            ORDER BY hash_raw, pool_update.id DESC"#, max_tx_id)
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(|pool| (hex::encode(&pool.vrf_key_hash), pool))
        .collect())
    }
}
