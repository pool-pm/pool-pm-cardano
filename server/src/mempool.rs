use gasket::framework::*;
use imbl::hashmap::HashMap;
use pallas::ledger::traverse::MultiEraTx;
use pallas::network::facades::NodeClient;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::info;
use url::Url;

use crate::dbsync::DbSync;
use crate::event::{AssetInfo, Event, TxInput, TxOutputInfo};
use crate::model::asset_fingerprint;
use crate::state::State;

async fn extract_tx(
    tx: &MultiEraTx<'_>,
    state_lock: &Arc<RwLock<State>>,
    db: &DbSync,
) -> Event {
    let hash = tx.hash().to_string();
    let fee = tx.fee().unwrap_or(0);
    let size = tx.size();

    // Collect raw input refs
    let raw_inputs: Vec<_> = tx
        .inputs()
        .iter()
        .map(|i| (i.hash().as_ref().to_vec(), i.index() as i16))
        .collect();

    // Phase 1: resolve from in-memory state (single read lock)
    let mut resolved: Vec<Option<TxInput>> = {
        let state = state_lock.read().await;
        raw_inputs
            .iter()
            .map(|(tx_hash, idx)| {
                state.resolve_input(tx_hash, *idx).map(|u| TxInput {
                    address: pallas::ledger::addresses::Address::from_bytes(&u.address)
                        .ok()
                        .and_then(|a| a.to_bech32().ok()),
                    lovelace: u.lovelaces.try_into().ok().unwrap_or(0),
                })
            })
            .collect()
    };

    // Phase 2: db-sync fallback for unresolved inputs
    for (i, result) in resolved.iter_mut().enumerate() {
        if result.is_none() {
            let (tx_hash, idx) = &raw_inputs[i];
            if let Ok(Some((address, value))) = db.resolve_utxo(tx_hash, *idx).await {
                *result = Some(TxInput {
                    address: Some(address),
                    lovelace: value.try_into().ok().unwrap_or(0),
                });
            }
        }
    }

    let inputs: Vec<TxInput> = resolved
        .into_iter()
        .map(|r| r.unwrap_or(TxInput { address: None, lovelace: 0 }))
        .collect();

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

    Event::MempoolTx {
        hash,
        fee,
        size,
        inputs,
        outputs,
    }
}

pub struct Worker {
    client: NodeClient,
    db: DbSync,
    pending: HashMap<String, ()>,
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(stage: &Stage) -> Result<Self, WorkerError> {
        let client = NodeClient::connect(&stage.config.socket_path, stage.config.magic)
            .await
            .or_retry()?;

        let url = Url::parse(&stage.config.db_url).or_panic()?;
        let db = DbSync::new(&url).await.or_retry()?;

        Ok(Self {
            client,
            db,
            pending: HashMap::new(),
        })
    }

    async fn schedule(&mut self, _stage: &mut Stage) -> Result<WorkSchedule<()>, WorkerError> {
        Ok(WorkSchedule::Unit(()))
    }

    async fn execute(&mut self, _unit: &(), stage: &mut Stage) -> Result<(), WorkerError> {
        let monitor = self.client.monitor();

        let slot = monitor.acquire().await.or_retry()?;

        let mut pending: HashMap<String, ()> = HashMap::new();

        while let Some((_era, tagged_body)) = monitor.query_next_tx().await.or_retry()? {
            let body = tagged_body.0.to_vec();
            let tx = MultiEraTx::decode(&body).or_panic()?;
            let hash = tx.hash().to_string();

            if !self.pending.contains_key(&hash) {
                let event = extract_tx(&tx, &stage.state, &self.db).await;
                let _ = stage.event_tx.send(event);

                info!(
                    hash,
                    fee = tx.fee(),
                    inputs = tx.inputs().len(),
                    outputs = tx.outputs().len(),
                    "new mempool tx"
                );
            }

            pending.insert(hash, ());
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
    pub db_url: String,
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
