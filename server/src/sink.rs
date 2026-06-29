use gasket::framework::*;
use oura::framework::*;
use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::miniprotocols::Point;
use sqlx::types::Decimal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use pallas::crypto::hash::Hasher;

use crate::cip68;

use crate::event::Event;
use crate::event_bus::EventBus;
use crate::mempool::extract_tx;
use crate::model::{
    asset_fingerprint, is_handle_policy, parse_handle_name, pool_bech32_id, TxOutput,
};
use crate::nftcdn::NftcdnConfig;
use crate::pallas::{
    stake_credential_bytes, stake_credential_from_address_bytes, MultiEraTxExt, PoolUpdate,
};
use crate::state::feed_index::{BlockRef, DelegationEntry};
use crate::state::{BlockUpdate, State};

/// During normal operation, persist a snapshot every this many blocks. Skipped
/// while catching up (initial sync), where periodic saves only slow it down.
const SNAPSHOT_INTERVAL: u64 = 50;

pub struct Worker;

impl Worker {
    async fn handle_reset(&self, point: &Point, stage: &Stage) -> Result<(), WorkerError> {
        let slot = point.slot_or_default();

        {
            let mut state = stage.state.write().await;
            if state.rollback(slot) {
                // Snapshot covered this slot, just truncate history. The off-chain
                // ticker/name cache isn't block-derived, so a truncated refresh would be
                // lost while its in-memory cursor stayed advanced; reset the cursors so
                // the next block re-reads from 0 and re-applies current values.
                state.pool_meta_cursor = 0;
                state.drep_meta_cursor = 0;
            } else {
                state
                    .reset(slot, &stage.genesis, stage.mainnet)
                    .await
                    .or_panic()?;
                match state.save_snapshot(
                    &stage.snapshot_path,
                    stage.snapshot_depth,
                    stage.genesis.magic,
                ) {
                    Ok(saved_slot) => info!(saved_slot, "snapshot saved after reset"),
                    Err(e) => warn!("failed to save snapshot after reset: {}", e),
                }
            }
        }

        stage.event_bus.send(Event::Rollback { slot }).await;

        Ok(())
    }

    async fn handle_apply(&self, cbor: &[u8], stage: &Stage) -> Result<(), WorkerError> {
        let block = MultiEraBlock::decode(cbor).or_panic()?;
        let slot = block.slot();
        let height = block.number();
        let block_hash = block.hash().to_string();
        let (
            txs,
            produced,
            consumed,
            pool_deleg,
            drep_deleg,
            pool_updates,
            pool_retirements,
            stake_changes,
            withdrawal_changes,
            pool_id,
            pool_ticker,
            issuer_pool_hash,
            stake_change_pools,
            stake_change_dreps,
            feed_delegations,
            drep_feed_delegations,
            new_decimals,
            handle_changes,
        ) = {
            let state = stage.state.read().await;
            let snap = state.current();
            let mut txs = Vec::new();
            // Each consumed input carries its resolved TxOutput so `apply_block`
            // can update per-address aggregates without re-looking up inputs
            // that predate the snapshot (db-sync fallback path).
            let mut consumed: Vec<((Vec<u8>, i16), TxOutput)> = Vec::new();
            let mut produced: std::collections::HashMap<(Vec<u8>, i16), TxOutput> =
                std::collections::HashMap::new();
            let mut pool_deleg: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
            let mut drep_deleg: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
            let mut pool_updates: Vec<PoolUpdate> = Vec::new();
            let mut pool_retirements: Vec<(Vec<u8>, u64)> = Vec::new();
            let mut stake_changes: Vec<(Vec<u8>, i64)> = Vec::new();
            let mut withdrawal_changes: Vec<(Vec<u8>, i64)> = Vec::new();

            // Feed index: track pools/dreps with stake changes in this block
            let mut stake_change_pools: std::collections::HashSet<Vec<u8>> =
                std::collections::HashSet::new();
            let mut stake_change_dreps: std::collections::HashSet<Vec<u8>> =
                std::collections::HashSet::new();

            // CIP-68: collect decimals from reference token datums in this block
            let mut new_decimals: Vec<(String, u8)> = Vec::new();

            // ADA Handle: collect (handle_name, owner_address) for this block
            let mut handle_changes: Vec<(String, String)> = Vec::new();

            // Feed index: collect raw delegation certs for building DelegationEntry
            struct RawDelegCert {
                tx_hash: String,
                cred_bytes: Vec<u8>,
                target_pool: Option<Vec<u8>>,
            }
            let mut raw_deleg_certs: Vec<RawDelegCert> = Vec::new();

            struct RawDrepDelegCert {
                tx_hash: String,
                cred_bytes: Vec<u8>,
                target_drep: Option<Vec<u8>>,
            }
            let mut raw_drep_deleg_certs: Vec<RawDrepDelegCert> = Vec::new();

            for tx in block.txs() {
                let hash = tx.hash();

                // Track consumed UTXOs: subtract lovelaces from stake credentials
                for input in tx.inputs() {
                    let key = (input.hash().as_ref().to_vec(), input.index() as i16);
                    // Check block-local UTXOs first, then in-memory state,
                    // then fall back to db-sync for pre-reset UTXOs.
                    let resolved: Option<TxOutput> = if let Some(utxo) = produced.get(&key) {
                        Some(utxo.clone())
                    } else if let Some(utxo) = snap.and_then(|s| s.utxos.get(&key)) {
                        Some(utxo.clone())
                    } else {
                        let (addr_str, lovelace, assets) = state
                            .resolve_input(input.hash().as_ref(), input.index() as i16)
                            .await;
                        addr_str
                            .and_then(|a| {
                                pallas::ledger::addresses::Address::from_bech32(&a)
                                    .ok()
                                    .map(|a| a.to_vec())
                            })
                            .map(|addr| TxOutput {
                                lovelaces: Decimal::from(lovelace),
                                address: addr,
                                assets,
                            })
                    };
                    if let Some(utxo) = resolved {
                        if let Some(cred) = stake_credential_from_address_bytes(&utxo.address) {
                            let amount: i64 = utxo
                                .lovelaces
                                .try_into()
                                .expect("lovelace value must fit i64");
                            stake_changes.push((cred.clone(), -amount));

                            // Feed index: track pool/drep for any consumed input
                            if let Some(pool) = snap.and_then(|s| s.pool_delegations.get(&cred)) {
                                stake_change_pools.insert(pool.clone());
                            }
                            if let Some(drep) = snap.and_then(|s| s.drep_delegations.get(&cred)) {
                                stake_change_dreps.insert(drep.clone());
                            }
                        }
                        consumed.push((key, utxo));
                    } else {
                        // Unresolved input (very old / byron-only): record the
                        // utxo removal key with a zero-lovelace, no-asset
                        // sentinel so the utxo map drop still happens.
                        consumed.push((
                            key,
                            TxOutput {
                                lovelaces: Decimal::ZERO,
                                address: Vec::new(),
                                assets: Vec::new(),
                            },
                        ));
                    }
                }

                txs.push(
                    extract_tx(
                        &tx,
                        &state,
                        &stage.nftcdn,
                        &produced,
                        stage.mainnet,
                        &stage.genesis,
                    )
                    .await,
                );

                // CIP-68: extract decimals from reference token datums
                new_decimals.extend(cip68::extract_from_tx(&tx));

                // Track produced UTXOs: add lovelaces to stake credentials
                for (idx, output) in tx.outputs().iter().enumerate() {
                    let addr = output
                        .address()
                        .ok()
                        .map(|a| a.to_vec())
                        .unwrap_or_default();
                    let coin = output.value().coin();
                    let lovelaces = Decimal::from(coin);
                    if let Some(cred) = stake_credential_from_address_bytes(&addr) {
                        let amount: i64 =
                            lovelaces.try_into().expect("lovelace value must fit i64");
                        stake_changes.push((cred.clone(), amount));

                        // Feed index: track pool/drep for outputs > 1000 ADA
                        if coin > 1_000_000_000 {
                            if let Some(pool) = snap.and_then(|s| s.pool_delegations.get(&cred)) {
                                stake_change_pools.insert(pool.clone());
                            }
                            if let Some(drep) = snap.and_then(|s| s.drep_delegations.get(&cred)) {
                                stake_change_dreps.insert(drep.clone());
                            }
                        }
                    }
                    let mut assets = Vec::new();
                    for pa in output.value().assets().iter() {
                        let policy_id = pa.policy().as_ref();
                        for a in pa.assets().iter() {
                            if let Some(raw) = a.output_coin() {
                                let fp = asset_fingerprint(policy_id, a.name());
                                assets.push((fp, raw));
                            }
                            // Detect ADA Handle tokens (classic, CIP-68, virtual)
                            if is_handle_policy(policy_id) {
                                if let Some((handle, is_virtual)) = parse_handle_name(a.name()) {
                                    let owner = if is_virtual {
                                        // Virtual: resolve from inline datum
                                        output.datum().and_then(|d| {
                                            use pallas::ledger::primitives::conway::DatumOption;
                                            match d.into() {
                                                DatumOption::Data(data) => {
                                                    crate::model::parse_virtual_handle_address_from_datum(
                                                        &data.0,
                                                    )
                                                }
                                                _ => None,
                                            }
                                        })
                                    } else {
                                        pallas::ledger::addresses::Address::from_bytes(&addr)
                                            .ok()
                                            .map(|a| a.to_string())
                                    };
                                    if let Some(owner) = owner {
                                        handle_changes.push((handle, owner));
                                    }
                                }
                            }
                        }
                    }
                    produced.insert(
                        (hash.as_ref().to_vec(), idx as i16),
                        TxOutput {
                            lovelaces,
                            address: addr,
                            assets,
                        },
                    );
                }

                // Track withdrawals (reduce reward balance)
                for (reward_addr, amount) in tx.withdrawals_sorted_set() {
                    if reward_addr.len() >= 29 {
                        let cred = reward_addr[1..29].to_vec();
                        // Feed index: a withdrawal is part of the credential's activity, so
                        // flag its pool/drep — surfaces withdrawal-only blocks in pool/drep
                        // feeds (zero-amount script-validation withdrawals included).
                        if let Some(pool) = snap.and_then(|s| s.pool_delegations.get(&cred)) {
                            stake_change_pools.insert(pool.clone());
                        }
                        if let Some(drep) = snap.and_then(|s| s.drep_delegations.get(&cred)) {
                            stake_change_dreps.insert(drep.clone());
                        }
                        withdrawal_changes.push((cred, amount as i64));
                    }
                }

                // Collect delegation certs (both raw bytes for state and full for feed index)
                let certs = tx.pool_delegation_certs();
                for (cred, target_pool) in &certs {
                    let cred_bytes = stake_credential_bytes(cred);
                    pool_deleg.push((cred_bytes.clone(), target_pool.clone()));
                    raw_deleg_certs.push(RawDelegCert {
                        tx_hash: hash.to_string(),
                        cred_bytes,
                        target_pool: target_pool.clone(),
                    });
                }

                let drep_changes = tx.drep_delegation_changes();
                for (cred_bytes, target_drep) in &drep_changes {
                    raw_drep_deleg_certs.push(RawDrepDelegCert {
                        tx_hash: hash.to_string(),
                        cred_bytes: cred_bytes.clone(),
                        target_drep: target_drep.clone(),
                    });
                }
                drep_deleg.extend(drep_changes);
                pool_updates.extend(tx.pool_updates());
                pool_retirements.extend(tx.pool_retirements());
            }

            let produced: Vec<_> = produced.into_iter().collect();

            // Extract block issuer pool hash
            let issuer_pool_hash: Option<Vec<u8>> = block
                .header()
                .issuer_vkey()
                .map(|vkey| Hasher::<224>::hash(vkey).as_ref().to_vec());

            let (pool_id, pool_ticker) = issuer_pool_hash
                .as_ref()
                .and_then(|hash| snap?.pools.get(&hex::encode(hash)))
                .map(|pool| (Some(pool_bech32_id(&pool.hash_raw)), pool.ticker.clone()))
                .unwrap_or((None, None));

            // Build feed delegation entries from pre-block state
            let mut feed_delegations: Vec<DelegationEntry> = Vec::new();
            if let Some(s) = snap {
                for cert in &raw_deleg_certs {
                    let from_pool = s.pool_delegations.get(&cert.cred_bytes);

                    // Skip same-pool re-delegation
                    if let (Some(from), Some(to)) = (from_pool, &cert.target_pool) {
                        if from == to {
                            continue;
                        }
                    }

                    let live_stake = s.stakes.get(&cert.cred_bytes).copied().unwrap_or(0)
                        + s.rewards.get(&cert.cred_bytes).copied().unwrap_or(0);

                    feed_delegations.push(DelegationEntry {
                        slot,
                        block_hash: block_hash.clone(),
                        block_no: height,
                        tx_hash: cert.tx_hash.clone(),
                        cred: cert.cred_bytes.clone(),
                        live_stake,
                        from: from_pool.cloned(),
                        to: cert.target_pool.clone(),
                    });
                }
            }

            // Build DRep feed delegation entries from pre-block state
            let mut drep_feed_delegations: Vec<DelegationEntry> = Vec::new();
            if let Some(s) = snap {
                for cert in &raw_drep_deleg_certs {
                    let from_drep = s.drep_delegations.get(&cert.cred_bytes);

                    // Skip same-drep re-delegation
                    if let (Some(from), Some(to)) = (from_drep, &cert.target_drep) {
                        if from == to {
                            continue;
                        }
                    }

                    let live_stake = s.stakes.get(&cert.cred_bytes).copied().unwrap_or(0)
                        + s.rewards.get(&cert.cred_bytes).copied().unwrap_or(0);

                    drep_feed_delegations.push(DelegationEntry {
                        slot,
                        block_hash: block_hash.clone(),
                        block_no: height,
                        tx_hash: cert.tx_hash.clone(),
                        cred: cert.cred_bytes.clone(),
                        live_stake,
                        from: from_drep.cloned(),
                        to: cert.target_drep.clone(),
                    });
                }
            }

            (
                txs,
                produced,
                consumed,
                pool_deleg,
                drep_deleg,
                pool_updates,
                pool_retirements,
                stake_changes,
                withdrawal_changes,
                pool_id,
                pool_ticker,
                issuer_pool_hash,
                stake_change_pools,
                stake_change_dreps,
                feed_delegations,
                drep_feed_delegations,
                new_decimals,
                handle_changes,
            )
        };

        let timestamp = crate::mempool::slot_to_timestamp(slot, &stage.genesis);
        let epoch = State::epoch_for_slot(slot, &stage.genesis);

        // Check for epoch boundary and fetch reward deltas + DRep activity from db-sync
        let (reward_deltas, drep_active_until) = {
            let state = stage.state.read().await;
            let last_epoch = state.current().and_then(|s| s.last_epoch);
            if last_epoch.is_some() && last_epoch != Some(epoch) {
                info!(
                    epoch,
                    "epoch boundary detected, fetching reward deltas + drep activity"
                );
                (
                    state.epoch_reward_delta(epoch).await,
                    state.drep_active_until().await,
                )
            } else {
                (None, None)
            }
        };

        // Watched-asset transitions this block (empty unless an assets page is open),
        // captured under the lock and fanned out after it's released.
        let asset_deltas: Vec<crate::state::AssetCountDelta>;
        {
            let mut state = stage.state.write().await;
            let block_ref = BlockRef {
                slot,
                hash: block_hash.clone(),
                number: height,
            };

            if let Some(pool_hash) = &issuer_pool_hash {
                state
                    .feed_index
                    .add_pool_minted(pool_hash.clone(), block_ref.clone());
            }

            if !stake_change_pools.is_empty() {
                state
                    .feed_index
                    .add_pool_stake_changes(stake_change_pools, block_ref.clone());
            }

            for entry in feed_delegations {
                state.feed_index.add_delegation_event(entry);
            }

            if !stake_change_dreps.is_empty() {
                state
                    .feed_index
                    .add_drep_stake_changes(stake_change_dreps, block_ref.clone());
            }

            for entry in drep_feed_delegations {
                state.feed_index.add_drep_delegation_event(entry);
            }

            // Prune feed index entries older than 5 days
            const FEED_INDEX_WINDOW: u64 = 5 * 86400;
            let prune_boundary = slot.saturating_sub(FEED_INDEX_WINDOW);
            state.feed_index.prune(prune_boundary);

            asset_deltas = state.apply_block(BlockUpdate {
                slot,
                block_hash: block_hash.clone(),
                epoch,
                produced,
                consumed: &consumed,
                pool_delegation_changes: &pool_deleg,
                drep_delegation_changes: &drep_deleg,
                pool_updates: &pool_updates,
                pool_retirements: &pool_retirements,
                issuer_pool_hash: issuer_pool_hash.as_deref(),
                stake_changes: &stake_changes,
                withdrawal_changes: &withdrawal_changes,
                reward_deltas: reward_deltas.as_ref(),
                drep_active_until: drep_active_until.as_ref(),
            });

            // ADA Handle: update handle cache in the latest snapshot
            if !handle_changes.is_empty() {
                if let Some(snap) = state.current_mut() {
                    for (handle, new_addr) in handle_changes {
                        let old_addr = snap.address_by_handle.get(&handle).cloned();
                        if old_addr.as_ref() == Some(&new_addr) {
                            continue; // no change
                        }
                        // Remove from old owner
                        if let Some(old) = old_addr {
                            if let Some(list) = snap.handle_by_address.get_mut(&old) {
                                list.retain(|h| h != &handle);
                                if list.is_empty() {
                                    snap.handle_by_address.remove(&old);
                                }
                            }
                        }
                        // Add to new owner
                        snap.handle_by_address
                            .entry(new_addr.clone())
                            .or_default()
                            .push(handle.clone());
                        snap.address_by_handle.insert(handle, new_addr);
                    }
                }
            }

            // CIP-68: update decimals in the latest snapshot
            if !new_decimals.is_empty() {
                if let Some(snap) = state.current_mut() {
                    for (fp, d) in new_decimals {
                        if d > 0 {
                            snap.decimals.insert(fp, d);
                        } else {
                            snap.decimals.remove(&fp);
                        }
                    }
                }
            }
        }

        let tx_count = txs.len();

        // Build the watched-asset change list before `txs` is moved into the Block
        // event (added assets take their decoded name from this block's outputs).
        let asset_changes: Vec<crate::event::AssetChange> = if asset_deltas.is_empty() {
            Vec::new()
        } else {
            let mut names: std::collections::HashMap<&str, Option<String>> =
                std::collections::HashMap::new();
            for tx in &txs {
                for out in &tx.outputs {
                    for a in &out.assets {
                        names
                            .entry(a.fingerprint.as_str())
                            .or_insert_with(|| a.name.clone());
                    }
                }
            }
            asset_deltas
                .into_iter()
                .map(|d| {
                    let name = if d.added {
                        names.get(d.fingerprint.as_str()).cloned().flatten()
                    } else {
                        None
                    };
                    let (address, cred) = match d.subject {
                        crate::state::AssetSubject::Address(a) => (Some(a), None),
                        crate::state::AssetSubject::Stake(c) => (None, Some(c)),
                    };
                    crate::event::AssetChange {
                        address,
                        cred,
                        fingerprint: d.fingerprint,
                        name,
                        added: d.added,
                    }
                })
                .collect()
        };

        stage
            .event_bus
            .send(Event::Block {
                slot,
                hash: block_hash,
                number: height,
                timestamp,
                pool_id,
                pool_ticker,
                txs,
            })
            .await;

        if !asset_changes.is_empty() {
            stage
                .event_bus
                .send(Event::AssetChanges {
                    slot,
                    changes: asset_changes,
                })
                .await;
        }

        let catchup = stage.catchup_target.load(Ordering::Relaxed);
        let mut catchup_complete = false;
        if catchup > 0 {
            if slot >= catchup {
                stage.catchup_target.store(0, Ordering::Relaxed);
                stage.catching_up.store(false, Ordering::Relaxed);
                catchup_complete = true;
                info!(slot, height, "catch-up complete");
            } else if height % 1000 == 0 {
                let remaining = (catchup - slot) / 20;
                info!(slot, height, remaining, "catching up");
            }
        } else {
            info!(slot, height, tx_count, "apply block");
        }

        // Skip periodic snapshots while catching up — they only slow the sync.
        // Save once when catch-up completes (so a restart doesn't repeat it),
        // then every SNAPSHOT_INTERVAL blocks during normal operation.
        if catchup_complete || (catchup == 0 && height % SNAPSHOT_INTERVAL == 0) {
            let state = stage.state.read().await;
            match state.save_snapshot(
                &stage.snapshot_path,
                stage.snapshot_depth,
                stage.genesis.magic,
            ) {
                Ok(saved_slot) => info!(saved_slot, "snapshot saved"),
                Err(e) => warn!("failed to save snapshot: {}", e),
            }
        }

        // Idle off-chain metadata refresh (pool tickers / DRep names) during normal
        // operation: a sub-ms MAX(id) gate, then read only rows newer than the cursor and
        // apply to the latest snapshot. Off-chain data db-sync fetches asynchronously isn't
        // in any block, so unlike decimals/handles it can't be derived per block — we poll.
        // Runs off the chain_state lock (db queries between a brief read and write guard).
        if catchup == 0 {
            let (pc, dc, db) = {
                let g = stage.state.read().await;
                (g.pool_meta_cursor, g.drep_meta_cursor, g.db_handle())
            };
            if let Some(db) = db {
                let pool = match db.max_pool_meta_id().await {
                    Ok(max) if max > pc => db.pool_ticker_updates(pc).await.ok().map(|r| (r, max)),
                    _ => None,
                };
                let drep = match db.max_drep_meta_id().await {
                    Ok(max) if max > dc => {
                        db.drep_metadata(i64::MAX, dc).await.ok().map(|d| (d, max))
                    }
                    _ => None,
                };
                if pool.is_some() || drep.is_some() {
                    let mut g = stage.state.write().await;
                    if let Some((rows, max)) = pool {
                        if let Some(snap) = g.current_mut() {
                            for (hash_raw, ticker, _) in rows {
                                if let Some(p) = snap.pools.get_mut(&hex::encode(&hash_raw)) {
                                    p.ticker = Some(ticker);
                                }
                            }
                        }
                        g.pool_meta_cursor = max;
                    }
                    if let Some((dreps, max)) = drep {
                        if let Some(snap) = g.current_mut() {
                            for (key, entry) in dreps {
                                if let Some(name) = entry.given_name {
                                    if let Some(target) = snap.dreps.get_mut(&key) {
                                        target.given_name = Some(name);
                                    }
                                }
                            }
                        }
                        g.drep_meta_cursor = max;
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(_stage: &Stage) -> Result<Self, WorkerError> {
        Ok(Self)
    }

    async fn schedule(
        &mut self,
        stage: &mut Stage,
    ) -> Result<WorkSchedule<ChainEvent>, WorkerError> {
        let msg = stage.input.recv().await.or_panic()?;
        Ok(WorkSchedule::Unit(msg.payload))
    }

    async fn execute(&mut self, unit: &ChainEvent, stage: &mut Stage) -> Result<(), WorkerError> {
        let point = unit.point();

        match unit {
            ChainEvent::Reset(point) => {
                info!("Reset to {:?}", point);
                self.handle_reset(point, stage).await?;
            }
            ChainEvent::Apply(_, Record::CborBlock(cbor)) => {
                self.handle_apply(cbor, stage).await?;
            }
            event => {
                warn!("Unexpected chain event {:?}", event);
            }
        }

        stage.ops_count.inc(1);
        stage.latest_block.set(point.slot_or_default() as i64);
        stage.cursor.send(point.clone().into()).await.or_panic()?;

        Ok(())
    }
}

#[derive(Stage)]
#[stage(name = "sink-fetcher", unit = "ChainEvent", worker = "Worker")]
pub struct Stage {
    genesis: GenesisValues,
    mainnet: bool,
    event_bus: Arc<EventBus>,
    state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,
    snapshot_path: PathBuf,
    snapshot_depth: usize,
    catchup_target: AtomicU64,
    /// Shared flag: set to false once catch-up is complete. SSE server waits on this.
    pub catching_up: Arc<std::sync::atomic::AtomicBool>,

    pub input: MapperInputPort,
    pub cursor: SinkCursorPort,

    #[metric]
    ops_count: gasket::metrics::Counter,

    #[metric]
    latest_block: gasket::metrics::Gauge,
}

/// Runtime handles + persistence config for the sink stage. Bundled so
/// `bootstrapper` takes the gasket context plus one config arg.
pub struct SinkConfig {
    pub event_bus: Arc<EventBus>,
    pub state: Arc<RwLock<State>>,
    pub nftcdn: NftcdnConfig,
    pub snapshot_path: PathBuf,
    pub snapshot_depth: usize,
    pub catchup_target: Option<u64>,
    pub catching_up: Arc<std::sync::atomic::AtomicBool>,
}

pub fn bootstrapper(context: &Context, config: SinkConfig) -> Result<Stage, Error> {
    let SinkConfig {
        event_bus,
        state,
        nftcdn,
        snapshot_path,
        snapshot_depth,
        catchup_target,
        catching_up,
    } = config;
    let genesis = GenesisValues::from(context.chain.clone());
    let mainnet = genesis.magic == 764824073;
    Ok(Stage {
        genesis,
        mainnet,
        event_bus,
        state,
        nftcdn,
        snapshot_path,
        snapshot_depth,
        catchup_target: AtomicU64::new(catchup_target.unwrap_or(0)),
        catching_up,
        ops_count: Default::default(),
        latest_block: Default::default(),
        input: Default::default(),
        cursor: Default::default(),
    })
}
