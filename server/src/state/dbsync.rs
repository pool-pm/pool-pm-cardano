use futures::TryStreamExt;
use imbl::{hashmap::HashMap, hashset::HashSet};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use tokio::time::Duration;
use url::Url;

use crate::model::{asset_fingerprint, parse_handle_name, DRep, Pool, CIP67_LABEL_222};

/// A resolved UTXO: `(address, lovelace, assets, unspent)`, keyed by
/// `(tx_hash, output_index)`. Returned by `resolve_utxos_batch`.
type ResolvedUtxos =
    std::collections::HashMap<(Vec<u8>, i16), (String, u64, Vec<(String, u64)>, bool)>;

/// Statements slower than this are logged at WARN. Sized to the ~100 ms
/// per-query target with headroom: anything over 1s in steady state signals a
/// regression. The reset/warm-up scans are deliberately slower and trip this
/// during init (as they did at the previous 15s threshold).
const SLOW_QUERY_THRESHOLD: Duration = Duration::from_secs(1);

/// Cheap to clone — `sqlx::Pool` is internally `Arc`-shared, so a `DbSync`
/// clone reuses the same underlying connection pool. Cloning hands a db
/// handle to a caller that wants to run queries without holding a lock on the
/// owner of the original `DbSync`.
#[derive(Clone)]
pub struct DbSync {
    db: sqlx::Pool<sqlx::Postgres>,
}

impl DbSync {
    pub async fn new(url: &Url) -> Result<Self, sqlx::Error> {
        let options = PgConnectOptions::from_url(url)?
            .log_slow_statements(log::LevelFilter::Warn, SLOW_QUERY_THRESHOLD);

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

    /// Recent blocks containing a transaction that touches the given stake address
    /// (`hash_raw` is the full 29-byte reward address = db-sync
    /// `stake_address.hash_raw`): either an output paid to it, or one of its
    /// outputs being consumed. Newest-first, capped at `limit`. Returns
    /// (slot_no, block_hash_hex, block_no). Used to drive feed replay; an unknown
    /// address yields an empty result.
    pub async fn stake_recent_blocks(
        &self,
        hash_raw: &[u8],
        limit: i64,
    ) -> Result<Vec<(u64, String, u64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT b.slot_no AS "slot_no!", b.hash, b.block_no AS "block_no!"
            FROM (
                SELECT tx_id FROM tx_out
                  WHERE stake_address_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
                UNION
                SELECT consumed_by_tx_id AS tx_id FROM tx_out
                  WHERE stake_address_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
                    AND consumed_by_tx_id IS NOT NULL
            ) t
            JOIN tx ON tx.id = t.tx_id
            JOIN block b ON b.id = tx.block_id
            GROUP BY b.id, b.slot_no, b.hash, b.block_no
            ORDER BY b.slot_no DESC
            LIMIT $2"#,
            hash_raw,
            limit
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.slot_no as u64, hex::encode(r.hash), r.block_no as u64))
            .collect())
    }

    /// Recent blocks containing a transaction that touches the given payment
    /// address (an output paid to it, or one of its outputs consumed). Newest-first,
    /// capped at `limit`. Returns (slot_no, block_hash_hex, block_no). Mirrors
    /// `stake_recent_blocks` but matches the exact address.
    pub async fn address_recent_blocks(
        &self,
        address: &str,
        limit: i64,
    ) -> Result<Vec<(u64, String, u64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT b.slot_no AS "slot_no!", b.hash, b.block_no AS "block_no!"
            FROM (
                SELECT tx_id FROM tx_out WHERE address = $1
                UNION
                SELECT consumed_by_tx_id AS tx_id FROM tx_out
                  WHERE address = $1 AND consumed_by_tx_id IS NOT NULL
            ) t
            JOIN tx ON tx.id = t.tx_id
            JOIN block b ON b.id = tx.block_id
            GROUP BY b.id, b.slot_no, b.hash, b.block_no
            ORDER BY b.slot_no DESC
            LIMIT $2"#,
            address,
            limit
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.slot_no as u64, hex::encode(r.hash), r.block_no as u64))
            .collect())
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
    ) -> Result<Vec<(i64, String, Vec<u8>)>, sqlx::Error> {
        // MATERIALIZED CTE inhibits the planner from pushing the outer
        // `ORDER BY id DESC LIMIT N` into a backward pkey scan that filters by
        // policy — that plan walks every newer multi_asset row before finding
        // matches in an old policy (e.g. SpaceBudz: minted years ago → low ids
        // → millions of rows scanned for 60 hits). Materializing forces the
        // bitmap-scan via `unique_multi_asset (policy, name)` first, then
        // top-N sort over the small per-policy result.
        let rows = sqlx::query!(
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
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.id, r.fingerprint, r.name))
            .collect())
    }

    /// All distinct assets currently held by a payment address, newest-minted
    /// first, keyset-paginated on `multi_asset.id` (same scheme as
    /// `assets_by_policy`). **No CIP-68 reference-token filter** — owned-asset
    /// listings show what the wallet actually holds (including reference NFTs
    /// it may hold). Returns (id, fingerprint, name).
    pub async fn address_assets(
        &self,
        address: &str,
        cursor: Option<i64>,
        limit: i64,
    ) -> Result<Vec<(i64, String, Vec<u8>)>, sqlx::Error> {
        // Resolve (fingerprint, name) for only the returned page, not for every
        // asset the address holds. The MATERIALIZED CTE forces the address-first
        // bitmap path (without it the planner picks a catastrophic backward
        // multi_asset index scan); we keyset-paginate the distinct ids first and
        // join multi_asset for just this page. The old form materialized metadata
        // for *all* of the address's assets merely to return one page — e.g. a
        // 30k-asset address spent ~700ms resolving 30k rows to show 60.
        let rows = sqlx::query!(
            r#"WITH ids AS MATERIALIZED (
                SELECT DISTINCT mto.ident AS id
                FROM ma_tx_out mto
                JOIN tx_out txo ON txo.id = mto.tx_out_id
                WHERE txo.address = $1 AND txo.consumed_by_tx_id IS NULL
            )
            SELECT ma.id AS "id!", ma.fingerprint AS "fingerprint!", ma.name AS "name!"
            FROM (
                SELECT id FROM ids
                WHERE ($2::bigint IS NULL OR id < $2)
                ORDER BY id DESC
                LIMIT $3
            ) page
            JOIN multi_asset ma ON ma.id = page.id
            ORDER BY ma.id DESC"#,
            address,
            cursor,
            limit
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.id, r.fingerprint, r.name))
            .collect())
    }

    /// All distinct assets currently held across every payment address that
    /// shares the given stake credential, newest-minted first, keyset-paginated
    /// on `multi_asset.id`. `hash_raw` is the full 29-byte reward-address
    /// payload (matches `stake_address.hash_raw`).
    pub async fn stake_assets(
        &self,
        hash_raw: &[u8],
        cursor: Option<i64>,
        limit: i64,
    ) -> Result<Vec<(i64, String, Vec<u8>)>, sqlx::Error> {
        // See `address_assets` for the rationale: resolve metadata for only the
        // returned page, keyset-paginating the distinct ids first.
        let rows = sqlx::query!(
            r#"WITH ids AS MATERIALIZED (
                SELECT DISTINCT mto.ident AS id
                FROM ma_tx_out mto
                JOIN tx_out txo ON txo.id = mto.tx_out_id
                WHERE txo.stake_address_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
                  AND txo.consumed_by_tx_id IS NULL
            )
            SELECT ma.id AS "id!", ma.fingerprint AS "fingerprint!", ma.name AS "name!"
            FROM (
                SELECT id FROM ids
                WHERE ($2::bigint IS NULL OR id < $2)
                ORDER BY id DESC
                LIMIT $3
            ) page
            JOIN multi_asset ma ON ma.id = page.id
            ORDER BY ma.id DESC"#,
            hash_raw,
            cursor,
            limit
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.id, r.fingerprint, r.name))
            .collect())
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

    /// Per-address balance **and unspent-UTXO count** at `last_tx_id`, from one
    /// grouped scan of unconsumed UTXOs — expensive on mainnet (paid once on cold
    /// reset / first run after upgrade); see `populate_address_balances`. The
    /// `COUNT(*)` rides free on the scan we already run for balances and is the
    /// signal that selects the scan-bound owned-asset cache set (see plan: Part
    /// 2 — "membership = unspent_utxos, free"), so the caller needs no separate
    /// discovery scan. Returns `(bech32 address, lovelace, unspent_utxos)`;
    /// addresses are returned bech32-encoded (the caller parses to bytes and
    /// skips Byron / non-bech32 ones — they don't appear in feeds).
    pub async fn address_balances(
        &self,
        last_tx_id: i64,
    ) -> Result<Vec<(String, i64, i64)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT address AS "address!", SUM(value)::bigint AS "balance!",
                      COUNT(*)::bigint AS "n_utxos!"
            FROM tx_out
            WHERE tx_id <= $1
              AND (consumed_by_tx_id IS NULL OR consumed_by_tx_id > $1)
            GROUP BY address"#,
            last_tx_id
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| (r.address, r.balance, r.n_utxos))
            .collect())
    }

    /// Asset holdings (fingerprint, name, summed quantity) for the given explicit
    /// set of payment `addresses` (bech32) — the scan-bound set the caller already
    /// identified from the free `unspent_utxos` counts (see `address_balances`),
    /// so this does NOT re-discover it. ONE bulk pass, not per-address: a single
    /// whale costs ~138s per-address, but here every heavy address is amortized
    /// into one scan. The `heavy` set is injected as a small relation (`unnest`),
    /// leaving the downstream join plan identical to the old grouped-discovery
    /// form — only the discovery scan is dropped. Rows are ordered by
    /// `multi_asset.id` so the caller can intern refs in mint order (ref DESC ⇒
    /// newest-first pagination). Returns (address_bytes, fingerprint, name,
    /// quantity); Byron (non-bech32) addresses are skipped. Cold-reset / warm only.
    pub async fn heavy_address_assets(
        &self,
        last_tx_id: i64,
        addresses: &[String],
    ) -> Result<Vec<(Vec<u8>, String, Vec<u8>, i64)>, sqlx::Error> {
        use pallas::ledger::addresses::Address;

        let mut rows = sqlx::query!(
            r#"WITH heavy(address) AS (
                SELECT * FROM unnest($2::text[])
            )
            SELECT txo.address AS "address!", ma.id AS "id!",
                   ma.fingerprint AS "fingerprint!", ma.name AS "name!",
                   SUM(mto.quantity)::bigint AS "qty!"
            FROM heavy h
            JOIN tx_out txo ON txo.address = h.address
            JOIN ma_tx_out mto ON mto.tx_out_id = txo.id
            JOIN multi_asset ma ON ma.id = mto.ident
            WHERE txo.tx_id <= $1
              AND (txo.consumed_by_tx_id IS NULL OR txo.consumed_by_tx_id > $1)
            GROUP BY txo.address, ma.id, ma.fingerprint, ma.name
            ORDER BY ma.id"#,
            last_tx_id,
            addresses
        )
        .fetch(&self.db);

        let mut out: Vec<(Vec<u8>, String, Vec<u8>, i64)> = Vec::new();
        while let Some(row) = rows.try_next().await? {
            let Ok(addr) = Address::from_bech32(&row.address) else {
                continue;
            };
            out.push((addr.to_vec(), row.fingerprint, row.name, row.qty));
        }
        Ok(out)
    }

    /// Asset holdings (summed across **all** of a stake credential's payment
    /// addresses) for the given explicit set of 28-byte `creds` — the scan-bound
    /// stakes the caller already identified from the free per-stake `unspent_utxos`
    /// sums (see `State::warm_asset_cache`), so this does NOT re-discover them.
    /// Caching the whole stake (not just its heavy addresses) keeps the union
    /// complete. Returns the 28-byte credential as key, matching how
    /// `stakes`/`pool_delegations` are keyed. Cold-reset / warm only.
    ///
    /// Two steps, because the holdings join only gets an index-driven plan when it
    /// receives **concrete** `stake_address_id`s: matching `stake_address` by a
    /// `substring(hash_raw)` expression inside the holdings query defeats the index
    /// AND mis-estimates cardinality, making the planner seq-scan all ~472M
    /// `ma_tx_out` rows. So we first resolve creds → ids (a cheap `stake_address`-
    /// only scan), then fetch holdings by the explicit id list (nested loop over
    /// `idx_tx_out_stake_address_id`).
    pub async fn heavy_stake_assets(
        &self,
        last_tx_id: i64,
        creds: &[Vec<u8>],
    ) -> Result<Vec<(Vec<u8>, String, Vec<u8>, i64)>, sqlx::Error> {
        if creds.is_empty() {
            return Ok(Vec::new());
        }

        // Step 1: resolve heavy creds → stake_address ids. Scans only
        // `stake_address` (the `substring` is confined here, never joined to
        // `ma_tx_out`), so it stays cheap.
        let ids: Vec<i64> = sqlx::query!(
            r#"SELECT id FROM stake_address
               WHERE substring(hash_raw FROM 2) = ANY($1::bytea[])"#,
            creds
        )
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Step 2: holdings via the concrete id list — index-driven nested loop.
        let rows = sqlx::query!(
            r#"WITH heavy(sid) AS (
                SELECT * FROM unnest($2::bigint[])
            )
            SELECT sa.hash_raw AS "hash_raw!", ma.id AS "id!",
                   ma.fingerprint AS "fingerprint!", ma.name AS "name!",
                   SUM(mto.quantity)::bigint AS "qty!"
            FROM heavy h
            JOIN stake_address sa ON sa.id = h.sid
            JOIN tx_out txo ON txo.stake_address_id = h.sid
            JOIN ma_tx_out mto ON mto.tx_out_id = txo.id
            JOIN multi_asset ma ON ma.id = mto.ident
            WHERE txo.tx_id <= $1
              AND (txo.consumed_by_tx_id IS NULL OR txo.consumed_by_tx_id > $1)
            GROUP BY sa.hash_raw, ma.id, ma.fingerprint, ma.name
            ORDER BY ma.id"#,
            last_tx_id,
            &ids
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows
            .into_iter()
            // strip the 1-byte header → 28-byte stake credential
            .map(|r| (r.hash_raw[1..].to_vec(), r.fingerprint, r.name, r.qty))
            .collect())
    }

    /// Distinct unconsumed multi-assets currently held by a payment address.
    /// Connect-time query for the feed header's `assets_count` — no live
    /// update, the user refreshes to re-fetch.
    pub async fn address_assets_count(&self, address: &str) -> Result<i64, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT COUNT(DISTINCT mto.ident)::bigint AS "count!"
            FROM ma_tx_out mto
            JOIN tx_out txo ON txo.id = mto.tx_out_id
            WHERE txo.address = $1 AND txo.consumed_by_tx_id IS NULL"#,
            address
        )
        .fetch_one(&self.db)
        .await?;
        Ok(row.count)
    }

    /// Distinct unconsumed multi-assets across every payment address sharing
    /// the stake credential (29-byte `hash_raw`). Connect-time only.
    pub async fn stake_assets_count(&self, hash_raw: &[u8]) -> Result<i64, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT COUNT(DISTINCT mto.ident)::bigint AS "count!"
            FROM ma_tx_out mto
            JOIN tx_out txo ON txo.id = mto.tx_out_id
            WHERE txo.stake_address_id = (SELECT id FROM stake_address WHERE hash_raw = $1)
              AND txo.consumed_by_tx_id IS NULL"#,
            hash_raw
        )
        .fetch_one(&self.db)
        .await?;
        Ok(row.count)
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
                r#"SELECT ma.name AS "name!", tx_out.address AS "address!",
                    d.bytes AS "datum?"
                FROM tx_out
                JOIN ma_tx_out ON ma_tx_out.tx_out_id = tx_out.id
                JOIN multi_asset ma ON ma.id = ma_tx_out.ident
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
