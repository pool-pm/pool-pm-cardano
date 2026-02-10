use gasket::framework::*;
use oura::framework::*;
use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::miniprotocols::Point;
use sqlx::types::Decimal;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use pallas::crypto::hash::Hasher;

use crate::event::Event;
use crate::event_bus::EventBus;
use crate::mempool::extract_tx;
use crate::model::{pool_bech32_id, TxOutput};
use crate::nftcdn::NftcdnConfig;
use crate::state::State;

pub struct Worker;

impl Worker {
    async fn handle_reset(&self, point: &Point, stage: &Stage) -> Result<(), WorkerError> {
        let slot = point.slot_or_default();

        {
            let mut state = stage.state.write().await;
            if state.current().is_some() {
                state.rollback(slot);
            } else {
                state.reset(slot).await.or_panic()?;
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
        let (txs, produced, consumed, pool_id, pool_ticker) = {
            let state = stage.state.read().await;
            let mut txs = Vec::new();
            let mut consumed = Vec::new();
            let mut produced: std::collections::HashMap<(Vec<u8>, i16), TxOutput> =
                std::collections::HashMap::new();

            for tx in block.txs() {
                let hash = tx.hash();
                for input in tx.inputs() {
                    consumed.push((input.hash().as_ref().to_vec(), input.index() as i16));
                }
                txs.push(extract_tx(&tx, &state, &stage.nftcdn, &produced).await);
                for (idx, output) in tx.outputs().iter().enumerate() {
                    produced.insert(
                        (hash.as_ref().to_vec(), idx as i16),
                        TxOutput {
                            lovelaces: Decimal::from(output.value().coin()),
                            address: output.address().ok().map(|a| a.to_vec()).unwrap_or_default(),
                        },
                    );
                }
            }

            let produced: Vec<_> = produced.into_iter().collect();

            let (pool_id, pool_ticker) = block
                .header()
                .issuer_vkey()
                .and_then(|vkey| {
                    let pool_hash = Hasher::<224>::hash(vkey);
                    state.current()?.pools.get(&hex::encode(pool_hash.as_ref()))
                })
                .map(|pool| {
                    (
                        Some(pool_bech32_id(&pool.hash_raw)),
                        pool.ticker.clone(),
                    )
                })
                .unwrap_or((None, None));

            (txs, produced, consumed, pool_id, pool_ticker)
        };

        let timestamp = stage.genesis.shelley_known_time
            + slot.saturating_sub(stage.genesis.shelley_known_slot)
                * stage.genesis.shelley_slot_length as u64;

        {
            let mut state = stage.state.write().await;
            state.apply_block(slot, height, produced, &consumed);
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
    event_bus: Arc<EventBus>,
    state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,

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
) -> Result<Stage, Error> {
    Ok(Stage {
        genesis: GenesisValues::from(context.chain.clone()),
        event_bus,
        state,
        nftcdn,
        ops_count: Default::default(),
        latest_block: Default::default(),
        input: Default::default(),
        cursor: Default::default(),
    })
}
