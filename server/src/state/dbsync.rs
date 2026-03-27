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

    pub async fn slot_info(&self, slot: u64) -> Result<(i64, String), sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT tx.id, block.hash FROM tx
            JOIN block ON block.id=tx.block_id
            WHERE block.slot_no <= $1
            ORDER BY tx.id DESC
            LIMIT 1"#,
            slot as i64
        )
        .fetch_one(&self.db)
        .await?;

        Ok((row.id, hex::encode(row.hash)))
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

    pub async fn resolve_utxos_batch(
        &self,
        inputs: &[(Vec<u8>, i16)],
    ) -> Result<std::collections::HashMap<(Vec<u8>, i16), (String, u64)>, sqlx::Error> {
        if inputs.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let hashes: Vec<Vec<u8>> = inputs.iter().map(|(h, _)| h.clone()).collect();
        let indices: Vec<i16> = inputs.iter().map(|(_, i)| *i).collect();
        let rows = sqlx::query!(
            r#"SELECT tx.hash, tx_out.index AS "index!: i16", tx_out.address, tx_out.value
            FROM tx_out
            JOIN tx ON tx.id = tx_out.tx_id
            WHERE (tx.hash, tx_out.index) IN (SELECT * FROM UNNEST($1::bytea[], $2::smallint[]))"#,
            &hashes,
            &indices
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let lovelace: u64 = r.value.try_into().expect("lovelace must fit u64");
                ((r.hash, r.index), (r.address, lovelace))
            })
            .collect())
    }

    pub async fn pools(&self, last_tx_id: i64) -> Result<HashMap<String, Pool>, sqlx::Error> {
        Ok(sqlx::query_as!(
            Pool,
            r#"SELECT DISTINCT ON (hash_raw)
            hash_raw, pledge, margin, fixed_cost,
            (SELECT ticker_name FROM off_chain_pool_data WHERE pool_id = pool_hash.id ORDER BY id DESC LIMIT 1) as ticker
            FROM pool_update
            JOIN pool_hash ON pool_hash.id=hash_id
            WHERE registered_tx_id <= $1
            GROUP BY hash_raw, pool_update.id, pool_hash.id
            ORDER BY hash_raw, pool_update.id DESC"#,
            last_tx_id
        )
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(|pool| (hex::encode(&pool.hash_raw), pool))
        .collect())
    }

    pub async fn pool_delegations(
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
            // db-sync hash_raw is 29 bytes (header + 28-byte credential); strip header
            let cred = row.stake_address[1..].to_vec();
            delegations.insert(cred.clone(), row.pool_id.clone());
            delegators.entry(row.pool_id).or_default().insert(cred);
        }

        Ok((delegations, delegators))
    }

    pub async fn utxo_stakes(
        &self,
        last_tx_id: i64,
    ) -> Result<HashMap<Vec<u8>, i64>, sqlx::Error> {
        let mut rows = sqlx::query!(
            r#"SELECT stake_address.hash_raw AS stake_address,
                      SUM(tx_out.value)::bigint AS "stake!"
            FROM tx_out
            JOIN stake_address ON stake_address.id = tx_out.stake_address_id
            WHERE tx_out.tx_id <= $1
              AND (tx_out.consumed_by_tx_id IS NULL OR tx_out.consumed_by_tx_id > $1)
            GROUP BY stake_address.hash_raw"#,
            last_tx_id
        )
        .fetch(&self.db);

        let mut stakes: HashMap<Vec<u8>, i64> = HashMap::new();
        while let Some(row) = rows.try_next().await? {
            let cred = row.stake_address[1..].to_vec();
            stakes.insert(cred, row.stake);
        }

        Ok(stakes)
    }

    pub async fn rewards(
        &self,
        current_epoch: u64,
        last_tx_id: i64,
    ) -> Result<HashMap<Vec<u8>, i64>, sqlx::Error> {
        let mut rows = sqlx::query!(
            r#"SELECT sa.hash_raw AS stake_address,
                      SUM(t.amount)::bigint AS "net!"
            FROM (
                SELECT addr_id, amount FROM reward WHERE spendable_epoch <= $1
                UNION ALL
                SELECT addr_id, amount FROM reward_rest WHERE spendable_epoch <= $1
                UNION ALL
                SELECT addr_id, -amount FROM withdrawal WHERE tx_id <= $2
            ) t
            JOIN stake_address sa ON sa.id = t.addr_id
            GROUP BY sa.hash_raw"#,
            current_epoch as i64,
            last_tx_id
        )
        .fetch(&self.db);

        let mut rewards: HashMap<Vec<u8>, i64> = HashMap::new();
        while let Some(row) = rows.try_next().await? {
            let cred = row.stake_address[1..].to_vec();
            rewards.insert(cred, row.net);
        }

        Ok(rewards)
    }

    pub async fn epoch_reward_delta(
        &self,
        epoch: u64,
    ) -> Result<HashMap<Vec<u8>, i64>, sqlx::Error> {
        let mut rows = sqlx::query!(
            r#"SELECT sa.hash_raw AS stake_address,
                      SUM(t.amount)::bigint AS "delta!"
            FROM (
                SELECT addr_id, amount FROM reward WHERE spendable_epoch = $1
                UNION ALL
                SELECT addr_id, amount FROM reward_rest WHERE spendable_epoch = $1
            ) t
            JOIN stake_address sa ON sa.id = t.addr_id
            GROUP BY sa.hash_raw"#,
            epoch as i64
        )
        .fetch(&self.db);

        let mut deltas: HashMap<Vec<u8>, i64> = HashMap::new();
        while let Some(row) = rows.try_next().await? {
            let cred = row.stake_address[1..].to_vec();
            deltas.insert(cred, row.delta);
        }

        Ok(deltas)
    }

    pub async fn pool_recent_blocks(
        &self,
        pool_hash: &[u8],
        limit: i64,
    ) -> Result<Vec<(u64, String, u64)>, sqlx::Error> {
        let leader_id = sqlx::query_scalar!(
            r#"SELECT sl.id AS "id!"
            FROM slot_leader sl
            JOIN pool_hash ph ON ph.id = sl.pool_hash_id
            WHERE ph.hash_raw = $1
            LIMIT 1"#,
            pool_hash
        )
        .fetch_optional(&self.db)
        .await?;

        let leader_id = match leader_id {
            Some(id) => id,
            None => return Ok(vec![]),
        };

        let rows = sqlx::query!(
            r#"WITH pool_blocks AS MATERIALIZED (
                SELECT id, slot_no, hash, block_no
                FROM block WHERE slot_leader_id = $1
            )
            SELECT slot_no AS "slot!", hash, block_no AS "block_no!"
            FROM pool_blocks ORDER BY id DESC LIMIT $2"#,
            leader_id,
            limit
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| (r.slot as u64, hex::encode(r.hash), r.block_no as u64))
            .collect())
    }

    pub async fn drep_delegations(
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
            r#"SELECT stake_address.hash_raw as stake_address,
                drep_hash.raw as drep_raw,
                drep_hash.has_script as drep_has_script,
                drep_hash.view as drep_view
            FROM
                (SELECT DISTINCT ON (addr_id) *
                    FROM delegation_vote
                    WHERE tx_id <= $1
                    ORDER BY addr_id, id DESC
                ) dv
            JOIN stake_address ON stake_address.id = dv.addr_id
            JOIN drep_hash ON drep_hash.id = dv.drep_hash_id
            WHERE NOT EXISTS
                (SELECT TRUE
                    FROM stake_deregistration
                    WHERE stake_deregistration.tx_id <= $1
                    AND stake_deregistration.addr_id = dv.addr_id
                    AND stake_deregistration.tx_id >= dv.tx_id
                )"#,
            last_tx_id
        )
        .fetch(&self.db);

        let mut delegations: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>> = HashMap::new();
        while let Some(row) = rows.try_next().await? {
            let drep_bytes = if row.drep_view.starts_with("drep_always_abstain") {
                vec![0x02]
            } else if row.drep_view.starts_with("drep_always_no_confidence") {
                vec![0x03]
            } else if let Some(raw) = &row.drep_raw {
                let tag = if row.drep_has_script { 0x01u8 } else { 0x00 };
                [&[tag][..], raw].concat()
            } else {
                continue;
            };
            // db-sync hash_raw is 29 bytes (header + 28-byte credential); strip header
            let cred = row.stake_address[1..].to_vec();
            delegations.insert(cred.clone(), drep_bytes.clone());
            delegators.entry(drep_bytes).or_default().insert(cred);
        }

        Ok((delegations, delegators))
    }

    /// Find blocks containing the largest outputs to pool delegators in a window.
    /// Returns (slot, block_hash_hex, block_no, pool_hash_raw, pool_ticker) ordered
    /// by largest output value, deduplicated by block.
    pub async fn pool_stake_change_blocks(
        &self,
        boundary_tx_id: i64,
        delegator_hash_raws: &[Vec<u8>],
        limit: i64,
    ) -> Result<Vec<(u64, String, u64, Option<Vec<u8>>, Option<String>)>, sqlx::Error> {
        if delegator_hash_raws.is_empty() {
            return Ok(vec![]);
        }

        // Find the top outputs to pool delegators, then get their distinct blocks.
        // Two-step: first find top output tx_ids, then get block info.
        let rows = sqlx::query!(
            r#"WITH top_txs AS (
                SELECT DISTINCT ON (tx.block_id)
                    tx.block_id, tx_out.value
                FROM tx_out
                JOIN tx ON tx.id = tx_out.tx_id
                WHERE tx_out.tx_id > $1
                  AND tx_out.stake_address_id IN (
                      SELECT id FROM stake_address WHERE hash_raw = ANY($2::bytea[])
                  )
                ORDER BY tx.block_id, tx_out.value DESC
            )
            SELECT b.slot_no AS "slot!",
                   encode(b.hash, 'hex') AS "hash!",
                   b.block_no AS "block_no!",
                   ph.hash_raw AS "pool_hash?",
                   (SELECT ticker_name FROM off_chain_pool_data opd
                    WHERE opd.pool_id = ph.id ORDER BY opd.id DESC LIMIT 1) AS pool_ticker
            FROM top_txs t
            JOIN block b ON b.id = t.block_id
            JOIN slot_leader sl ON sl.id = b.slot_leader_id
            LEFT JOIN pool_hash ph ON ph.id = sl.pool_hash_id
            ORDER BY t.value DESC
            LIMIT $3"#,
            boundary_tx_id,
            delegator_hash_raws as &[Vec<u8>],
            limit,
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.slot as u64,
                    r.hash,
                    r.block_no as u64,
                    r.pool_hash,
                    r.pool_ticker,
                )
            })
            .collect())
    }
}
