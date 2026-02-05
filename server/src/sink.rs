use gasket::framework::*;
use oura::framework::*;
use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::miniprotocols::Point;
use sqlx::types::Decimal;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use crate::event::Event;
use crate::model::TxOutput;
use crate::state::State;

pub struct Worker {
    chain: ChainConfig,
}

impl Worker {
    async fn handle_reset(&mut self, point: &Point, stage: &Stage) -> Result<(), WorkerError> {
        let slot = point.slot_or_default();

        {
            let mut state = stage.state.write().await;
            state.reset(slot).await.or_panic()?;
        }

        let _ = stage.event_tx.send(Event::Rollback { slot });

        Ok(())
    }

    async fn handle_apply(&mut self, cbor: &[u8], stage: &Stage) -> Result<(), WorkerError> {
        let block = MultiEraBlock::decode(cbor).or_panic()?;
        let slot = block.slot();
        let height = block.number();
        let block_hash = block.hash().to_string();

        let mut tx_hashes = Vec::new();
        let mut produced = Vec::new();
        let mut consumed = Vec::new();

        for tx in block.txs() {
            let tx_hash = tx.hash();
            tx_hashes.push(tx_hash.to_string());

            for input in tx.inputs() {
                consumed.push((input.hash().as_ref().to_vec(), input.index() as i16));
            }

            for (idx, output) in tx.outputs().iter().enumerate() {
                let address = output
                    .address()
                    .ok()
                    .map(|a| a.to_vec())
                    .unwrap_or_default();
                let lovelaces = Decimal::from(output.value().coin());
                produced.push((
                    (tx_hash.as_ref().to_vec(), idx as i16),
                    TxOutput { lovelaces, address },
                ));
            }
        }

        let genesis = GenesisValues::from(self.chain.clone());
        let timestamp = genesis.shelley_known_time
            + slot.saturating_sub(genesis.shelley_known_slot)
                * genesis.shelley_slot_length as u64;

        {
            let mut state = stage.state.write().await;
            state.apply_block(slot, height, produced, &consumed);
        }

        let tx_count = tx_hashes.len();
        let _ = stage.event_tx.send(Event::Block {
            slot,
            hash: block_hash,
            number: height,
            timestamp,
            tx_hashes,
        });

        info!(slot, height, tx_count, "apply block");

        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(stage: &Stage) -> Result<Self, WorkerError> {
        Ok(Self {
            chain: stage.chain.clone(),
        })
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
    chain: ChainConfig,
    event_tx: broadcast::Sender<Event>,
    state: Arc<RwLock<State>>,

    pub input: MapperInputPort,
    pub cursor: SinkCursorPort,

    #[metric]
    ops_count: gasket::metrics::Counter,

    #[metric]
    latest_block: gasket::metrics::Gauge,
}

pub fn bootstrapper(
    context: &Context,
    event_tx: broadcast::Sender<Event>,
    state: Arc<RwLock<State>>,
) -> Result<Stage, Error> {
    Ok(Stage {
        chain: context.chain.clone(),
        event_tx,
        state,
        ops_count: Default::default(),
        latest_block: Default::default(),
        input: Default::default(),
        cursor: Default::default(),
    })
}
