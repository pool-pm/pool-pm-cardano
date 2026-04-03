use futures::TryStreamExt;
use imbl::{hashmap::HashMap, hashset::HashSet};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use tokio::time::Duration;
use url::Url;

use crate::model::{asset_fingerprint, DRep, Pool};

pub struct DbSync {
    db: sqlx::Pool<sqlx::Postgres>,
}

impl DbSync {
    pub async fn new(url: &Url) -> Result<Self, sqlx::Error> {
        let options = PgConnectOptions::from_url(url)?
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
    ) -> Result<Option<(String, sqlx::types::Decimal, Vec<(String, u64)>)>, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT tx_out.id, tx_out.address, tx_out.value
            FROM tx_out
            JOIN tx ON tx.id = tx_out.tx_id
            WHERE tx.hash = $1 AND tx_out.index = $2"#,
            tx_hash,
            index
        )
        .fetch_optional(&self.db)
        .await?;

        let Some(row) = row else { return Ok(None) };

        let ma_rows = sqlx::query!(
            r#"SELECT ident, quantity AS "quantity!" FROM ma_tx_out WHERE tx_out_id = $1"#,
            row.id,
        )
        .fetch_all(&self.db)
        .await?;

        let mut assets = Vec::new();
        if !ma_rows.is_empty() {
            let idents: Vec<i64> = ma_rows.iter().map(|r| r.ident).collect();
            let ma_info = sqlx::query!(
                r#"SELECT id, policy AS "policy!", name AS "name!"
                FROM multi_asset WHERE id = ANY($1)"#,
                &idents
            )
            .fetch_all(&self.db)
            .await?;
            let lookup: std::collections::HashMap<i64, _> = ma_info
                .into_iter()
                .map(|r| (r.id, (r.policy, r.name)))
                .collect();
            for r in &ma_rows {
                if let Some((policy, name)) = lookup.get(&r.ident) {
                    let qty: u64 = r.quantity.try_into().unwrap_or(0);
                    assets.push((asset_fingerprint(policy, name), qty));
                }
            }
        }

        Ok(Some((row.address, row.value, assets)))
    }

    /// Batch-resolve UTXOs. Returns (address, lovelace, assets, unspent).
    /// `unspent` is true when consumed_by_tx_id IS NULL — callers can cache these.
    pub async fn resolve_utxos_batch(
        &self,
        inputs: &[(Vec<u8>, i16)],
    ) -> Result<
        std::collections::HashMap<(Vec<u8>, i16), (String, u64, Vec<(String, u64)>, bool)>,
        sqlx::Error,
    > {
        if inputs.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let hashes: Vec<Vec<u8>> = inputs.iter().map(|(h, _)| h.clone()).collect();
        let indices: Vec<i16> = inputs.iter().map(|(_, i)| *i).collect();
        let rows = sqlx::query!(
            r#"SELECT tx.hash, tx_out.index AS "index!: i16", tx_out.id,
                    tx_out.address, tx_out.value, tx_out.consumed_by_tx_id
            FROM tx_out
            JOIN tx ON tx.id = tx_out.tx_id
            WHERE (tx.hash, tx_out.index) IN (SELECT * FROM UNNEST($1::bytea[], $2::smallint[]))"#,
            &hashes,
            &indices
        )
        .fetch_all(&self.db)
        .await?;

        // Build id→key lookup and initial result map from first query
        let mut id_to_key: std::collections::HashMap<i64, (Vec<u8>, i16)> =
            std::collections::HashMap::with_capacity(rows.len());
        let mut result: std::collections::HashMap<
            (Vec<u8>, i16),
            (String, u64, Vec<(String, u64)>, bool),
        > = std::collections::HashMap::with_capacity(rows.len());
        let mut tx_out_ids: Vec<i64> = Vec::with_capacity(rows.len());

        for r in rows {
            let lovelace: u64 = r.value.try_into().expect("lovelace must fit u64");
            let unspent = r.consumed_by_tx_id.is_none();
            let key = (r.hash, r.index);
            id_to_key.insert(r.id, key.clone());
            tx_out_ids.push(r.id);
            result.insert(key, (r.address, lovelace, vec![], unspent));
        }

        // Fetch assets in two simple indexed lookups (no join for Postgres to misoptimize)
        if !tx_out_ids.is_empty() {
            // Step 1: ma_tx_out by tx_out_id → (tx_out_id, ident)
            let ma_rows = sqlx::query!(
                r#"SELECT tx_out_id, ident, quantity AS "quantity!" FROM ma_tx_out WHERE tx_out_id = ANY($1)"#,
                &tx_out_ids
            )
            .fetch_all(&self.db)
            .await?;

            if !ma_rows.is_empty() {
                // Step 2: multi_asset by id → (id, policy, name)
                let idents: Vec<i64> = ma_rows.iter().map(|r| r.ident).collect();
                let ma_info: std::collections::HashMap<i64, (Vec<u8>, Vec<u8>)> = sqlx::query!(
                    r#"SELECT id, policy AS "policy!", name AS "name!"
                    FROM multi_asset WHERE id = ANY($1)"#,
                    &idents
                )
                .fetch_all(&self.db)
                .await?
                .into_iter()
                .map(|r| (r.id, (r.policy, r.name)))
                .collect();

                for r in &ma_rows {
                    if let Some((policy, name)) = ma_info.get(&r.ident) {
                        if let Some(key) = id_to_key.get(&r.tx_out_id) {
                            if let Some(entry) = result.get_mut(key) {
                                let qty: u64 = r.quantity.try_into().unwrap_or(0);
                                entry.2.push((asset_fingerprint(policy, name), qty));
                            }
                        }
                    }
                }
            }
        }

        Ok(result)
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

    pub async fn utxo_stakes(&self, last_tx_id: i64) -> Result<HashMap<Vec<u8>, i64>, sqlx::Error> {
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

    /// Fetch DRep metadata (given_name) from off-chain vote data.
    /// Returns a map keyed by DRep bytes (tag + raw hash).
    pub async fn drep_metadata(
        &self,
        last_tx_id: i64,
    ) -> Result<HashMap<Vec<u8>, DRep>, sqlx::Error> {
        let mut rows = sqlx::query!(
            r#"SELECT dh.raw AS drep_raw,
                      dh.has_script AS drep_has_script,
                      dd.given_name
            FROM drep_registration dr
            JOIN drep_hash dh ON dh.id = dr.drep_hash_id
            JOIN off_chain_vote_data ovd ON ovd.voting_anchor_id = dr.voting_anchor_id
            JOIN off_chain_vote_drep_data dd ON dd.off_chain_vote_data_id = ovd.id
            WHERE dr.tx_id <= $1
              AND dh.raw IS NOT NULL
              AND dr.id = (
                  SELECT MAX(dr2.id) FROM drep_registration dr2
                  WHERE dr2.drep_hash_id = dr.drep_hash_id AND dr2.tx_id <= $1
              )"#,
            last_tx_id
        )
        .fetch(&self.db);

        let mut dreps: HashMap<Vec<u8>, DRep> = HashMap::new();
        while let Some(row) = rows.try_next().await? {
            let Some(raw) = &row.drep_raw else { continue };
            let tag = if row.drep_has_script { 0x01u8 } else { 0x00 };
            let hash_bytes = [&[tag][..], raw].concat();
            dreps.insert(
                hash_bytes.clone(),
                DRep {
                    hash_bytes,
                    given_name: Some(row.given_name),
                },
            );
        }

        Ok(dreps)
    }

    /// Fetch CIP-68 decimals from reference token datums.
    /// Queries unspent outputs holding label-100 assets and extracts "decimals"
    /// from their inline datum JSONB. Returns (policy_id, asset_name, decimals).
    pub async fn cip68_decimals(
        &self,
        last_tx_id: i64,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>, i32)>, sqlx::Error> {
        // Reference tokens have asset name starting with 000643b0 (CIP-67 label 100).
        // The datum value JSONB is: {"constructor":0,"fields":[{"map":[...]}, ...]}
        // We look for a "decimals" key in the first field's map entries.
        let label_prefix: Vec<u8> = vec![0x00, 0x06, 0x43, 0xb0];
        let rows = sqlx::query!(
            r#"SELECT ma.policy AS "policy!", ma.name AS "name!",
                      entry->'v'->>'int' AS "decimals"
            FROM tx_out
            JOIN ma_tx_out ON ma_tx_out.tx_out_id = tx_out.id
            JOIN multi_asset ma ON ma.id = ma_tx_out.ident
            JOIN datum ON datum.hash = tx_out.data_hash OR datum.id = tx_out.inline_datum_id
            CROSS JOIN LATERAL jsonb_array_elements(
                datum.value->'fields'->0->'map'
            ) AS entry
            WHERE substring(ma.name from 1 for 4) = $2
              AND tx_out.tx_id <= $1
              AND (tx_out.consumed_by_tx_id IS NULL OR tx_out.consumed_by_tx_id > $1)
              AND (entry->'k') @> '{"bytes":"646563696d616c73"}'
              AND (entry->'v'->>'int') IS NOT NULL"#,
            last_tx_id,
            &label_prefix
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let decimals: i32 = r.decimals?.parse().ok()?;
                Some((r.policy, r.name, decimals))
            })
            .collect())
    }

    /// Find the most recent block at or before the given slot.
    /// Used at startup to find a valid intersection point for backfill.
    /// Fetch all current ADA Handle owners for the given policy ID.
    pub async fn handles(&self, policy: &[u8]) -> Result<Vec<(String, String)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT convert_from(ma.name, 'UTF8') AS "handle!", tx_out.address AS "address!"
            FROM tx_out
            JOIN ma_tx_out ON ma_tx_out.tx_out_id = tx_out.id
            JOIN multi_asset ma ON ma.id = ma_tx_out.ident
            WHERE ma.policy = $1
            AND tx_out.consumed_by_tx_id IS NULL
            AND ma.name != '' AND ma.name IS NOT NULL"#,
            policy
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .filter(|r| !r.handle.is_empty())
            .map(|r| (r.handle, r.address))
            .collect())
    }

    pub async fn boundary_block(&self, boundary_slot: u64) -> Option<(u64, String)> {
        let row = sqlx::query!(
            r#"SELECT slot_no AS "slot!", encode(hash, 'hex') AS "hash!"
            FROM block WHERE slot_no <= $1 ORDER BY slot_no DESC LIMIT 1"#,
            boundary_slot as i64
        )
        .fetch_optional(&self.db)
        .await
        .ok()??;
        Some((row.slot as u64, row.hash))
    }
}
