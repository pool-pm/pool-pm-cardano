use gasket::framework::*;
use im::hashmap::HashMap;
use pallas::crypto::hash::Hasher;
use pallas::network::facades::NodeClient;
use std::path::PathBuf;
use tracing::info;

pub struct Worker {
    client: NodeClient,
    pending: HashMap<Vec<u8>, Vec<u8>>,
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(stage: &Stage) -> Result<Self, WorkerError> {
        let client = NodeClient::connect(&stage.config.socket_path, stage.config.magic)
            .await
            .or_retry()?;

        Ok(Self {
            client,
            pending: HashMap::new(),
        })
    }

    async fn schedule(&mut self, _stage: &mut Stage) -> Result<WorkSchedule<()>, WorkerError> {
        Ok(WorkSchedule::Unit(()))
    }

    async fn execute(&mut self, _unit: &(), stage: &mut Stage) -> Result<(), WorkerError> {
        let monitor = self.client.monitor();

        let slot = monitor.acquire().await.or_retry()?;

        let mut pending: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

        while let Some((_era, tagged_body)) = monitor.query_next_tx().await.or_retry()? {
            let body = tagged_body.0;
            let hash = Hasher::<256>::hash(&body);
            pending.insert(hash.to_vec(), body.to_vec());
        }

        let count = pending.len();
        self.pending = pending;

        stage.pending_count.set(count as i64);
        stage.snapshots.inc(1);

        info!(slot, count, "mempool snapshot");

        Ok(())
    }
}

#[derive(Stage)]
#[stage(name = "mempool-monitor", unit = "()", worker = "Worker")]
pub struct Stage {
    config: Config,

    #[metric]
    pending_count: gasket::metrics::Gauge,

    #[metric]
    snapshots: gasket::metrics::Counter,
}

pub struct Config {
    pub socket_path: PathBuf,
    pub magic: u64,
}

impl Config {
    pub fn bootstrapper(self) -> Stage {
        Stage {
            config: self,
            pending_count: Default::default(),
            snapshots: Default::default(),
        }
    }
}
