use gasket::framework::*;
use imbl::HashSet;
use pallas::ledger::traverse::MultiEraTx;
use pallas::network::facades::NodeClient;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::event::{AssetInfo, BlockTx, DelegationInfo, Event, TxInput, TxOutputInfo};
use crate::event_bus::EventBus;
use crate::filter;
use crate::model::{asset_fingerprint, pool_bech32_id, TxOutput};
use crate::nftcdn::NftcdnConfig;
use crate::pallas::{stake_address_bech32, stake_credential_bytes, MultiEraTxExt};
use crate::state::State;

pub fn slot_to_timestamp(slot: u64, genesis: &oura::framework::GenesisValues) -> u64 {
    genesis.shelley_known_time
        + slot.saturating_sub(genesis.shelley_known_slot) * genesis.shelley_slot_length as u64
}

pub async fn extract_tx(
    tx: &MultiEraTx<'_>,
    state: &State,
    nftcdn: &NftcdnConfig,
    block_utxos: &std::collections::HashMap<(Vec<u8>, i16), TxOutput>,
    mainnet: bool,
    genesis: &oura::framework::GenesisValues,
) -> BlockTx {
    let hash = tx.hash().to_string();
    let fee = tx.fee().unwrap_or(0);
    let size = tx.size();

    let mut inputs = Vec::new();
    for input in tx.inputs() {
        let input_tx_hash = input.hash().to_string();
        let input_index = input.index() as i16;
        let key = (input.hash().as_ref().to_vec(), input_index);
        let (address, lovelace) = if let Some(utxo) = block_utxos.get(&key) {
            (
                pallas::ledger::addresses::Address::from_bytes(&utxo.address)
                    .ok()
                    .map(|a| a.to_string()),
                utxo.lovelaces.try_into().ok().unwrap_or(0),
            )
        } else {
            state
                .resolve_input(input.hash().as_ref(), input_index)
                .await
        };
        inputs.push(TxInput {
            tx_hash: input_tx_hash,
            index: input_index,
            address,
            lovelace,
        });
    }

    let outputs: Vec<TxOutputInfo> = tx
        .outputs()
        .iter()
        .map(|output| {
            let address = output
                .address()
                .ok()
                .map(|a| a.to_string())
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

    let delegations = extract_delegations(tx, state, mainnet);

    let expiry = tx.ttl().map(|slot| slot_to_timestamp(slot, genesis));

    let mut block_tx = BlockTx {
        hash,
        fee,
        size,
        inputs,
        outputs,
        expiry,
        delegations,
        stake_credentials: Vec::new(),
    };
    block_tx.stake_credentials = filter::extract_stake_credentials(&block_tx);
    block_tx
}

fn extract_delegations(tx: &MultiEraTx<'_>, state: &State, mainnet: bool) -> Vec<DelegationInfo> {
    let snap = match state.current() {
        Some(s) => s,
        None => return Vec::new(),
    };

    let resolve_pool = |hash: &[u8]| -> (Option<String>, Option<String>) {
        let pool = snap.pools.get(&hex::encode(hash));
        (
            Some(pool_bech32_id(hash)),
            pool.and_then(|p| p.ticker.clone()),
        )
    };

    tx.pool_delegation_certs()
        .iter()
        .map(|(cred, pool_hash)| {
            let cred_bytes = stake_credential_bytes(cred);

            let (from_pool_id, from_ticker) = snap
                .pool_delegations
                .get(&cred_bytes)
                .map(|h| resolve_pool(h))
                .unwrap_or((None, None));

            let (to_pool_id, to_ticker) = pool_hash
                .as_ref()
                .map(|h| resolve_pool(h))
                .unwrap_or((None, None));

            let live_stake = snap.stakes.get(&cred_bytes).copied().unwrap_or(0)
                + snap.rewards.get(&cred_bytes).copied().unwrap_or(0);

            DelegationInfo {
                stake_address: stake_address_bech32(cred, mainnet),
                from_pool_id,
                from_ticker,
                to_pool_id,
                to_ticker,
                live_stake,
            }
        })
        .collect()
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
                let block_tx = extract_tx(
                    &tx,
                    &state,
                    &stage.nftcdn,
                    &Default::default(),
                    stage.config.mainnet,
                    &stage.config.genesis,
                )
                .await;
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

        let removed: Vec<String> = self
            .pending
            .iter()
            .filter(|h| !pending.contains(h.as_str()))
            .cloned()
            .collect();
        if !removed.is_empty() {
            stage.event_bus.send(Event::MempoolPrune { removed }).await;
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
    pub mainnet: bool,
    pub genesis: oura::framework::GenesisValues,
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
