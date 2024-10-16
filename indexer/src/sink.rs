use gasket::framework::*;
use oura::framework::*;
use pallas::interop::utxorpc::spec::cardano::Block;
use pallas::network::miniprotocols::Point;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};
use url::Url;

use crate::dbsync::DbSync;
use crate::model::Pool;
use crate::utxorpc::BlockExt;

pub struct Worker {
    db: DbSync,
    chain: ChainConfig,
    pools: HashMap<String, Pool>,
    delegations: HashMap<Vec<u8>, Vec<u8>>,
    delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
}

impl Worker {
    async fn reset(&mut self, point: &Point) -> Result<(), WorkerError> {
        let slot = point.slot_or_default();
        let last_tx_id = self.db.last_slot_tx_id(slot).await.or_panic()?;
        info!("Last tx id: {last_tx_id}");

        info!("Fetching pools...");
        self.pools = self.db.pools(last_tx_id).await.or_panic()?;
        info!("{} pools retrieved", self.pools.len());

        info!("Fetching delegations...");
        (self.delegations, self.delegators) = self.db.delegations(last_tx_id).await.or_panic()?;
        info!(
            "{} delegations in {} pools retrieved",
            self.delegations.len(),
            self.delegators.len()
        );

        Ok(())
    }

    async fn apply_stake_delegations(&mut self, block: &Block) -> Result<(), WorkerError> {
        for delegation in block.stake_delegations(&self.chain).into_iter() {
            debug!(
                "delegation from stake {} to pool {}",
                hex::encode(&delegation.addr),
                hex::encode(&delegation.pool_keyhash)
            );
            self.delegations
                .insert(delegation.addr.clone(), delegation.pool_keyhash.clone())
                .map(|prev_pool| {
                    self.delegators.entry(prev_pool).and_modify(|delegators| {
                        delegators.remove(&delegation.addr);
                    });
                });
            self.delegators
                .entry(delegation.pool_keyhash)
                .or_default()
                .insert(delegation.addr);
        }

        Ok(())
    }

    async fn apply_stake_deregistrations(&mut self, block: &Block) -> Result<(), WorkerError> {
        for addr in block.stake_deregistrations(&self.chain).into_iter() {
            debug!("deregistration from stake {}", hex::encode(&addr));
            self.delegations.remove(&addr).map(|pool| {
                self.delegators.entry(pool).and_modify(|delegators| {
                    delegators.remove(&addr);
                });
            });
        }

        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(stage: &Stage) -> Result<Self, WorkerError> {
        let url = Url::parse(&stage.config.db_url).or_panic()?;

        Ok(Self {
            db: DbSync::new(&url).await.or_retry()?,
            chain: stage.chain.clone(),
            pools: HashMap::new(),
            delegations: HashMap::new(),
            delegators: HashMap::new(),
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
                self.reset(point).await?;
            }
            ChainEvent::Apply(point, Record::ParsedBlock(block)) => {
                info!("Apply block {:?}", point);
                self.apply_stake_delegations(block).await?;
                self.apply_stake_deregistrations(block).await?;
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
#[stage(name = "sink", unit = "ChainEvent", worker = "Worker")]
pub struct Stage {
    config: Config,
    chain: ChainConfig,

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
    pub fn bootstrapper(self, context: &Context) -> Result<Stage, Error> {
        let stage = Stage {
            config: self,
            chain: context.chain.clone(),
            ops_count: Default::default(),
            latest_block: Default::default(),
            input: Default::default(),
            cursor: Default::default(),
        };

        Ok(stage)
    }
}
