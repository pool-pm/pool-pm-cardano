use gasket::framework::*;
use oura::framework::*;
use pallas::network::miniprotocols::Point;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::{debug, info};

use crate::dbsync::DbSync;
use crate::model::Pool;

pub struct Worker {
    db: DbSync,
    pools: HashMap<String, Pool>,
}

impl Worker {
    async fn reset(&mut self, point: &Point) -> Result<(), WorkerError> {
        info!("Reset to {:?}", point);
        let slot = point.slot_or_default();
        let max_tx_id = self.db.max_tx_id(slot).await.or_panic()?;
        info!("Last tx id: {max_tx_id}");

        info!("Fetching pools...");
        self.pools = self.db.pools(max_tx_id).await.or_panic()?;
        info!("{} pools retrieved", self.pools.len());

        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(stage: &Stage) -> Result<Self, WorkerError> {
        Ok(Self {
            db: DbSync::new(&stage.config.db_url).await.or_retry()?,
            pools: HashMap::new(),
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
                self.reset(point).await?;
            }
            ChainEvent::Apply(point, Record::ParsedBlock(_block)) => {
                debug!("Apply block {:?}", point);
            }
            ChainEvent::Undo(point, Record::ParsedBlock(_block)) => {
                debug!("Undo block {:?}", point);
            }
            event => {
                debug!("Unexpected chain event {:?}", event);
            }
        }

        stage.ops_count.inc(1);
        stage.latest_block.set(point.slot_or_default() as i64);
        stage.cursor.send(point.clone().into()).await.or_panic()?;

        Ok(())
    }
}

#[derive(Stage)]
#[stage(name = "sink", unit = "ChainEvent", worker = "Worker")]
pub struct Stage {
    config: Config,

    pub input: MapperInputPort,
    pub cursor: SinkCursorPort,

    #[metric]
    ops_count: gasket::metrics::Counter,

    #[metric]
    latest_block: gasket::metrics::Gauge,
}

#[derive(Default, Debug, Deserialize)]
pub struct Config {
    pub db_url: String,
}

impl Config {
    pub fn bootstrapper(self, _: &Context) -> Result<Stage, Error> {
        let stage = Stage {
            config: self,
            ops_count: Default::default(),
            latest_block: Default::default(),
            input: Default::default(),
            cursor: Default::default(),
        };

        Ok(stage)
    }
}
