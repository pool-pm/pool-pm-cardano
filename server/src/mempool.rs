use gasket::framework::*;
use imbl::HashSet;
use pallas::ledger::traverse::MultiEraTx;
use pallas::network::facades::NodeClient;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::event::{
    format_quantity, AssetInfo, BlockTx, DelegationInfo, Event, TxInput, TxOutputInfo, VoteInfo,
};
use crate::event_bus::EventBus;
use crate::filter;
use crate::model::{asset_fingerprint, drep_bech32_id, pool_bech32_id, TxOutput};
use crate::nftcdn::NftcdnConfig;
use crate::pallas::{
    extract_cip20_message, stake_address_bech32, stake_credential_bytes, MultiEraTxExt,
};
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
        let (address, lovelace, raw_assets) = if let Some(utxo) = block_utxos.get(&key) {
            (
                pallas::ledger::addresses::Address::from_bytes(&utxo.address)
                    .ok()
                    .map(|a| a.to_string()),
                utxo.lovelaces.try_into().ok().unwrap_or(0),
                utxo.assets.clone(),
            )
        } else {
            state
                .resolve_input(input.hash().as_ref(), input_index)
                .await
        };
        let assets = raw_assets
            .iter()
            .map(|(fp, raw)| {
                let decimals = state
                    .current()
                    .and_then(|s| s.decimals.get(fp).copied())
                    .unwrap_or(0);
                let tks = nftcdn.compute_ladder(fp, "preview");
                AssetInfo {
                    fingerprint: fp.clone(),
                    name: None,
                    quantity: format_quantity(*raw, decimals),
                    tks,
                    tk: None,
                    size: 0,
                }
            })
            .collect();
        let handle = address
            .as_ref()
            .and_then(|a| state.current().and_then(|s| s.handle_for(a)));
        inputs.push(TxInput {
            tx_hash: input_tx_hash,
            index: input_index,
            address,
            lovelace,
            assets,
            handle,
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
                            let raw = asset.output_coin()?;
                            let fingerprint = asset_fingerprint(&policy_id, asset.name());
                            let decimals = state
                                .current()
                                .and_then(|s| s.decimals.get(&fingerprint).copied())
                                .unwrap_or(0);
                            let name = std::str::from_utf8(asset.name())
                                .ok()
                                .filter(|s| !s.is_empty())
                                .map(String::from);
                            let tks = nftcdn.compute_ladder(&fingerprint, "preview");
                            Some(AssetInfo {
                                fingerprint,
                                name,
                                quantity: format_quantity(raw, decimals),
                                tks,
                                tk: None,
                                size: 0,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .collect();

            let handle = state.current().and_then(|s| s.handle_for(&address));
            TxOutputInfo {
                address,
                lovelace,
                assets,
                handle,
            }
        })
        .collect();

    let delegations = extract_delegations(tx, state, mainnet);

    let expiry = tx.ttl().map(|slot| slot_to_timestamp(slot, genesis));

    let mut withdrawals = Vec::new();
    for (addr, amount) in tx.withdrawals_sorted_set() {
        if addr.len() >= 29 {
            withdrawals.push((addr[1..29].to_vec(), amount));
            let stake_addr = pallas::ledger::addresses::Address::from_bytes(addr)
                .ok()
                .map(|a| a.to_string());
            inputs.push(TxInput {
                tx_hash: String::new(),
                index: -1,
                address: stake_addr,
                lovelace: amount,
                assets: vec![],
                handle: None,
            });
        }
    }

    let message = extract_cip20_message(tx);
    let votes = extract_votes(tx, state);

    let mut block_tx = BlockTx {
        hash,
        fee,
        size,
        inputs,
        outputs,
        expiry,
        delegations,
        votes,
        message,
        stake_change: None,
        stake_credentials: Vec::new(),
        withdrawals,
    };
    block_tx.stake_credentials = filter::extract_stake_credentials(&block_tx);
    block_tx
}

/// Resolve DRep bytes to (bech32_id, given_name).
fn resolve_drep(
    bytes: &[u8],
    snap: &crate::state::BlockSnapshot,
) -> (Option<String>, Option<String>) {
    let id = drep_bech32_id(bytes);
    let name = match bytes.first() {
        Some(0x02) => Some("Always Abstain".to_string()),
        Some(0x03) => Some("Always No Confidence".to_string()),
        _ => snap.dreps.get(bytes).and_then(|d| d.given_name.clone()),
    };
    (Some(id), name)
}

pub fn extract_votes(tx: &MultiEraTx<'_>, state: &State) -> Vec<VoteInfo> {
    use pallas::ledger::primitives::conway::{Vote, Voter};

    let snap = match state.current() {
        Some(s) => s,
        None => return Vec::new(),
    };

    tx.voting_procedures()
        .into_iter()
        .map(|(voter, action_id, vote)| {
            let (voter_role, voter_id, voter_name) = match &voter {
                Voter::ConstitutionalCommitteeKey(h) | Voter::ConstitutionalCommitteeScript(h) => {
                    ("CC".to_string(), hex::encode(h.as_ref()), None)
                }
                Voter::DRepKey(h) | Voter::DRepScript(h) => {
                    let bytes = if matches!(voter, Voter::DRepScript(_)) {
                        [&[0x01], h.as_ref()].concat()
                    } else {
                        [&[0x00], h.as_ref()].concat()
                    };
                    let (id, name) = resolve_drep(&bytes, snap);
                    ("DRep".to_string(), id.unwrap_or_default(), name)
                }
                Voter::StakePoolKey(h) => {
                    let pool_id = pool_bech32_id(h.as_ref());
                    let ticker = snap
                        .pools
                        .get(&hex::encode(h.as_ref()))
                        .and_then(|p| p.ticker.clone());
                    ("SPO".to_string(), pool_id, ticker)
                }
            };

            let action_tx_hash = hex::encode(action_id.transaction_id.as_ref());
            let action_key = format!("{}#{}", action_tx_hash, action_id.action_index);
            let action_title = snap.gov_action_titles.get(&action_key).cloned();

            let vote_str = match vote {
                Vote::Yes => "Yes",
                Vote::No => "No",
                Vote::Abstain => "Abstain",
            };

            VoteInfo {
                voter_role,
                voter_id,
                voter_name,
                vote: vote_str.to_string(),
                action_tx_hash,
                action_index: action_id.action_index,
                action_title,
            }
        })
        .collect()
}

/// Per-credential merge of pool + DRep delegation changes within one tx.
/// `cred_bytes -> (StakeCredential, pool_hash, drep_bytes)`; each `Option<Option<_>>`
/// is `Some(Some(x))` to set, `Some(None)` to deregister, `None` if unchanged.
type MergedDelegations<'a> = std::collections::HashMap<
    Vec<u8>,
    (
        Option<&'a pallas::ledger::primitives::conway::StakeCredential>,
        Option<Option<&'a Vec<u8>>>,
        Option<Option<&'a Vec<u8>>>,
    ),
>;

pub fn extract_delegations(
    tx: &MultiEraTx<'_>,
    state: &State,
    mainnet: bool,
) -> Vec<DelegationInfo> {
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

    // Collect pool delegation certs by credential
    let pool_certs = tx.pool_delegation_certs();
    // Collect DRep delegation changes by credential
    let drep_changes = tx.drep_delegation_changes();

    // Merge by credential: build a map of cred_bytes → (pool info, drep info, StakeCredential)
    let mut merged: MergedDelegations = std::collections::HashMap::new();

    for (cred, pool_hash) in &pool_certs {
        let cred_bytes = stake_credential_bytes(cred);
        let entry = merged.entry(cred_bytes).or_insert((None, None, None));
        entry.0 = Some(cred);
        entry.1 = Some(pool_hash.as_ref());
    }

    for (cred_bytes, drep_bytes) in &drep_changes {
        let entry = merged
            .entry(cred_bytes.clone())
            .or_insert((None, None, None));
        entry.2 = Some(drep_bytes.as_ref());
    }

    merged
        .into_iter()
        .filter_map(|(cred_bytes, (maybe_cred, pool_change, drep_change))| {
            // Need at least a pool or drep change with a target to show
            let has_pool = pool_change.is_some();
            let has_drep = drep_change.is_some();
            if !has_pool && !has_drep {
                return None;
            }

            let stake_address = if let Some(cred) = maybe_cred {
                stake_address_bech32(cred, mainnet)
            } else {
                // DRep-only change (e.g. VoteDeleg): build stake address from cred bytes
                crate::pallas::stake_address_from_cred_bytes(&cred_bytes, mainnet)
            };

            let (from_pool_id, from_ticker) = if has_pool {
                snap.pool_delegations
                    .get(&cred_bytes)
                    .map(|h| resolve_pool(h))
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };

            let (to_pool_id, to_ticker) = pool_change
                .flatten()
                .map(|h| resolve_pool(h))
                .unwrap_or((None, None));

            let (from_drep_id, from_drep_name) = if has_drep {
                snap.drep_delegations
                    .get(&cred_bytes)
                    .map(|h| resolve_drep(h, snap))
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };

            let (to_drep_id, to_drep_name) = drep_change
                .flatten()
                .map(|h| resolve_drep(h, snap))
                .unwrap_or((None, None));

            let live_stake = snap.stakes.get(&cred_bytes).copied().unwrap_or(0)
                + snap.rewards.get(&cred_bytes).copied().unwrap_or(0);

            Some(DelegationInfo {
                stake_address,
                from_pool_id,
                from_ticker,
                to_pool_id,
                to_ticker,
                from_drep_id,
                from_drep_name,
                to_drep_id,
                to_drep_name,
                live_stake,
            })
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
