use futures::TryStreamExt;
use imbl::{hashmap::HashMap, hashset::HashSet};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use tokio::time::Duration;
use url::Url;

use crate::model::Pool;

pub struct DelegationRow {
    pub slot: u64,
    pub block_hash: String,
    pub block_no: u64,
    pub block_pool_hash: Option<Vec<u8>>,
    pub block_pool_ticker: Option<String>,
    pub tx_hash: String,
    pub stake_address: String,
    pub stake_cred: Vec<u8>,
    pub from_pool_hash: Option<Vec<u8>>,
    pub from_ticker: Option<String>,
    pub to_pool_hash: Option<Vec<u8>>,
    pub to_ticker: Option<String>,
}

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

    /// All blocks minted by a pool since a slot boundary.
    /// Returns (slot, block_hash_hex, block_number), newest first.
    pub async fn pool_blocks_since(
        &self,
        pool_hash: &[u8],
        boundary_slot: i64,
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
            r#"SELECT slot_no AS "slot!", encode(hash, 'hex') AS "hash!", block_no AS "block_no!"
            FROM block
            WHERE slot_leader_id = $1 AND slot_no > $2
            ORDER BY id DESC"#,
            leader_id,
            boundary_slot,
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| (r.slot as u64, r.hash, r.block_no as u64))
            .collect())
    }

    /// Delegation changes TO or FROM a pool since a slot.
    /// Returns per-delegation rows with block info, stake address, from/to pool.
    pub async fn pool_delegations_since(
        &self,
        pool_hash: &[u8],
        boundary_slot: i64,
    ) -> Result<Vec<DelegationRow>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"WITH changes AS (
                -- Delegations TO the pool
                SELECT d.addr_id, d.pool_hash_id AS to_pool_id, d.tx_id,
                       prev_d.pool_hash_id AS from_pool_id
                FROM delegation d
                JOIN pool_hash ph ON ph.id = d.pool_hash_id
                LEFT JOIN LATERAL (
                    SELECT pool_hash_id FROM delegation d_prev
                    WHERE d_prev.addr_id = d.addr_id AND d_prev.id < d.id
                    ORDER BY d_prev.id DESC LIMIT 1
                ) prev_d ON TRUE
                JOIN tx ON tx.id = d.tx_id
                JOIN block b ON b.id = tx.block_id
                WHERE ph.hash_raw = $1 AND b.slot_no > $2
                  AND (prev_d.pool_hash_id IS NULL OR prev_d.pool_hash_id != d.pool_hash_id)

                UNION ALL

                -- Delegations FROM the pool (to a different pool)
                SELECT d.addr_id, d.pool_hash_id AS to_pool_id, d.tx_id,
                       prev_d.pool_hash_id AS from_pool_id
                FROM delegation d
                JOIN pool_hash ph_new ON ph_new.id = d.pool_hash_id
                LEFT JOIN LATERAL (
                    SELECT pool_hash_id FROM delegation d_prev
                    WHERE d_prev.addr_id = d.addr_id AND d_prev.id < d.id
                    ORDER BY d_prev.id DESC LIMIT 1
                ) prev_d ON TRUE
                JOIN pool_hash ph_prev ON ph_prev.id = prev_d.pool_hash_id
                JOIN tx ON tx.id = d.tx_id
                JOIN block b ON b.id = tx.block_id
                WHERE ph_prev.hash_raw = $1 AND b.slot_no > $2
                  AND ph_new.hash_raw != $1
            )
            SELECT b.slot_no AS "slot!",
                   encode(b.hash, 'hex') AS "block_hash!",
                   b.block_no AS "block_no!",
                   sl_ph.hash_raw AS "block_pool_hash?",
                   (SELECT ticker_name FROM off_chain_pool_data
                    WHERE pool_id = sl_ph.id ORDER BY id DESC LIMIT 1) AS block_pool_ticker,
                   encode(tx.hash, 'hex') AS "tx_hash!",
                   sa.view AS "stake_address!",
                   sa.hash_raw AS "stake_hash_raw!",
                   from_ph.hash_raw AS "from_pool_hash?",
                   (SELECT ticker_name FROM off_chain_pool_data
                    WHERE pool_id = from_ph.id ORDER BY id DESC LIMIT 1) AS from_ticker,
                   to_ph.hash_raw AS "to_pool_hash?",
                   (SELECT ticker_name FROM off_chain_pool_data
                    WHERE pool_id = to_ph.id ORDER BY id DESC LIMIT 1) AS to_ticker
            FROM changes c
            JOIN stake_address sa ON sa.id = c.addr_id
            JOIN tx ON tx.id = c.tx_id
            JOIN block b ON b.id = tx.block_id
            JOIN slot_leader sl ON sl.id = b.slot_leader_id
            LEFT JOIN pool_hash sl_ph ON sl_ph.id = sl.pool_hash_id
            LEFT JOIN pool_hash from_ph ON from_ph.id = c.from_pool_id
            LEFT JOIN pool_hash to_ph ON to_ph.id = c.to_pool_id
            ORDER BY b.slot_no"#,
            pool_hash,
            boundary_slot,
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| DelegationRow {
                slot: r.slot as u64,
                block_hash: r.block_hash,
                block_no: r.block_no as u64,
                block_pool_hash: r.block_pool_hash,
                block_pool_ticker: r.block_pool_ticker,
                tx_hash: r.tx_hash,
                stake_address: r.stake_address,
                stake_cred: r.stake_hash_raw[1..].to_vec(),
                from_pool_hash: r.from_pool_hash,
                from_ticker: r.from_ticker,
                to_pool_hash: r.to_pool_hash,
                to_ticker: r.to_ticker,
            })
            .collect())
    }
}
