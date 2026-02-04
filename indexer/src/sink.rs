use gasket::framework::*;
use im::{hashmap::HashMap, hashset::HashSet};
use oura::framework::*;
use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::miniprotocols::Point;
use serde::Deserialize;
use sqlx::types::Decimal;
use tracing::{info, warn};
use url::Url;

use crate::dbsync::DbSync;
use crate::model::{Pool, TxOutput};

pub struct Worker {
    db: DbSync,
    chain: ChainConfig,
    pools: HashMap<String, Pool>,
    delegations: HashMap<Vec<u8>, Vec<u8>>,
    delegators: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    utxos: HashMap<(Vec<u8>, i16), TxOutput>,
    stakes: HashMap<Vec<u8>, Decimal>,
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

        info!("Fetching utxos...");
        (self.utxos, self.stakes) = self.db.utxos(last_tx_id).await.or_panic()?;
        info!(
            "{} utxos and {} stakes retrieved",
            self.utxos.len(),
            self.stakes.len()
        );

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
            utxos: HashMap::new(),
            stakes: HashMap::new(),
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
            ChainEvent::Apply(point, Record::CborBlock(cbor)) => {
                let _block = MultiEraBlock::decode(&cbor).or_panic()?;
                info!("Apply block {:?}", point);
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
