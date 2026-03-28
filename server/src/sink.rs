use gasket::framework::*;
use oura::framework::*;
use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::miniprotocols::Point;
use sqlx::types::Decimal;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use pallas::crypto::hash::Hasher;

use crate::event::Event;
use crate::event_bus::EventBus;
use crate::mempool::extract_tx;
use crate::model::{pool_bech32_id, TxOutput};
use crate::nftcdn::NftcdnConfig;
use crate::pallas::{
    stake_address_bech32, stake_credential_bytes, stake_credential_from_address_bytes,
    MultiEraTxExt, PoolUpdate,
};
use crate::state::feed_index::{BlockRef, DelegationEntry, DelegationTarget};
use crate::state::State;

pub struct Worker;

impl Worker {
    async fn handle_reset(&self, point: &Point, stage: &Stage) -> Result<(), WorkerError> {
        let slot = point.slot_or_default();

        {
            let mut state = stage.state.write().await;
            if state.rollback(slot) {
                // Snapshot covered this slot, just truncate history
            } else {
                state.reset(slot, &stage.genesis).await.or_panic()?;
                match state.save_snapshot(&stage.snapshot_path, stage.snapshot_depth) {
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

        // Single pass: txs are ordered in a block, so chained tx outputs
        // are available for resolving later txs' inputs.
        let (
            txs,
            produced,
            consumed,
            pool_deleg,
            drep_deleg,
            pool_updates,
            stake_changes,
            withdrawal_changes,
            pool_id,
            pool_ticker,
            issuer_pool_hash,
            stake_change_pools,
            feed_delegations,
        ) = {
            let state = stage.state.read().await;
            let snap = state.current();
            let mut txs = Vec::new();
            let mut consumed = Vec::new();
            let mut produced: std::collections::HashMap<(Vec<u8>, i16), TxOutput> =
                std::collections::HashMap::new();
            let mut pool_deleg: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
            let mut drep_deleg: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
            let mut pool_updates: Vec<PoolUpdate> = Vec::new();
            let mut stake_changes: Vec<(Vec<u8>, i64)> = Vec::new();
            let mut withdrawal_changes: Vec<(Vec<u8>, i64)> = Vec::new();

            // Feed index: track pools with stake changes in this block
            let mut stake_change_pools: std::collections::HashSet<Vec<u8>> =
                std::collections::HashSet::new();

            // Feed index: collect raw delegation certs for building DelegationEntry
            struct RawDelegCert {
                tx_hash: String,
                cred: pallas::ledger::primitives::StakeCredential,
                cred_bytes: Vec<u8>,
                target_pool: Option<Vec<u8>>,
            }
            let mut raw_deleg_certs: Vec<RawDelegCert> = Vec::new();

            for tx in block.txs() {
                let hash = tx.hash();

                // Track consumed UTXOs: subtract lovelaces from stake credentials
                for input in tx.inputs() {
                    let key = (input.hash().as_ref().to_vec(), input.index() as i16);
                    // Check block-local UTXOs first, then in-memory state,
                    // then fall back to db-sync for pre-reset UTXOs
                    let resolved = if let Some(utxo) = produced.get(&key) {
                        Some((utxo.address.clone(), utxo.lovelaces))
                    } else if let Some(utxo) = snap.and_then(|s| s.utxos.get(&key)) {
                        Some((utxo.address.clone(), utxo.lovelaces))
                    } else {
                        let (addr_str, lovelace) = state
                            .resolve_input(input.hash().as_ref(), input.index() as i16)
                            .await;
                        addr_str
                            .and_then(|a| {
                                pallas::ledger::addresses::Address::from_bech32(&a)
                                    .ok()
                                    .map(|a| a.to_vec())
                            })
                            .map(|addr| (addr, Decimal::from(lovelace)))
                    };
                    if let Some((addr, lovelaces)) = resolved {
                        if let Some(cred) = stake_credential_from_address_bytes(&addr) {
                            let amount: i64 =
                                lovelaces.try_into().expect("lovelace value must fit i64");
                            stake_changes.push((cred.clone(), -amount));

                            // Feed index: track pool for any consumed input
                            if let Some(pool) = snap.and_then(|s| s.pool_delegations.get(&cred)) {
                                stake_change_pools.insert(pool.clone());
                            }
                        }
                    }
                    consumed.push(key);
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

                        // Feed index: track pool for outputs > 1000 ADA
                        if coin > 1_000_000_000 {
                            if let Some(pool) = snap.and_then(|s| s.pool_delegations.get(&cred)) {
                                stake_change_pools.insert(pool.clone());
                            }
                        }
                    }
                    produced.insert(
                        (hash.as_ref().to_vec(), idx as i16),
                        TxOutput {
                            lovelaces,
                            address: addr,
                        },
                    );
                }

                // Track withdrawals (reduce reward balance)
                for (reward_addr, amount) in tx.withdrawals_sorted_set() {
                    if reward_addr.len() >= 29 {
                        let cred = reward_addr[1..29].to_vec();
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
                        cred: cred.clone(),
                        cred_bytes,
                        target_pool: target_pool.clone(),
                    });
                }

                drep_deleg.extend(tx.drep_delegation_changes());
                pool_updates.extend(tx.pool_updates());
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
                let resolve_pool_target = |pool_hash: &[u8]| -> DelegationTarget {
                    DelegationTarget {
                        raw: pool_hash.to_vec(),
                        id: pool_bech32_id(pool_hash),
                        label: s
                            .pools
                            .get(&hex::encode(pool_hash))
                            .and_then(|p| p.ticker.clone()),
                    }
                };

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
                        block_pool_hash: issuer_pool_hash.clone().unwrap_or_default(),
                        block_pool_ticker: pool_ticker.clone(),
                        tx_hash: cert.tx_hash.clone(),
                        stake_address: stake_address_bech32(&cert.cred, stage.mainnet),
                        stake_cred: cert.cred_bytes.clone(),
                        live_stake,
                        from: from_pool.map(|h| resolve_pool_target(h)),
                        to: cert.target_pool.as_ref().map(|h| resolve_pool_target(h)),
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
                stake_changes,
                withdrawal_changes,
                pool_id,
                pool_ticker,
                issuer_pool_hash,
                stake_change_pools,
                feed_delegations,
            )
        };

        let timestamp = crate::mempool::slot_to_timestamp(slot, &stage.genesis);
        let epoch = State::epoch_for_slot(slot, &stage.genesis);

        // Check for epoch boundary and fetch reward deltas from db-sync
        let reward_deltas = {
            let state = stage.state.read().await;
            let last_epoch = state.current().and_then(|s| s.last_epoch);
            if last_epoch.is_some() && last_epoch != Some(epoch) {
                info!(epoch, "epoch boundary detected, fetching reward deltas");
                state.epoch_reward_delta(epoch).await
            } else {
                None
            }
        };

        {
            let mut state = stage.state.write().await;

            // Update feed index BEFORE apply_block (pre-block state for delegation from_pool)
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
                    .add_pool_stake_changes(stake_change_pools, block_ref);
            }

            for entry in feed_delegations {
                state.feed_index.add_delegation_event(entry);
            }

            // Prune entries older than one epoch
            let prune_boundary = slot.saturating_sub(stage.genesis.shelley_epoch_length as u64);
            state.feed_index.prune(prune_boundary);

            state.apply_block(
                slot,
                block_hash.clone(),
                produced,
                &consumed,
                &pool_deleg,
                &drep_deleg,
                &pool_updates,
                &stake_changes,
                &withdrawal_changes,
                epoch,
                reward_deltas.as_ref(),
            );
        }

        let tx_count = txs.len();
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

        info!(slot, height, tx_count, "apply block");

        if height % 50 == 0 {
            let state = stage.state.read().await;
            match state.save_snapshot(&stage.snapshot_path, stage.snapshot_depth) {
                Ok(saved_slot) => {
                    let fi_path = stage.snapshot_path.with_file_name("feed_index.bin");
                    if let Err(e) = state.feed_index.save(&fi_path) {
                        warn!("failed to save feed index: {}", e);
                    }
                    info!(saved_slot, "snapshot saved");
                }
                Err(e) => warn!("failed to save snapshot: {}", e),
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

    pub input: MapperInputPort,
    pub cursor: SinkCursorPort,

    #[metric]
    ops_count: gasket::metrics::Counter,

    #[metric]
    latest_block: gasket::metrics::Gauge,
}

pub fn bootstrapper(
    context: &Context,
    event_bus: Arc<EventBus>,
    state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,
    snapshot_path: PathBuf,
    snapshot_depth: usize,
) -> Result<Stage, Error> {
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
        ops_count: Default::default(),
        latest_block: Default::default(),
        input: Default::default(),
        cursor: Default::default(),
    })
}
