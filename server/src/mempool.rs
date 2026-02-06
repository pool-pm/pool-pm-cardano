use gasket::framework::*;
use imbl::HashSet;
use pallas::ledger::traverse::MultiEraTx;
use pallas::network::facades::NodeClient;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::info;

use crate::event::{AssetInfo, BlockTx, Event, TxOutputInfo};
use crate::model::asset_fingerprint;
use crate::state::State;

pub async fn extract_tx(tx: &MultiEraTx<'_>, state: &State) -> BlockTx {
    let hash = tx.hash().to_string();
    let fee = tx.fee().unwrap_or(0);
    let size = tx.size();

    let mut inputs = Vec::new();
    for input in tx.inputs() {
        inputs.push(
            state
                .resolve_input(input.hash().as_ref(), input.index() as i16)
                .await,
        );
    }

    let outputs: Vec<TxOutputInfo> = tx
        .outputs()
        .iter()
        .map(|output| {
            let address = output
                .address()
                .ok()
                .and_then(|a| a.to_bech32().ok())
                .unwrap_or_default();
            let lovelace = output.value().coin();
            let assets: Vec<AssetInfo> = output
                .value()
                .assets()
                .iter()
                .flat_map(|policy_assets| {
                    let policy_id = policy_assets.policy().as_ref().to_vec();
                    policy_assets
                        .assets()
                        .iter()
                        .filter_map(|asset| {
                            Some(AssetInfo {
                                fingerprint: asset_fingerprint(&policy_id, asset.name()),
                                quantity: asset.output_coin()?,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .collect();

            TxOutputInfo {
                address,
                lovelace,
                assets,
            }
        })
        .collect();

    BlockTx {
        hash,
        fee,
        size,
        inputs,
        outputs,
    }
}

pub struct Worker {
    client: NodeClient,
    pending: HashSet<String>,
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(stage: &Stage) -> Result<Self, WorkerError> {
        let client = NodeClient::connect(&stage.config.socket_path, stage.config.magic)
            .await
            .or_retry()?;

        Ok(Self {
            client,
            pending: HashSet::new(),
        })
    }

    async fn schedule(&mut self, _stage: &mut Stage) -> Result<WorkSchedule<()>, WorkerError> {
        Ok(WorkSchedule::Unit(()))
    }

    async fn execute(&mut self, _unit: &(), stage: &mut Stage) -> Result<(), WorkerError> {
        let monitor = self.client.monitor();

        let slot = monitor.acquire().await.or_retry()?;

        let mut pending: HashSet<String> = HashSet::new();

        while let Some((_era, tagged_body)) = monitor.query_next_tx().await.or_retry()? {
            let body = tagged_body.0.to_vec();
            let tx = MultiEraTx::decode(&body).or_panic()?;
            let hash = tx.hash().to_string();

            if !self.pending.contains(&hash) {
                let state = stage.state.read().await;
                let block_tx = extract_tx(&tx, &state).await;
                drop(state);
                let _ = stage.event_tx.send(Event::MempoolTx(block_tx));

                info!(
                    hash,
                    fee = tx.fee(),
                    inputs = tx.inputs().len(),
                    outputs = tx.outputs().len(),
                    "new mempool tx"
                );
            }

            pending.insert(hash);
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
    event_tx: broadcast::Sender<Event>,
    state: Arc<RwLock<State>>,

    #[metric]
    pending_count: gasket::metrics::Gauge,

    #[metric]
    snapshots: gasket::metrics::Counter,
}

pub struct Config {
    pub socket_path: PathBuf,
    pub magic: u64,
}

pub fn bootstrapper(
    config: Config,
    event_tx: broadcast::Sender<Event>,
    state: Arc<RwLock<State>>,
) -> Stage {
    Stage {
        config,
        event_tx,
        state,
        pending_count: Default::default(),
        snapshots: Default::default(),
    }
}
