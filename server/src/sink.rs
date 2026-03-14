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
use crate::pallas::{stake_credential_from_address_bytes, MultiEraTxExt, PoolUpdate};
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
        let (txs, produced, consumed, pool_deleg, drep_deleg, pool_updates,
             stake_changes, withdrawal_changes, pool_id, pool_ticker) = {
            let state = stage.state.read().await;
            let mut txs = Vec::new();
            let mut consumed = Vec::new();
            let mut produced: std::collections::HashMap<(Vec<u8>, i16), TxOutput> =
                std::collections::HashMap::new();
            let mut pool_deleg: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
            let mut drep_deleg: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
            let mut pool_updates: Vec<PoolUpdate> = Vec::new();
            let mut stake_changes: Vec<(Vec<u8>, i64)> = Vec::new();
            let mut withdrawal_changes: Vec<(Vec<u8>, i64)> = Vec::new();

            for tx in block.txs() {
                let hash = tx.hash();

                // Track consumed UTXOs: subtract lovelaces from stake credentials
                for input in tx.inputs() {
                    let key = (input.hash().as_ref().to_vec(), input.index() as i16);
                    // Check block-local UTXOs first, then in-memory state
                    let resolved = if let Some(utxo) = produced.get(&key) {
                        Some((utxo.address.clone(), utxo.lovelaces))
                    } else {
                        state
                            .current()
                            .and_then(|s| s.utxos.get(&key))
                            .map(|utxo| (utxo.address.clone(), utxo.lovelaces))
                    };
                    if let Some((addr, lovelaces)) = resolved {
                        if let Some(cred) = stake_credential_from_address_bytes(&addr) {
                            let amount: i64 = lovelaces
                                .try_into()
                                .expect("lovelace value must fit i64");
                            stake_changes.push((cred, -amount));
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
                    let lovelaces = Decimal::from(output.value().coin());
                    if let Some(cred) = stake_credential_from_address_bytes(&addr) {
                        let amount: i64 = lovelaces
                            .try_into()
                            .expect("lovelace value must fit i64");
                        stake_changes.push((cred, amount));
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

                pool_deleg.extend(tx.pool_delegation_changes());
                drep_deleg.extend(tx.drep_delegation_changes());
                pool_updates.extend(tx.pool_updates());
            }

            let produced: Vec<_> = produced.into_iter().collect();

            let (pool_id, pool_ticker) = block
                .header()
                .issuer_vkey()
                .and_then(|vkey| {
                    let pool_hash = Hasher::<224>::hash(vkey);
                    state.current()?.pools.get(&hex::encode(pool_hash.as_ref()))
                })
                .map(|pool| (Some(pool_bech32_id(&pool.hash_raw)), pool.ticker.clone()))
                .unwrap_or((None, None));

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
                Ok(saved_slot) => info!(saved_slot, "snapshot saved"),
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
