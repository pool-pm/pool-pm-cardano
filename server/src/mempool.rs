use gasket::framework::*;
use imbl::HashSet;
use pallas::ledger::traverse::MultiEraTx;
use pallas::network::facades::NodeClient;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::event::{AssetInfo, BlockTx, Event, TxInput, TxOutputInfo};
use crate::event_bus::EventBus;
use crate::model::{asset_fingerprint, TxOutput};
use crate::nftcdn::NftcdnConfig;
use crate::state::State;

pub async fn extract_tx(
    tx: &MultiEraTx<'_>,
    state: &State,
    nftcdn: &NftcdnConfig,
    block_utxos: &std::collections::HashMap<(Vec<u8>, i16), &TxOutput>,
) -> BlockTx {
    let hash = tx.hash().to_string();
    let fee = tx.fee().unwrap_or(0);
    let size = tx.size();

    let mut inputs = Vec::new();
    for input in tx.inputs() {
        let key = (input.hash().as_ref().to_vec(), input.index() as i16);
        if let Some(utxo) = block_utxos.get(&key) {
            inputs.push(TxInput {
                address: pallas::ledger::addresses::Address::from_bytes(&utxo.address)
                    .ok()
                    .and_then(|a| a.to_bech32().ok()),
                lovelace: utxo.lovelaces.try_into().ok().unwrap_or(0),
            });
        } else {
            inputs.push(
                state
                    .resolve_input(input.hash().as_ref(), input.index() as i16)
                    .await,
            );
        }
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
                            let fingerprint = asset_fingerprint(&policy_id, asset.name());
                            let name = std::str::from_utf8(asset.name())
                                .ok()
                                .filter(|s| !s.is_empty())
                                .map(String::from);
                            let tk = nftcdn.compute_tk(&fingerprint, "preview", 128);
                            Some(AssetInfo {
                                fingerprint,
                                name,
                                quantity: asset.output_coin()?,
                                tk,
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
                let block_tx = extract_tx(&tx, &state, &stage.nftcdn, &Default::default()).await;
                drop(state);
                stage.event_bus.send(Event::MempoolTx(block_tx)).await;

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
    event_bus: Arc<EventBus>,
    state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,

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
    event_bus: Arc<EventBus>,
    state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,
) -> Stage {
    Stage {
        config,
        event_bus,
        state,
        nftcdn,
        pending_count: Default::default(),
        snapshots: Default::default(),
    }
}
