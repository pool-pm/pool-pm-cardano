use futures::TryStreamExt;
use imbl::{hashmap::HashMap, hashset::HashSet};
use pallas::ledger::addresses::Address;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use tokio::time::Duration;
use url::Url;

use crate::model::{parse_handle_name, DRep, Pool, CIP67_LABEL_222};

/// Raw on-chain bytes of a bech32 payment address — db-sync's `address.raw`
/// (same serialization pallas produces). `None` for Byron / non-bech32 input
/// (those never appear in feeds). With `use_address_table`, `tx_out` no longer
/// carries the address; we resolve it to an `address.id` via the `idx_address_raw`
/// hash index and filter `tx_out.address_id` instead.
fn address_raw(address: &str) -> Option<Vec<u8>> {
    Address::from_bech32(address).ok().map(|a| a.to_vec())
}

/// A resolved UTXO: `(address, lovelace, assets, unspent)`, keyed by
/// `(tx_hash, output_index)`. Returned by `resolve_utxos_batch`.
type ResolvedUtxos =
    std::collections::HashMap<(Vec<u8>, i16), (String, u64, crate::model::PolicyAssets, bool)>;

/// Add one `(policy, name, quantity)` token to a [`crate::model::PolicyAssets`],
/// grouping under its policy (stored once, first-seen order) — the Pallas-style nested
/// shape. The linear policy scan is fine: a single UTXO holds only a handful of
/// policies. Used by the UTXO-resolution queries (one row per token).
fn push_policy_asset(
    assets: &mut crate::model::PolicyAssets,
    policy: &[u8],
    name: Vec<u8>,
    qty: u64,
) {
    match assets.iter_mut().find(|(p, _)| p == policy) {
        Some((_, tokens)) => tokens.push((name, qty)),
        None => assets.push((policy.to_vec(), vec![(name, qty)])),
    }
}

/// Statements slower than this are logged at WARN. Sized to the ~100 ms
/// per-query target with headroom: anything over 1s in steady state signals a
/// regression. The reset scans are deliberately slower and trip this during
/// init (as they did at the previous 15s threshold).
const SLOW_QUERY_THRESHOLD: Duration = Duration::from_secs(1);

/// `*_recent_blocks` fetches at most `limit * TOUCH_FACTOR` of a subject's most-recent
/// `tx_out` touches per side (produced/consumed) before grouping to blocks. It must be
/// large enough to span `limit` distinct blocks so the caller's `has_more = (returned ==
/// limit)` pagination stays correct; 256 covers up to ~256 touches/block while keeping
/// each side an index-only top-K (a few ms). A subject sustaining more than that over
/// `limit` blocks would just paginate in smaller chunks — no correctness loss.
const TOUCH_FACTOR: i64 = 256;

/// Cheap to clone — `sqlx::Pool` is internally `Arc`-shared, so a `DbSync`
/// clone reuses the same underlying connection pool. Cloning hands a db
/// handle to a caller that wants to run queries without holding a lock on the
/// owner of the original `DbSync`.
#[derive(Clone)]
pub struct DbSync {
    db: sqlx::Pool<sqlx::Postgres>,
}

/// One delegation or deregistration event in an address's history, with the
/// previous target resolved via `LAG`. `to`/`from` are pool hashes (28 bytes) for
/// pool history, or DRep bytes (tag+hash, or 0x02/0x03 sentinels) for DRep history.
/// `to: None` = a deregistration; `from: None` = no prior delegation (first ever, or
/// the previous event was a deregistration).
pub struct DelegationEvent {
    pub tx_hash: String,
    pub to: Option<Vec<u8>>,
    pub from: Option<Vec<u8>>,
}

/// Build DRep bytes (tag+hash, or 0x02/0x03 for the predefined DReps) from a
/// `drep_hash` row's `raw`/`has_script`/`view`. `None` if `view` is absent (a
/// deregistration row). Mirrors the encoding in `drep_delegations`.
fn drep_bytes(raw: Option<&[u8]>, has_script: Option<bool>, view: Option<&str>) -> Option<Vec<u8>> {
    let view = view?;
    if view.starts_with("drep_always_abstain") {
        Some(vec![0x02])
    } else if view.starts_with("drep_always_no_confidence") {
        Some(vec![0x03])
    } else {
        let raw = raw?;
        let tag = if has_script.unwrap_or(false) {
            0x01u8
        } else {
            0x00
        };
        Some([&[tag][..], raw].concat())
    }
}

impl DbSync {
    pub async fn new(url: &Url) -> Result<Self, sqlx::Error> {
        let options = PgConnectOptions::from_url(url)?
            .log_slow_statements(log::LevelFilter::Warn, SLOW_QUERY_THRESHOLD);

        let db = PgPoolOptions::new()
            .max_connections(8)
            // Per-address point queries (current-holdings lookups via the partial
            // unconsumed index) are planned at ~125k cost — just over Postgres's
            // default `jit_above_cost` of 100k — so JIT fires and adds ~80 ms of
            // compile time to a query that runs in ~5 ms. Raise the threshold
            // above those point queries while staying far below the million-cost
            // reset scans (which still benefit from JIT). Scoped to this pool, so
            // db-sync's own connections are unaffected.
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    sqlx::query("SET jit_above_cost = 200000")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
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

    /// Recent blocks containing a transaction that touches the given stake address
    /// (`hash_raw` is the full 29-byte reward address = db-sync
    /// `stake_address.hash_raw`): either an output paid to it, or one of its
    /// outputs being consumed. Newest-first with `slot_no < before_slot` (pass
    /// `i64::MAX` for the first page), capped at `limit`. Returns
    /// (slot_no, block_hash_hex, block_no, epoch). Used to drive feed replay and its
    /// infinite-scroll pagination; an unknown address yields an empty result.
    pub async fn stake_recent_blocks(
        &self,
        hash_raw: &[u8],
        before_slot: i64,
        limit: i64,
    ) -> Result<Vec<(u64, String, u64, u64)>, sqlx::Error> {
        let Some(stake_id) =
            sqlx::query_scalar!("SELECT id FROM stake_address WHERE hash_raw = $1", hash_raw)
                .fetch_optional(&self.db)
                .await?
        else {
            return Ok(Vec::new());
        };
        let rows = sqlx::query!(
            // `bnd` = tx_id boundary for "blocks strictly older than before_slot" (the first
            // tx of the first block at/after it; i64::MAX on the first page). Each UNION arm
            // is an early-LIMIT top-K backward scan by tx_id — receives/sends on the tx_out
            // composites, withdrawals via db-sync's idx_withdrawal_tx_id / idx_withdrawal_addr_id
            // — so only ~3K touches are grouped to blocks, not the subject's whole history.
            // No amount filter on withdrawals: zero-amount script-validation withdrawals are
            // legitimate and shown (and filtering them would defeat the top-K on a dense
            // all-zero reward account).
            r#"WITH bnd AS MATERIALIZED (
                SELECT COALESCE(MIN(t.id), 9223372036854775807) AS max_tx_id
                FROM tx t
                WHERE t.block_id =
                    (SELECT id FROM block WHERE slot_no >= $2 ORDER BY slot_no ASC LIMIT 1)
            )
            SELECT b.slot_no AS "slot_no!", b.hash, b.block_no AS "block_no!",
                   b.epoch_no AS "epoch_no!"
            FROM (
                (SELECT tx_id FROM tx_out
                   WHERE stake_address_id = $1 AND tx_id < (SELECT max_tx_id FROM bnd)
                   ORDER BY tx_id DESC LIMIT $3)
                UNION
                (SELECT consumed_by_tx_id AS tx_id FROM tx_out
                   WHERE stake_address_id = $1 AND consumed_by_tx_id IS NOT NULL
                     AND consumed_by_tx_id < (SELECT max_tx_id FROM bnd)
                   ORDER BY consumed_by_tx_id DESC LIMIT $3)
                UNION
                (SELECT tx_id FROM withdrawal
                   WHERE addr_id = $1 AND tx_id < (SELECT max_tx_id FROM bnd)
                   ORDER BY tx_id DESC LIMIT $3)
            ) t
            JOIN tx ON tx.id = t.tx_id
            JOIN block b ON b.id = tx.block_id
            GROUP BY b.id, b.slot_no, b.hash, b.block_no, b.epoch_no
            ORDER BY b.slot_no DESC
            LIMIT $4"#,
            stake_id,
            before_slot,
            limit.saturating_mul(TOUCH_FACTOR),
            limit
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.slot_no as u64,
                    hex::encode(r.hash),
                    r.block_no as u64,
                    r.epoch_no as u64,
                )
            })
            .collect())
    }

    /// Recent blocks containing a transaction that touches the given payment
    /// address (an output paid to it, or one of its outputs consumed). Newest-first
    /// with `slot_no < before_slot` (pass `i64::MAX` for the first page), capped at
    /// `limit`. Returns (slot_no, block_hash_hex, block_no, epoch). Mirrors
    /// `stake_recent_blocks` but matches the exact address.
    pub async fn address_recent_blocks(
        &self,
        address: &str,
        before_slot: i64,
        limit: i64,
    ) -> Result<Vec<(u64, String, u64, u64)>, sqlx::Error> {
        let Some(raw) = address_raw(address) else {
            return Ok(Vec::new());
        };
        let Some(addr_id) = sqlx::query_scalar!("SELECT id FROM address WHERE raw = $1", raw)
            .fetch_optional(&self.db)
            .await?
        else {
            return Ok(Vec::new());
        };
        // Same shape as `stake_recent_blocks`; see its comment for the bnd/early-LIMIT logic.
        let rows = sqlx::query!(
            r#"WITH bnd AS MATERIALIZED (
                SELECT COALESCE(MIN(t.id), 9223372036854775807) AS max_tx_id
                FROM tx t
                WHERE t.block_id =
                    (SELECT id FROM block WHERE slot_no >= $2 ORDER BY slot_no ASC LIMIT 1)
            )
            SELECT b.slot_no AS "slot_no!", b.hash, b.block_no AS "block_no!",
                   b.epoch_no AS "epoch_no!"
            FROM (
                (SELECT tx_id FROM tx_out
                   WHERE address_id = $1 AND tx_id < (SELECT max_tx_id FROM bnd)
                   ORDER BY tx_id DESC LIMIT $3)
                UNION
                (SELECT consumed_by_tx_id AS tx_id FROM tx_out
                   WHERE address_id = $1 AND consumed_by_tx_id IS NOT NULL
                     AND consumed_by_tx_id < (SELECT max_tx_id FROM bnd)
                   ORDER BY consumed_by_tx_id DESC LIMIT $3)
            ) t
            JOIN tx ON tx.id = t.tx_id
            JOIN block b ON b.id = tx.block_id
            GROUP BY b.id, b.slot_no, b.hash, b.block_no, b.epoch_no
            ORDER BY b.slot_no DESC
            LIMIT $4"#,
            addr_id,
            before_slot,
            limit.saturating_mul(TOUCH_FACTOR),
            limit
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.slot_no as u64,
                    hex::encode(r.hash),
                    r.block_no as u64,
                    r.epoch_no as u64,
                )
            })
            .collect())
    }

    pub async fn resolve_utxo(
        &self,
        tx_hash: &[u8],
        index: i16,
    ) -> Result<Option<(String, sqlx::types::Decimal, crate::model::PolicyAssets)>, sqlx::Error>
    {
        let row = sqlx::query!(
            r#"SELECT tx_out.id, a.address, tx_out.value
            FROM tx_out
            JOIN tx ON tx.id = tx_out.tx_id
            JOIN address a ON a.id = tx_out.address_id
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

        let mut assets: crate::model::PolicyAssets = Vec::new();
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
                    push_policy_asset(&mut assets, policy, name.clone(), qty);
                }
            }
        }

        Ok(Some((row.address, row.value, assets)))
    }

    /// Chain facts for the standalone asset page, by CIP-14 `fingerprint`: the policy id
    /// (hex), the minted supply (`Σ ma_tx_mint.quantity` as a string — can exceed i64),
    /// and the first/last mint timestamps (unix seconds; a token minted across several
    /// txs spans a range). `None` if the fingerprint isn't a known asset.
    ///
    /// `multi_asset` maps the public identity → db-sync's internal `ident`; the supply
    /// and mint times then come from `ma_tx_mint` keyed on that `ident`. Wants
    /// `idx_multi_asset_fingerprint` (else a ~270 ms parallel seq scan of ~11M rows) and
    /// `idx_ma_tx_mint_ident` (else a ~1.5 s seq scan of ~19M rows) — neither exists in a
    /// default db-sync.
    pub async fn asset_chain_info(
        &self,
        fingerprint: &str,
    ) -> Result<Option<(String, Vec<u8>, Option<String>, Option<i64>, Option<i64>)>, sqlx::Error>
    {
        let row = sqlx::query!(
            r#"SELECT ma.policy AS "policy!", ma.name AS "name!", agg.supply, agg.first_mint, agg.last_mint
               FROM multi_asset ma
               LEFT JOIN LATERAL (
                   SELECT SUM(m.quantity)::text AS supply,
                          EXTRACT(EPOCH FROM MIN(b.time))::bigint AS first_mint,
                          EXTRACT(EPOCH FROM MAX(b.time))::bigint AS last_mint
                   FROM ma_tx_mint m
                   JOIN tx t ON t.id = m.tx_id
                   JOIN block b ON b.id = t.block_id
                   WHERE m.ident = ma.id
               ) agg ON true
               WHERE ma.fingerprint = $1"#,
            fingerprint
        )
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|r| {
            (
                hex::encode(r.policy),
                r.name,
                r.supply,
                r.first_mint,
                r.last_mint,
            )
        }))
    }

    /// All assets of a policy, newest-first-minted first, keyset-paginated on
    /// `multi_asset.id` (a bigserial assigned at first sighting, so a higher id
    /// means more recently first minted). `cursor` is the last id of the previous
    /// page (None for the first page). Querying `multi_asset` alone (unique on
    /// (policy, name)) yields each asset exactly once — no `ma_tx_mint` join, so
    /// re-mints/burns cannot produce duplicate rows. Returns (id, fingerprint,
    /// name); `fingerprint` is db-sync's canonical CIP-14 value.
    ///
    /// Robust to concurrent mints: new assets always get ids above the current
    /// max and we page strictly downward (`id < cursor`), so a mint during an
    /// in-progress paging session can never shift, skip, or duplicate a page — it
    /// only appears on reload. "Most recently minted" therefore means most
    /// recently *first* minted: a burned-then-reminted asset keeps its original
    /// id ordering.
    pub async fn assets_by_policy(
        &self,
        policy: &[u8],
        cursor: Option<i64>,
        limit: i64,
        ascending: bool,
    ) -> Result<Vec<(i64, String, Vec<u8>)>, sqlx::Error> {
        // MATERIALIZED CTE inhibits the planner from pushing the outer
        // `ORDER BY id DESC LIMIT N` into a backward pkey scan that filters by
        // policy — that plan walks every newer multi_asset row before finding
        // matches in an old policy (e.g. SpaceBudz: minted years ago → low ids
        // → millions of rows scanned for 60 hits). Materializing forces the
        // bitmap-scan via `unique_multi_asset (policy, name)` first, then
        // top-N sort over the small per-policy result.
        //
        // `ascending` (oldest first) pages strictly upward (`id > cursor`,
        // `ORDER BY id ASC`); the default descending pages downward
        // (`id < cursor`, `ORDER BY id DESC`). Both directions are a top-N sort
        // over the materialized per-policy set, so the cursor stays stable
        // against concurrent mints exactly as documented above.
        let rows = if ascending {
            sqlx::query!(
                r#"WITH filtered AS MATERIALIZED (
                    SELECT id, fingerprint AS "fingerprint!", name AS "name!"
                    FROM multi_asset
                    WHERE policy = $1 AND ($2::bigint IS NULL OR id > $2)
                    AND substring(name from 1 for 4) != '\x000643b0'
                )
                SELECT id, "fingerprint!", "name!" FROM filtered
                ORDER BY id ASC
                LIMIT $3"#,
                policy,
                cursor,
                limit
            )
            .fetch_all(&self.db)
            .await?
            .into_iter()
            .map(|r| (r.id, r.fingerprint, r.name))
            .collect()
        } else {
            sqlx::query!(
                r#"WITH filtered AS MATERIALIZED (
                    SELECT id, fingerprint AS "fingerprint!", name AS "name!"
                    FROM multi_asset
                    WHERE policy = $1 AND ($2::bigint IS NULL OR id < $2)
                    -- Hide CIP-68 reference NFTs (CIP-67 label 100); the (222)
                    -- user token renders the same image, so they'd otherwise show
                    -- as duplicates.
                    AND substring(name from 1 for 4) != '\x000643b0'
                )
                SELECT id, "fingerprint!", "name!" FROM filtered
                ORDER BY id DESC
                LIMIT $3"#,
                policy,
                cursor,
                limit
            )
            .fetch_all(&self.db)
            .await?
            .into_iter()
            .map(|r| (r.id, r.fingerprint, r.name))
            .collect()
        };
        Ok(rows)
    }

    /// Batch-resolve UTXOs. Returns (address, lovelace, assets, unspent).
    /// `unspent` is true when consumed_by_tx_id IS NULL — callers can cache these.
    pub async fn resolve_utxos_batch(
        &self,
        inputs: &[(Vec<u8>, i16)],
    ) -> Result<ResolvedUtxos, sqlx::Error> {
        if inputs.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let hashes: Vec<Vec<u8>> = inputs.iter().map(|(h, _)| h.clone()).collect();
        let indices: Vec<i16> = inputs.iter().map(|(_, i)| *i).collect();
        let rows = sqlx::query!(
            r#"SELECT tx.hash, tx_out.index AS "index!: i16", tx_out.id,
                    a.address, tx_out.value, tx_out.consumed_by_tx_id
            FROM tx_out
            JOIN tx ON tx.id = tx_out.tx_id
            JOIN address a ON a.id = tx_out.address_id
            WHERE (tx.hash, tx_out.index) IN (SELECT * FROM UNNEST($1::bytea[], $2::smallint[]))"#,
            &hashes,
            &indices
        )
        .fetch_all(&self.db)
        .await?;

        // Build id→key lookup and initial result map from first query
        let mut id_to_key: std::collections::HashMap<i64, (Vec<u8>, i16)> =
            std::collections::HashMap::with_capacity(rows.len());
        let mut result: ResolvedUtxos = std::collections::HashMap::with_capacity(rows.len());
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
                                push_policy_asset(&mut entry.2, policy, name.clone(), qty);
                            }
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    pub async fn pools(
        &self,
        last_tx_id: i64,
        slot: i64,
    ) -> Result<HashMap<String, Pool>, sqlx::Error> {
        Ok(sqlx::query_as!(
            Pool,
            r#"SELECT DISTINCT ON (hash_raw)
            hash_raw, pledge, margin, fixed_cost,
            (SELECT ticker_name FROM off_chain_pool_data WHERE pool_id = pool_hash.id ORDER BY id DESC LIMIT 1) as ticker,
            -- Pending retirement: latest retire announced *after* this (latest)
            -- registration — a re-registration cancels it — else NULL (active).
            (SELECT pr.retiring_epoch::bigint FROM pool_retire pr
             WHERE pr.hash_id = pool_hash.id
               AND pr.announced_tx_id > pool_update.registered_tx_id
               AND pr.announced_tx_id <= $1
             ORDER BY pr.announced_tx_id DESC LIMIT 1) as retiring_epoch,
            -- Lifetime blocks minted as of the reset slot (one grouped pass).
            COALESCE(bc.cnt, 0)::bigint AS "blocks!"
            FROM pool_update
            JOIN pool_hash ON pool_hash.id=hash_id
            LEFT JOIN (
                SELECT sl.pool_hash_id, COUNT(*) AS cnt
                FROM block b JOIN slot_leader sl ON sl.id = b.slot_leader_id
                WHERE b.slot_no <= $2 AND sl.pool_hash_id IS NOT NULL
                GROUP BY sl.pool_hash_id
            ) bc ON bc.pool_hash_id = pool_hash.id
            WHERE registered_tx_id <= $1
            GROUP BY hash_raw, pool_update.id, pool_hash.id, bc.cnt
            ORDER BY hash_raw, pool_update.id DESC"#,
            last_tx_id,
            slot
        )
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(|pool| (hex::encode(&pool.hash_raw), pool))
        .collect())
    }

    /// Lifetime blocks minted per pool as of `slot`, `hash_raw -> count`. Used by
    /// `populate_block_counts` to backfill `Pool::blocks` when resuming from a pre-field
    /// snapshot (`reset` gets the count inline via the `pools` query). Bounded by `slot`
    /// so it matches the snapshot point and blocks applied afterwards aren't double-counted.
    pub async fn pool_block_counts(&self, slot: i64) -> Result<HashMap<Vec<u8>, i64>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT ph.hash_raw AS "hash_raw!", COUNT(*) AS "count!"
               FROM block b
               JOIN slot_leader sl ON sl.id = b.slot_leader_id
               JOIN pool_hash ph ON ph.id = sl.pool_hash_id
               WHERE b.slot_no <= $1
               GROUP BY ph.hash_raw"#,
            slot
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|r| (r.hash_raw, r.count)).collect())
    }

    /// Pools with a pending (un-cancelled) retirement as of `last_tx_id`, as
    /// `(hash_raw, retiring_epoch)` — the latest retirement announced *after* the pool's
    /// latest registration (a re-registration cancels it). Used to backfill
    /// `Pool::retiring_epoch` when resuming from a snapshot saved before the field
    /// existed; thereafter `apply_block` maintains it.
    pub async fn pending_pool_retirements(
        &self,
        last_tx_id: i64,
    ) -> Result<Vec<(Vec<u8>, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT ON (ph.hash_raw)
                      ph.hash_raw AS "hash_raw!", pr.retiring_epoch::bigint AS "retiring_epoch!"
               FROM pool_retire pr
               JOIN pool_hash ph ON ph.id = pr.hash_id
               WHERE pr.announced_tx_id <= $1
                 AND pr.announced_tx_id > (
                     SELECT COALESCE(MAX(pu.registered_tx_id), 0) FROM pool_update pu
                     WHERE pu.hash_id = pr.hash_id AND pu.registered_tx_id <= $1
                 )
               ORDER BY ph.hash_raw, pr.announced_tx_id DESC"#,
            last_tx_id
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.hash_raw, r.retiring_epoch))
            .collect())
    }

    /// Latest `ada_pots` `(reserves, stakeable)` in lovelace.
    /// - `reserves`: not-yet-minted ADA; circulating supply = max supply − reserves.
    /// - `stakeable`: ADA that can be delegated = `utxo + rewards + fees`, i.e. supply
    ///   minus everything locked in protocol pots (reserves, treasury, deposits). This
    ///   is the % staked denominator, so the ratio → 100% when all of it is delegated.
    ///
    /// Both settle at epoch boundaries, so the caller caches them. Returns `(0, 0)` if
    /// `ada_pots` is empty (pre-Shelley).
    pub async fn reserves_and_stakeable(&self) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT reserves::bigint AS "reserves!",
                      (utxo + rewards + fees)::bigint AS "stakeable!"
               FROM ada_pots ORDER BY id DESC LIMIT 1"#
        )
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|r| (r.reserves, r.stakeable)).unwrap_or((0, 0)))
    }

    /// Count of currently-active (not expired) DReps. db-sync's `drep_distr` carries
    /// `active_until` — the epoch through which each DRep stays active, with the
    /// `drepActivity` + dormancy math already applied — so a DRep is active when
    /// `active_until >= current_epoch`. The predefined Always-Abstain / No-Confidence
    /// have a NULL `active_until` (never expire), and are excluded as not real DReps.
    /// Reads the latest distribution epoch present.
    pub async fn active_drep_count(&self, current_epoch: i64) -> Result<i64, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT COUNT(*)::bigint AS "count!"
               FROM drep_distr dd
               WHERE dd.epoch_no = (SELECT MAX(epoch_no) FROM drep_distr)
                 AND dd.active_until IS NOT NULL
                 AND dd.active_until >= $1"#,
            current_epoch as i32
        )
        .fetch_one(&self.db)
        .await?;
        Ok(row.count)
    }

    /// Each DRep's `active_until` from the latest `drep_distr`, as
    /// `(drep_hash.raw, has_script, active_until)`. Refreshes `DRep::active_until` at
    /// epoch boundaries (and the resume backfill); the caller rebuilds the tagged key
    /// (`[has_script ? 0x01 : 0x00] ++ raw`) to match the `dreps` map. Only rows with a
    /// non-NULL `active_until` (real DReps) are returned.
    pub async fn drep_active_until(&self) -> Result<Vec<(Vec<u8>, bool, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT dh.raw AS "raw!", dh.has_script AS "has_script!",
                      dd.active_until::bigint AS "active_until!"
               FROM drep_distr dd
               JOIN drep_hash dh ON dh.id = dd.hash_id
               WHERE dd.epoch_no = (SELECT MAX(epoch_no) FROM drep_distr)
                 AND dd.active_until IS NOT NULL
                 AND dh.raw IS NOT NULL"#
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.raw, r.has_script, r.active_until))
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
        // The deregistration anti-join is pushed *inside* the DISTINCT ON subquery
        // (not applied after it): filtering out delegations that have a later
        // deregistration before picking the latest per addr_id yields the same
        // active delegation, but lets the planner use a Merge Anti Join (delegation
        // ⋈ stake_deregistration in addr_id order) + Incremental Sort instead of a
        // full sort of all ~3.5M delegations (reset 28s → ~7s).
        let mut rows = sqlx::query!(
            r#"SELECT stake_address.hash_raw as stake_address, pool_hash.hash_raw as pool_id FROM
                (SELECT DISTINCT ON (addr_id) *
                    FROM delegation d
                    WHERE d.tx_id <= $1
                    AND NOT EXISTS
                        (SELECT TRUE
                            FROM stake_deregistration sd
                            WHERE sd.tx_id <= $1
                            AND sd.addr_id = d.addr_id
                            AND sd.tx_id >= d.tx_id
                        )
                    ORDER BY addr_id, id DESC
            ) delegation
            JOIN stake_address ON stake_address.id = delegation.addr_id
            JOIN pool_hash ON pool_hash.id = delegation.pool_hash_id"#,
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

    /// Per-address balance at `last_tx_id`, from one grouped scan of unconsumed
    /// UTXOs — expensive on mainnet (paid once on cold reset / first run after
    /// upgrade); see `populate_address_balances`. Returns `(bech32 address,
    /// lovelace)`; addresses are returned bech32-encoded (the caller parses to
    /// bytes and skips Byron / non-bech32 ones — they don't appear in feeds).
    pub async fn address_balances(
        &self,
        last_tx_id: i64,
    ) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT a.address AS "address!", b.balance AS "balance!"
            FROM (
                SELECT address_id, SUM(value)::bigint AS balance
                FROM tx_out
                WHERE tx_id <= $1
                  AND (consumed_by_tx_id IS NULL OR consumed_by_tx_id > $1)
                GROUP BY address_id
            ) b
            JOIN address a ON a.id = b.address_id"#,
            last_tx_id
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.address, r.balance)).collect())
    }

    /// Stream every address's current (unspent as-of `last_tx_id`) multi-asset
    /// holdings as `(bech32 address, policy bytes, name bytes, unspent-UTXO count)`,
    /// ordered by address so the caller can group per address, invoking `f` once per
    /// token. The cold-start populate for the global [`BlockSnapshot::asset_holdings`]
    /// map (warm resume deserializes instead). Heavy — a full join of the unspent
    /// `tx_out` set with `ma_tx_out` (~15M output rows); streamed (`fetch`, not
    /// `fetch_all`) so the result never materializes as one giant `Vec`. Counts per
    /// `(address_id, ident)` in a MATERIALIZED CTE on integer keys, then resolves the
    /// text address plus the asset's policy/name (binary — the map keys, fingerprint
    /// derived on demand). `c` is the summed held quantity (not a UTXO count), as **text**:
    /// a token's per-address total can exceed i64, and a `::bigint` cast throws "bigint out
    /// of range", so the caller parses to u64 (saturating).
    pub async fn asset_holdings_for_each<F: FnMut(String, Vec<u8>, Vec<u8>, String, i64)>(
        &self,
        last_tx_id: i64,
        mut f: F,
    ) -> Result<(), sqlx::Error> {
        let mut stream = sqlx::query!(
            r#"WITH held AS MATERIALIZED (
                SELECT o.address_id, m.ident, SUM(m.quantity)::text AS c
                FROM tx_out o
                JOIN ma_tx_out m ON m.tx_out_id = o.id
                WHERE o.tx_id <= $1
                  AND (o.consumed_by_tx_id IS NULL OR o.consumed_by_tx_id > $1)
                GROUP BY o.address_id, m.ident
            )
            SELECT a.address AS "address!", ma.policy AS "policy!", ma.name AS "name!", held.c AS "count!", held.ident AS "ident!"
            FROM held
            JOIN address a ON a.id = held.address_id
            JOIN multi_asset ma ON ma.id = held.ident
            ORDER BY held.address_id"#,
            last_tx_id
        )
        .fetch(&self.db);
        while let Some(r) = stream.try_next().await? {
            f(r.address, r.policy, r.name, r.count, r.ident);
        }
        Ok(())
    }

    /// First-mint time (unix seconds) per asset `ident` (`multi_asset.id`), aggregated from
    /// `ma_tx_mint → tx → block`. Streamed (`fetch`) so the ~11M-row result never materializes
    /// as one `Vec`. The heavy cold-start join (~minutes over ~19M mint rows, wants
    /// `idx_ma_tx_mint_ident`); warm resume reads the times from the snapshot instead. Keyed by
    /// `ident` (8 bytes) rather than `(policy, name)` to keep the transient reset map small.
    pub async fn asset_mint_times_for_each<F: FnMut(i64, i64)>(
        &self,
        mut f: F,
    ) -> Result<(), sqlx::Error> {
        let mut stream = sqlx::query!(
            r#"SELECT m.ident AS "ident!", EXTRACT(EPOCH FROM MIN(b.time))::bigint AS "first_mint!"
               FROM ma_tx_mint m
               JOIN tx t ON t.id = m.tx_id
               JOIN block b ON b.id = t.block_id
               GROUP BY m.ident"#
        )
        .fetch(&self.db);
        while let Some(r) = stream.try_next().await? {
            f(r.ident, r.first_mint);
        }
        Ok(())
    }

    pub async fn rewards(
        &self,
        current_epoch: u64,
        last_tx_id: i64,
    ) -> Result<HashMap<Vec<u8>, i64>, sqlx::Error> {
        // Aggregate by the integer `addr_id` BEFORE joining `stake_address`.
        // Grouping the ~437M-row union by an int collapses it to ~5M groups in
        // memory; joining `stake_address` then runs on those via the PK index.
        // Grouping by the 38-byte `hash_raw` instead (i.e. joining first) forces a
        // 437M-row hash join + a disk-spilling group-by (EXPLAIN ~7x costlier).
        // `addr_id` ↔ `hash_raw` is 1:1, so the result is identical.
        let mut rows = sqlx::query!(
            r#"SELECT sa.hash_raw AS stake_address, t.net AS "net!"
            FROM (
                SELECT addr_id, SUM(amount)::bigint AS net
                FROM (
                    SELECT addr_id, amount FROM reward WHERE spendable_epoch <= $1
                    UNION ALL
                    SELECT addr_id, amount FROM reward_rest WHERE spendable_epoch <= $1
                    UNION ALL
                    SELECT addr_id, -amount FROM withdrawal WHERE tx_id <= $2
                ) u
                GROUP BY addr_id
            ) t
            JOIN stake_address sa ON sa.id = t.addr_id"#,
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
        // Deregistration anti-join pushed inside the DISTINCT ON subquery — see
        // `pool_delegations` for the rationale (Merge Anti Join + Incremental Sort
        // instead of a full sort of all delegation_vote rows).
        let mut rows = sqlx::query!(
            r#"SELECT stake_address.hash_raw as stake_address,
                drep_hash.raw as drep_raw,
                drep_hash.has_script as drep_has_script,
                drep_hash.view as drep_view
            FROM
                (SELECT DISTINCT ON (addr_id) *
                    FROM delegation_vote dv
                    WHERE dv.tx_id <= $1
                    AND NOT EXISTS
                        (SELECT TRUE
                            FROM stake_deregistration sd
                            WHERE sd.tx_id <= $1
                            AND sd.addr_id = dv.addr_id
                            AND sd.tx_id >= dv.tx_id
                        )
                    ORDER BY addr_id, id DESC
                ) dv
            JOIN stake_address ON stake_address.id = dv.addr_id
            JOIN drep_hash ON drep_hash.id = dv.drep_hash_id"#,
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

    /// Full pool-delegation history for one stake credential (29-byte `hash_raw`),
    /// oldest-first, with each event's previous target via `LAG`. Stake
    /// deregistrations are interleaved as `to: None` so the `from` chain is correct
    /// across them. `addr_id`-indexed (small per address).
    pub async fn pool_delegation_history(
        &self,
        hash_raw: &[u8],
    ) -> Result<Vec<DelegationEvent>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT encode(tx_hash, 'hex') AS "tx_hash!",
                      to_pool,
                      lag(to_pool) OVER w AS from_pool
            FROM (
                SELECT t.hash AS tx_hash, d.tx_id, d.cert_index, ph.hash_raw AS to_pool
                FROM delegation d
                JOIN tx t ON t.id = d.tx_id
                JOIN pool_hash ph ON ph.id = d.pool_hash_id
                WHERE d.addr_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
                UNION ALL
                SELECT t.hash, sd.tx_id, sd.cert_index, NULL::bytea
                FROM stake_deregistration sd
                JOIN tx t ON t.id = sd.tx_id
                WHERE sd.addr_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
            ) e
            WINDOW w AS (ORDER BY tx_id, cert_index)
            ORDER BY tx_id, cert_index"#,
            hash_raw
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| DelegationEvent {
                tx_hash: r.tx_hash,
                to: r.to_pool,
                from: r.from_pool,
            })
            .collect())
    }

    /// Full DRep-delegation history for one stake credential, oldest-first, with the
    /// previous target via `LAG`. Stake deregistrations are interleaved as `to: None`.
    /// DRep bytes are built the same way as `drep_delegations` (tag+hash, or the
    /// 0x02/0x03 sentinels for the predefined DReps).
    pub async fn drep_delegation_history(
        &self,
        hash_raw: &[u8],
    ) -> Result<Vec<DelegationEvent>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT encode(tx_hash, 'hex') AS "tx_hash!",
                      to_raw, to_script, to_view,
                      lag(to_raw) OVER w AS from_raw,
                      lag(to_script) OVER w AS from_script,
                      lag(to_view) OVER w AS from_view
            FROM (
                SELECT t.hash AS tx_hash, dv.tx_id, dv.cert_index,
                       dh.raw AS to_raw, dh.has_script AS to_script, dh.view AS to_view
                FROM delegation_vote dv
                JOIN tx t ON t.id = dv.tx_id
                JOIN drep_hash dh ON dh.id = dv.drep_hash_id
                WHERE dv.addr_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
                UNION ALL
                SELECT t.hash, sd.tx_id, sd.cert_index, NULL::bytea, NULL::boolean, NULL::varchar
                FROM stake_deregistration sd
                JOIN tx t ON t.id = sd.tx_id
                WHERE sd.addr_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
            ) e
            WINDOW w AS (ORDER BY tx_id, cert_index)
            ORDER BY tx_id, cert_index"#,
            hash_raw
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| DelegationEvent {
                tx_hash: r.tx_hash,
                to: drep_bytes(r.to_raw.as_deref(), r.to_script, r.to_view.as_deref()),
                from: drep_bytes(r.from_raw.as_deref(), r.from_script, r.from_view.as_deref()),
            })
            .collect())
    }

    /// Per-source reward rows for one stake credential, spendable in `(min_epoch, max_epoch]`,
    /// as `(epoch, type, pool_hash, amount)`. Pool rewards (`member`/`leader`) carry the source
    /// pool's `hash_raw`; `reward_rest` rows (reserves/treasury/…) have `None`. Drives the
    /// per-epoch REWARDS capsule; summing all rows per epoch reproduces `stake_reward_deltas`.
    /// `addr_id`-indexed.
    pub async fn stake_epoch_rewards(
        &self,
        hash_raw: &[u8],
        min_epoch: i64,
        max_epoch: i64,
    ) -> Result<Vec<(i64, String, Option<Vec<u8>>, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT epoch AS "epoch!", label AS "label!",
                      pool_hash, amount AS "amount!"
            FROM (
                SELECT r.spendable_epoch AS epoch, r.type::text AS label,
                       ph.hash_raw AS pool_hash, SUM(r.amount)::bigint AS amount
                FROM reward r
                JOIN pool_hash ph ON ph.id = r.pool_id
                WHERE r.addr_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
                  AND r.spendable_epoch > $2 AND r.spendable_epoch <= $3
                GROUP BY r.spendable_epoch, r.type, ph.hash_raw
                UNION ALL
                SELECT rr.spendable_epoch, rr.type::text,
                       NULL::bytea, SUM(rr.amount)::bigint
                FROM reward_rest rr
                WHERE rr.addr_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
                  AND rr.spendable_epoch > $2 AND rr.spendable_epoch <= $3
                GROUP BY rr.spendable_epoch, rr.type
            ) t
            ORDER BY epoch"#,
            hash_raw,
            min_epoch,
            max_epoch,
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| (r.epoch, r.label, r.pool_hash, r.amount))
            .collect())
    }

    /// Reward withdrawals for one stake credential at `slot_no >= min_slot`, as
    /// `(slot_no, amount)`. Used to undo the reward balance backward for withdrawal
    /// txs that aren't in the replayed set (their outputs went elsewhere).
    /// `addr_id`-indexed.
    pub async fn stake_withdrawals_since(
        &self,
        hash_raw: &[u8],
        min_slot: i64,
    ) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        // Bound on `w.tx_id` (not `b.slot_no` post-join): tx ids increase monotonically
        // with chain order, so `tx_id >= first-tx-of-first-block-at-slot-≥-min_slot` is
        // exactly `slot_no >= min_slot`. This lets the (addr_id, tx_id) composite seek
        // straight to the account's recent withdrawals instead of `idx_withdrawal_addr_id`
        // fetching *all* of them (≈1.5M for a script stake) only to drop the old ones.
        let rows = sqlx::query!(
            r#"WITH bnd AS MATERIALIZED (
                SELECT COALESCE(MIN(t.id), 9223372036854775807) AS min_tx_id
                FROM tx t
                WHERE t.block_id =
                    (SELECT id FROM block WHERE slot_no >= $2 ORDER BY slot_no ASC LIMIT 1)
            )
            SELECT b.slot_no AS "slot!", w.amount::bigint AS "amount!"
            FROM withdrawal w
            JOIN tx ON tx.id = w.tx_id
            JOIN block b ON b.id = tx.block_id
            WHERE w.addr_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
              AND w.tx_id >= (SELECT min_tx_id FROM bnd)"#,
            hash_raw,
            min_slot,
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.slot, r.amount)).collect())
    }

    /// Fetch DRep metadata (given_name) from off-chain vote data.
    /// Returns a map keyed by DRep bytes (tag + raw hash).
    pub async fn drep_metadata(
        &self,
        last_tx_id: i64,
        since_id: i64,
    ) -> Result<HashMap<Vec<u8>, DRep>, sqlx::Error> {
        let mut rows = sqlx::query!(
            r#"SELECT dh.raw AS drep_raw,
                      dh.has_script AS drep_has_script,
                      dd.given_name,
                      (SELECT ddist.active_until::bigint FROM drep_distr ddist
                       WHERE ddist.hash_id = dh.id
                         AND ddist.epoch_no = (SELECT MAX(epoch_no) FROM drep_distr)
                      ) AS active_until
            FROM drep_registration dr
            JOIN drep_hash dh ON dh.id = dr.drep_hash_id
            JOIN off_chain_vote_data ovd ON ovd.voting_anchor_id = dr.voting_anchor_id
            JOIN off_chain_vote_drep_data dd ON dd.off_chain_vote_data_id = ovd.id
            WHERE dr.tx_id <= $1
              AND dh.raw IS NOT NULL
              -- Incremental cursor: only DReps whose off-chain metadata row is newer than
              -- `since_id` (0 = all, at reset). Rollbacks never reuse ids, so > is safe.
              AND dd.id > $2
              -- Latest registration that carried an anchor: a deregistration (and the
              -- initial register cert) has voting_anchor_id NULL, so picking the plain
              -- MAX(id) would drop the name of a deregistered/updated DRep whose
              -- metadata is still in db-sync.
              AND dr.id = (
                  SELECT MAX(dr2.id) FROM drep_registration dr2
                  WHERE dr2.drep_hash_id = dr.drep_hash_id AND dr2.tx_id <= $1
                    AND dr2.voting_anchor_id IS NOT NULL
              )"#,
            last_tx_id,
            since_id
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
                    active_until: row.active_until,
                },
            );
        }

        Ok(dreps)
    }

    /// Highest `off_chain_pool_data.id` — the cheap (PK max) gate for the live
    /// ticker refresh: only query the rows themselves when this exceeds the cursor.
    pub async fn max_pool_meta_id(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar!(r#"SELECT COALESCE(MAX(id), 0) AS "max!" FROM off_chain_pool_data"#)
            .fetch_one(&self.db)
            .await
    }

    /// Highest `off_chain_vote_drep_data.id` — gate for the live DRep-name refresh.
    pub async fn max_drep_meta_id(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT COALESCE(MAX(id), 0) AS "max!" FROM off_chain_vote_drep_data"#
        )
        .fetch_one(&self.db)
        .await
    }

    /// Off-chain pool tickers fetched since `since_id` (0 = all). Returns
    /// `(pool hash_raw, ticker, off_chain_pool_data.id)` ordered by id, so applying
    /// in order leaves the latest ticker per pool. db-sync appends a row per fetch and
    /// never reuses ids, so `id > since_id` is rollback-safe.
    pub async fn pool_ticker_updates(
        &self,
        since_id: i64,
    ) -> Result<Vec<(Vec<u8>, String, i64)>, sqlx::Error> {
        Ok(sqlx::query!(
            r#"SELECT ph.hash_raw, ocpd.ticker_name AS "ticker_name!", ocpd.id
            FROM off_chain_pool_data ocpd
            JOIN pool_hash ph ON ph.id = ocpd.pool_id
            WHERE ocpd.id > $1
            ORDER BY ocpd.id"#,
            since_id
        )
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(|r| (r.hash_raw, r.ticker_name, r.id))
        .collect())
    }

    /// Fetch CIP-68 decimals, keyed by the **user token** that actually carries
    /// them. Returns `(policy, user_token_name, decimals)` — the caller stores one
    /// fingerprint per token.
    ///
    /// Inverted search: per CIP-68 only (333) FT / (444) RFT user tokens have
    /// decimals (never a (222) NFT), and there are only ~5k of those vs ~297k
    /// (100) reference tokens. So we enumerate the FT/RFT (via the partial
    /// `idx_multi_asset_cip68_ft`), resolve each to its (100) reference token's id
    /// (`unique_multi_asset`), and read decimals from that reference's current
    /// datum — ~5k `ma_tx_out.ident` index lookups instead of a 472M-row scan
    /// (~230s → ~2s).
    pub async fn cip68_decimals(
        &self,
        last_tx_id: i64,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>, i32)>, sqlx::Error> {
        // The datum value JSONB is {"fields":[{"map":[{"k":…,"v":{"int":…}}]}, …]};
        // we read the "decimals" key (hex 646563696d616c73) from the first field's
        // map. Two indexed datum joins + COALESCE cover both inline and hash-
        // referenced datums — the single `ON d.id=… OR d.hash=…` form is a ~180s
        // trap (neither index usable).
        let rows = sqlx::query!(
            r#"WITH ref AS MATERIALIZED (
                -- real 333/444 user token paired with its (100) reference id
                SELECT ft.policy AS policy, ft.name AS user_name, rma.id AS ref_id
                FROM multi_asset ft
                JOIN multi_asset rma ON rma.policy = ft.policy
                  AND rma.name = '\x000643b0'::bytea || substring(ft.name FROM 5)
                WHERE substring(ft.name FROM 1 FOR 4) IN ('\x0014df10', '\x001bc280')
            ),
            held AS MATERIALIZED (
                -- reference-token UTXO held as of last_tx_id. `ident = ANY(array)`
                -- keeps the index path; a plain join from the ref set flips to a
                -- parallel seq scan of all 472M ma_tx_out rows.
                SELECT mto.ident AS ref_id, txo.inline_datum_id, txo.data_hash
                FROM ma_tx_out mto
                JOIN tx_out txo ON txo.id = mto.tx_out_id
                WHERE mto.ident = ANY(ARRAY(SELECT DISTINCT ref_id FROM ref))
                  AND txo.tx_id <= $1
                  AND (txo.consumed_by_tx_id IS NULL OR txo.consumed_by_tx_id > $1)
            )
            SELECT ref.policy AS "policy!", ref.user_name AS "name!",
                   e->'v'->>'int' AS "decimals"
            FROM held
            JOIN ref ON ref.ref_id = held.ref_id
            LEFT JOIN datum di ON di.id = held.inline_datum_id
            LEFT JOIN datum dh ON dh.hash = held.data_hash
            CROSS JOIN LATERAL jsonb_array_elements(
                COALESCE(di.value, dh.value)->'fields'->0->'map'
            ) AS e
            WHERE (e->'k') @> '{"bytes":"646563696d616c73"}'
              AND (e->'v'->>'int') IS NOT NULL"#,
            last_tx_id
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
    /// Fetch all current ADA Handle owners for the given policy IDs.
    /// Returns (handle_name, address, optional_datum_bytes) tuples.
    /// For classic and CIP-68 (222) handles, address is the token holder.
    /// For virtual (000) handles, datum_bytes contains the inline datum to parse.
    pub async fn handles(
        &self,
        policies: &[&[u8]],
    ) -> Result<Vec<(String, String, Option<Vec<u8>>)>, sqlx::Error> {
        let mut results = Vec::new();
        let mut classic = 0usize;
        let mut cip68 = 0usize;
        let mut virtual_count = 0usize;
        let mut skipped = 0usize;
        for policy in policies {
            let rows = sqlx::query!(
                r#"SELECT ma.name AS "name!", a.address AS "address!",
                    d.bytes AS "datum?"
                FROM tx_out
                JOIN ma_tx_out ON ma_tx_out.tx_out_id = tx_out.id
                JOIN multi_asset ma ON ma.id = ma_tx_out.ident
                JOIN address a ON a.id = tx_out.address_id
                LEFT JOIN datum d ON d.id = tx_out.inline_datum_id
                WHERE ma.policy = $1
                AND tx_out.consumed_by_tx_id IS NULL
                AND ma.name != '' AND ma.name IS NOT NULL
                AND substring(ma.name from 1 for 4) != '\x000643b0'"#,
                *policy as &[u8]
            )
            .fetch_all(&self.db)
            .await?;
            for r in rows {
                let (handle, is_virtual) = match parse_handle_name(&r.name) {
                    Some(parsed) => parsed,
                    None => {
                        skipped += 1;
                        continue;
                    }
                };
                let datum = if is_virtual {
                    virtual_count += 1;
                    r.datum
                } else if r.name.starts_with(CIP67_LABEL_222) {
                    cip68 += 1;
                    None
                } else {
                    classic += 1;
                    None
                };
                results.push((handle, r.address, datum));
            }
        }
        tracing::info!(
            classic,
            cip68,
            virtual_count,
            skipped,
            total = results.len(),
            "fetched ADA Handles from db-sync"
        );
        Ok(results)
    }

    /// Fetch governance action titles: "tx_hash#index" → title (or type as fallback).
    pub async fn gov_action_titles(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT encode(tx.hash, 'hex') AS "tx_hash!",
                    gap.index AS "index!",
                    gap.type AS "type!: String",
                    ovgad.title AS "title?"
            FROM gov_action_proposal gap
            JOIN tx ON tx.id = gap.tx_id
            LEFT JOIN voting_anchor va ON va.id = gap.voting_anchor_id
            LEFT JOIN off_chain_vote_data ovd ON ovd.voting_anchor_id = va.id
            LEFT JOIN off_chain_vote_gov_action_data ovgad ON ovgad.off_chain_vote_data_id = ovd.id"#
        )
        .fetch_all(&self.db)
        .await?;
        let mut map = std::collections::HashMap::new();
        for r in rows {
            let key = format!("{}#{}", r.tx_hash, r.index);
            let title = r.title.unwrap_or_else(|| r.r#type.clone());
            map.insert(key, title);
        }
        Ok(map)
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
