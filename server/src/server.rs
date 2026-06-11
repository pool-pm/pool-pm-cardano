use axum::{
    http::StatusCode,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::get,
    Router,
};
use futures::stream::Stream;
use pallas::crypto::hash::Hasher;
use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::facades::PeerClient;
use pallas::network::miniprotocols::Point;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

use crate::event::{format_quantity, AssetInfo, BlockTx, DelegationInfo, TxInput, TxOutputInfo};
use crate::event_bus::EventBus;
use crate::filter;
use crate::model::{asset_fingerprint, drep_bech32_id, pool_bech32_id, Pool, TxOutput};
use crate::nftcdn::{rung_for_dpr, NftcdnConfig, SIZE_LADDER};
use crate::state::feed_index::BlockRef;
use crate::state::{BlockSnapshot, State};

#[derive(Clone)]
struct AppState {
    bus: Arc<EventBus>,
    chain_state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,
    http: reqwest::Client,
    genesis: GenesisConfig,
    n2n_addr: SocketAddr,
    magic: u64,
    mainnet: bool,
}

#[derive(Clone, serde::Serialize)]
pub struct GenesisConfig {
    pub shelley_known_slot: u64,
    pub shelley_known_time: u64,
    pub shelley_slot_length: u32,
    pub byron_epoch_length: u32,
    pub byron_slot_length: u32,
    pub shelley_epoch_length: u32,
}

fn slot_to_timestamp(slot: u64, genesis: &GenesisConfig) -> u64 {
    genesis.shelley_known_time
        + slot.saturating_sub(genesis.shelley_known_slot) * genesis.shelley_slot_length as u64
}

/// Maximum number of blocks to replay on feed connection. Must match
/// `MAX_BLOCKS` in `web/src/lib/components/Feed.svelte`.
const MAX_REPLAY_BLOCKS: usize = 30;

/// Minimum stake change (as fraction of live stake) to include a block in feed
/// replay. Must match `STAKE_CHANGE_PRUNE_DIVISOR` in Feed.svelte.
const STAKE_CHANGE_DIVISOR: u64 = 1_000; // 0.1%

/// Recent blocks to replay on a stake-address feed connection (fetched from
/// db-sync, since stake addresses are not pre-indexed in memory).
const STAKE_REPLAY_BLOCKS: i64 = 30;

// --- SSE event builders ---

fn config_event(
    nftcdn_subdomain: &str,
    genesis: &GenesisConfig,
    magic: u64,
) -> Result<SseEvent, Infallible> {
    let genesis_json = serde_json::to_string(genesis).unwrap();
    Ok(SseEvent::default().data(format!(
        "{{\"type\":\"Config\",\"nftcdn\":\"{}\",\"magic\":{},\"genesis\":{}}}",
        nftcdn_subdomain, magic, genesis_json
    )))
}

fn pool_sse_event(pool: &Pool, snap: Option<&BlockSnapshot>) -> Result<SseEvent, Infallible> {
    let live_stake = snap
        .and_then(|s| State::pool_live_stake(s, &pool.hash_raw))
        .map(|v| format!(r#","live_stake":"{}""#, v))
        .unwrap_or_default();
    let delegators = snap
        .and_then(|s| s.pool_delegators.get(&pool.hash_raw))
        .map(|d| format!(r#","delegators":{}"#, d.len()))
        .unwrap_or_default();
    Ok(SseEvent::default().data(format!(
        r#"{{"type":"Pool","pool_id":"{}","ticker":{},"pledge":"{}","margin":{},"fixed_cost":"{}"{}{}}}"#,
        pool_bech32_id(&pool.hash_raw),
        serde_json::to_string(&pool.ticker).unwrap(),
        pool.pledge,
        pool.margin,
        pool.fixed_cost,
        live_stake,
        delegators
    )))
}

fn drep_sse_event(drep_bytes: &[u8], snap: Option<&BlockSnapshot>) -> Result<SseEvent, Infallible> {
    let drep_id = drep_bech32_id(drep_bytes);
    let given_name = match drep_bytes.first() {
        Some(0x02) => Some("Always Abstain".to_string()),
        Some(0x03) => Some("Always No Confidence".to_string()),
        _ => snap
            .and_then(|s| s.dreps.get(drep_bytes))
            .and_then(|d| d.given_name.clone()),
    };
    let live_stake = snap
        .and_then(|s| State::drep_live_stake(s, drep_bytes))
        .map(|v| format!(r#","live_stake":"{}""#, v))
        .unwrap_or_default();
    let delegators = snap
        .and_then(|s| s.drep_delegators.get(drep_bytes))
        .map(|d| format!(r#","delegators":{}"#, d.len()))
        .unwrap_or_default();
    Ok(SseEvent::default().data(format!(
        r#"{{"type":"DRep","drep_id":"{}","given_name":{}{}{}}}"#,
        drep_id,
        serde_json::to_string(&given_name).unwrap(),
        live_stake,
        delegators
    )))
}

#[derive(serde::Serialize)]
struct StakeEvent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    stake_address: &'a str,
    /// String-encoded (can exceed JS Number.MAX_SAFE_INTEGER).
    balance: String,
    rewards: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pool_ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drep_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drep_name: Option<String>,
    /// Distinct multi-assets held across every payment address sharing this
    /// credential. Computed once via db-sync at connect; not updated live.
    assets_count: u32,
}

/// Build a `Stake` info event for a stake feed: ADA balance, available rewards,
/// current pool/drep delegation (all snapshot-live), plus a connect-time
/// `assets_count` the caller passes through unchanged on every emit.
fn stake_sse_event(
    stake_address: &str,
    cred: &[u8],
    snap: Option<&BlockSnapshot>,
    assets_count: u32,
) -> Result<SseEvent, Infallible> {
    let balance = snap.and_then(|s| s.stakes.get(cred).copied()).unwrap_or(0);
    let rewards = snap.and_then(|s| s.rewards.get(cred).copied()).unwrap_or(0);
    let (pool_id, pool_ticker) = match snap.and_then(|s| s.pool_delegations.get(cred)) {
        Some(hash) => {
            let ticker = snap
                .and_then(|s| s.pools.get(&hex::encode(hash)))
                .and_then(|p| p.ticker.clone());
            (Some(pool_bech32_id(hash)), ticker)
        }
        None => (None, None),
    };
    let (drep_id, drep_name) = match snap.and_then(|s| s.drep_delegations.get(cred)) {
        Some(bytes) => {
            let name = match bytes.first() {
                Some(0x02) => Some("Always Abstain".to_string()),
                Some(0x03) => Some("Always No Confidence".to_string()),
                _ => snap
                    .and_then(|s| s.dreps.get(bytes))
                    .and_then(|d| d.given_name.clone()),
            };
            (Some(drep_bech32_id(bytes)), name)
        }
        None => (None, None),
    };
    let json = serde_json::to_string(&StakeEvent {
        kind: "Stake",
        stake_address,
        balance: balance.to_string(),
        rewards: rewards.to_string(),
        pool_id,
        pool_ticker,
        drep_id,
        drep_name,
        assets_count,
    })
    .unwrap();
    Ok(SseEvent::default().data(json))
}

/// Send current stake-address info as an SSE event.
async fn send_stake_info(
    sender: &Sender<Result<SseEvent, Infallible>>,
    chain_state: &RwLock<State>,
    stake_address: &str,
    cred: &[u8],
    assets_count: u32,
) {
    let guard = chain_state.read().await;
    let _ = sender
        .send(stake_sse_event(
            stake_address,
            cred,
            guard.current(),
            assets_count,
        ))
        .await;
}

/// The bech32 stake (reward) address of a payment address, or `None` for an
/// address with no stake part (enterprise/pointer). Preserves the key/script
/// credential type and network so it round-trips to db-sync.
/// The 29-byte reward-address `hash_raw` (network+type header + 28-byte stake
/// credential) for a payment address's stake part — matching db-sync's
/// `stake_address.hash_raw`. `None` for enterprise addresses (no stake part) or
/// non-Shelley addresses.
fn stake_hash_raw_of(address: &str, mainnet: bool) -> Option<Vec<u8>> {
    use pallas::ledger::addresses::{Address, ShelleyDelegationPart};
    let Address::Shelley(sh) = Address::from_bech32(address).ok()? else {
        return None;
    };
    let (is_script, hash) = match sh.delegation() {
        ShelleyDelegationPart::Key(h) => (false, h.as_ref().to_vec()),
        ShelleyDelegationPart::Script(h) => (true, h.as_ref().to_vec()),
        _ => return None,
    };
    let net = if mainnet { 1u8 } else { 0u8 };
    let mut payload = Vec::with_capacity(29);
    payload.push(if is_script { 0xf0 | net } else { 0xe0 | net });
    payload.extend_from_slice(&hash);
    Some(payload)
}

fn stake_address_of(address: &str, mainnet: bool) -> Option<String> {
    let payload = stake_hash_raw_of(address, mainnet)?;
    let hrp = if mainnet { "stake" } else { "stake_test" };
    bech32::encode::<bech32::Bech32>(bech32::Hrp::parse(hrp).unwrap(), &payload).ok()
}

#[derive(serde::Serialize)]
struct AddressEvent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    address: &'a str,
    /// String-encoded (can exceed JS Number.MAX_SAFE_INTEGER).
    balance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stake_address: Option<String>,
    /// ADA Handle currently held by this address, if any (without the `$`).
    #[serde(skip_serializing_if = "Option::is_none")]
    handle: Option<String>,
    /// Distinct multi-assets currently held. Computed once via db-sync at
    /// connect; not updated live.
    assets_count: u32,
}

/// Decode a payment-address bech32 string to its raw bytes — the key used by
/// `BlockSnapshot.address_balances`. None on parse failure (e.g. byron-style).
fn address_bytes(address: &str) -> Option<Vec<u8>> {
    pallas::ledger::addresses::Address::from_bech32(address)
        .ok()
        .map(|a| a.to_vec())
}

/// Build an `Address` info event — balance from snapshot `address_balances`
/// (kept live by the sink); `assets_count` is the caller's connect-time value.
fn address_sse_event(
    address: &str,
    addr_bytes: &[u8],
    snap: Option<&BlockSnapshot>,
    mainnet: bool,
    assets_count: u32,
) -> Result<SseEvent, Infallible> {
    let balance = snap
        .and_then(|s| s.address_balances.get(addr_bytes).copied())
        .unwrap_or(0);
    let handle = snap.and_then(|s| s.handle_for(address));
    let json = serde_json::to_string(&AddressEvent {
        kind: "Address",
        address,
        balance: balance.to_string(),
        stake_address: stake_address_of(address, mainnet),
        handle,
        assets_count,
    })
    .unwrap();
    Ok(SseEvent::default().data(json))
}

/// Send a payment-address info event: balance (sum of unspent UTXOs, no rewards),
/// its stake address (for linking to the stake feed), its ADA Handle if any,
/// and its connect-time assets_count.
async fn send_address_info(
    sender: &Sender<Result<SseEvent, Infallible>>,
    chain_state: &RwLock<State>,
    address: &str,
    addr_bytes: &[u8],
    mainnet: bool,
    assets_count: u32,
) {
    let guard = chain_state.read().await;
    let _ = sender
        .send(address_sse_event(
            address,
            addr_bytes,
            guard.current(),
            mainnet,
            assets_count,
        ))
        .await;
}

/// Query string for SSE endpoints. `dpr` is the client's
/// `window.devicePixelRatio`, used to negotiate the thumbnail image size.
#[derive(serde::Deserialize)]
struct SseQuery {
    dpr: Option<f64>,
}

/// Collapse every `AssetInfo`'s precomputed token ladder down to the single
/// `tk` + `size` matching this client's negotiated rung, dropping the rest so
/// it never hits the wire. Idempotent and cheap (a slice scan, no crypto).
fn resolve_event_assets(event: &mut crate::event::Event, size: u16) {
    fn resolve(assets: &mut [AssetInfo], size: u16) {
        for a in assets {
            a.tk = a
                .tks
                .iter()
                .find(|(s, _)| *s == size)
                .map(|(_, t)| t.clone());
            a.tks = Vec::new();
            a.size = size;
        }
    }
    let txs: &mut [BlockTx] = match event {
        crate::event::Event::MempoolTx(tx) => std::slice::from_mut(tx),
        crate::event::Event::Block { txs, .. } => txs.as_mut_slice(),
        crate::event::Event::Rollback { .. }
        | crate::event::Event::MempoolPrune { .. }
        | crate::event::Event::ReplayCursor { .. } => return,
    };
    for tx in txs {
        for inp in &mut tx.inputs {
            resolve(&mut inp.assets, size);
        }
        for out in &mut tx.outputs {
            resolve(&mut out.assets, size);
        }
    }
}

fn serialize_event(
    mut event: crate::event::Event,
    size: u16,
) -> Option<Result<SseEvent, Infallible>> {
    resolve_event_assets(&mut event, size);
    serde_json::to_string(&event)
        .ok()
        .map(|json| Ok(SseEvent::default().data(json)))
}

// --- Block decoding ---

/// Decode a block CBOR into a BlockTx list and extract the minting pool info.
fn decode_block_txs(
    cbor: &[u8],
    nftcdn: &NftcdnConfig,
    state: Option<&State>,
    mainnet: bool,
    extract_delegations: bool,
) -> (Vec<BlockTx>, Option<String>, Option<String>) {
    let block = match MultiEraBlock::decode(cbor) {
        Ok(b) => b,
        Err(_) => return (vec![], None, None),
    };

    // Extract minting pool from block header
    let (block_pool_id, block_pool_ticker) = block
        .header()
        .issuer_vkey()
        .and_then(|vkey| {
            let hash = Hasher::<224>::hash(vkey);
            state?
                .current()?
                .pools
                .get(&hex::encode(hash.as_ref()))
                .cloned()
        })
        .map(|pool| (Some(pool_bech32_id(&pool.hash_raw)), pool.ticker))
        .unwrap_or((None, None));

    let txs = block
        .txs()
        .iter()
        .map(|tx| {
            let mut inputs: Vec<TxInput> = tx
                .inputs()
                .iter()
                .map(|input| TxInput {
                    tx_hash: input.hash().to_string(),
                    index: input.index() as i16,
                    address: None,
                    lovelace: 0,
                    assets: vec![],
                    handle: None,
                })
                .collect();

            let outputs = tx
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
                                    let fp = asset_fingerprint(&policy_id, asset.name());
                                    let decimals = state
                                        .and_then(|s| s.current())
                                        .and_then(|s| s.decimals.get(&fp).copied())
                                        .unwrap_or(0);
                                    let name = std::str::from_utf8(asset.name())
                                        .ok()
                                        .filter(|s| !s.is_empty())
                                        .map(String::from);
                                    let tks = nftcdn.compute_ladder(&fp, "preview");
                                    Some(AssetInfo {
                                        fingerprint: fp,
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

                    let handle = state
                        .and_then(|s| s.current())
                        .and_then(|s| s.handle_for(&address));
                    TxOutputInfo {
                        address,
                        lovelace,
                        assets,
                        handle,
                    }
                })
                .collect();

            let delegations = if extract_delegations {
                state
                    .map(|s| crate::mempool::extract_delegations(tx, s, mainnet))
                    .unwrap_or_default()
            } else {
                vec![]
            };

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

            let message = crate::pallas::extract_tx_metadata(tx);
            let catalyst = crate::pallas::extract_catalyst(tx, mainnet);
            let mut annotations = Vec::new();
            annotations.extend(crate::oracle::extract_oracle(tx));

            let votes = state
                .map(|s| crate::mempool::extract_votes(tx, s))
                .unwrap_or_default();

            BlockTx {
                hash: tx.hash().to_string(),
                fee: tx.fee().unwrap_or(0),
                size: tx.size(),
                inputs,
                outputs,
                expiry: None,
                delegations,
                votes,
                message,
                stake_change: None,
                catalyst,
                annotations,
                stake_credentials: vec![],
                withdrawals,
            }
        })
        .collect();

    (txs, block_pool_id, block_pool_ticker)
}

/// Resolve input addresses for a list of transactions via batch db-sync query.
async fn resolve_block_inputs(
    txs: &mut Vec<BlockTx>,
    chain_state: &RwLock<State>,
    nftcdn: &NftcdnConfig,
) {
    let input_keys: Vec<(Vec<u8>, i16)> = txs
        .iter()
        .flat_map(|tx| {
            tx.inputs
                .iter()
                .map(|inp| (hex::decode(&inp.tx_hash).unwrap_or_default(), inp.index))
        })
        .collect();
    if input_keys.is_empty() {
        return;
    }
    // Phase 1: brief read lock — snapshot peek for in-memory UTXOs +
    // clone the per-snapshot lookup tables + take a db handle. Anything
    // synchronous; lock released before the slow db query so other readers
    // (homepage feed, every other SSE) aren't queued behind this one.
    let (mut resolved, remaining_keys, decimals, handle_by_address, db) = {
        let guard = chain_state.read().await;
        let snap = guard.current();
        let decimals = snap.map(|s| s.decimals.clone()).unwrap_or_default();
        let handle_by_address = snap
            .map(|s| s.handle_by_address.clone())
            .unwrap_or_default();
        let db = guard.db_handle();
        let mut resolved = std::collections::HashMap::<
            (Vec<u8>, i16),
            (String, u64, Vec<(String, u64)>),
        >::with_capacity(input_keys.len());
        let mut remaining = Vec::new();
        if let Some(s) = snap {
            for (hash, index) in &input_keys {
                let key = (hash.clone(), *index);
                if let Some(utxo) = s.utxos.get(&key) {
                    let addr = pallas::ledger::addresses::Address::from_bytes(&utxo.address)
                        .ok()
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    let lovelace: u64 = utxo
                        .lovelaces
                        .try_into()
                        .expect("lovelace value must fit u64");
                    resolved.insert(key, (addr, lovelace, utxo.assets.clone()));
                } else {
                    remaining.push(key);
                }
            }
        } else {
            remaining = input_keys.clone();
        }
        (resolved, remaining, decimals, handle_by_address, db)
    };

    // Phase 2: db query for cache misses, with NO lock held.
    let mut to_cache = Vec::new();
    if !remaining_keys.is_empty() {
        if let Some(db) = db {
            if let Ok(db_result) = db.resolve_utxos_batch(&remaining_keys).await {
                for (key, (addr, lovelace, assets, unspent)) in db_result {
                    if unspent {
                        let address_bytes = pallas::ledger::addresses::Address::from_bech32(&addr)
                            .ok()
                            .map(|a| a.to_vec())
                            .unwrap_or_default();
                        to_cache.push((
                            key.clone(),
                            TxOutput {
                                lovelaces: rust_decimal::Decimal::from(lovelace),
                                address: address_bytes,
                                assets: assets.clone(),
                            },
                        ));
                    }
                    resolved.insert(key, (addr, lovelace, assets));
                }
            }
        }
    }

    // Phase 3: brief write lock to insert into the snapshot's utxo cache.
    if !to_cache.is_empty() {
        let mut guard = chain_state.write().await;
        if let Some(snap) = guard.current_mut() {
            for (key, utxo) in to_cache {
                snap.utxos.insert(key, utxo);
            }
        }
    }

    for tx in txs {
        for inp in &mut tx.inputs {
            let key = (hex::decode(&inp.tx_hash).unwrap_or_default(), inp.index);
            if let Some((addr, lovelace, raw_assets)) = resolved.get(&key) {
                inp.address = Some(addr.clone());
                inp.lovelace = *lovelace;
                inp.handle = handle_by_address
                    .get(addr)
                    .and_then(|hs| hs.iter().min_by_key(|h| h.len()).cloned());
                inp.assets = raw_assets
                    .iter()
                    .map(|(fp, raw)| {
                        let dec = decimals.get(fp).copied().unwrap_or(0);
                        let tks = nftcdn.compute_ladder(fp, "preview");
                        AssetInfo {
                            fingerprint: fp.clone(),
                            name: None,
                            quantity: format_quantity(*raw, dec),
                            tks,
                            tk: None,
                            size: 0,
                        }
                    })
                    .collect();
            }
        }
    }
}

// --- Replay: send historical events through mpsc channel ---

/// A block to replay: pool's own block (all txs) or stake-change block (filtered).
struct ReplayBlock {
    slot: u64,
    hash: String,
    number: u64,
    /// Block's epoch — only used by the stake/address backward stake walk
    /// (`SubjectReplay`); 0 for pool/DRep replay blocks, which don't walk.
    epoch: u64,
    pool_id: Option<String>,
    pool_ticker: Option<String>,
    /// If true, filter txs to only those involving pool delegators.
    filter_by_delegators: bool,
}

/// Backward stake/delegation reconstruction for a single stake credential's feed.
/// Walks the replayed blocks newest→oldest, undoing each block's net stake change
/// (plus epoch-boundary reward accruals and off-window withdrawals that happen
/// between the address's blocks) from the current snapshot stake to recover the
/// exact pre-block `live_stake` at every displayed block. Delegation `from`/`to`
/// come from the full db history (`deleg_by_tx`), so both are correct at any age.
struct SubjectReplay {
    /// Live stake walking backward; starts at the current snapshot value.
    running: i64,
    /// The feed subject's reward (stake1…) address — to attach the pre-block stake to
    /// a Catalyst registration of this same credential.
    subject_stake_address: String,
    /// tx_hash → delegation (from/to resolved); `live_stake` filled in during the walk.
    deleg_by_tx: HashMap<String, DelegationInfo>,
    /// Reward additions per epoch (`spendable_epoch`, delta), sorted by epoch desc.
    reward_deltas: Vec<(u64, i64)>,
    reward_cursor: usize,
    /// Off-window reward withdrawals (slot, amount), sorted by slot desc.
    withdrawals: Vec<(u64, i64)>,
    wd_cursor: usize,
    /// Slot/epoch of the last block walked — the pagination cursor anchor (with
    /// `running`). Tracks the last *walked* block, not the last *sent*, so an
    /// empty/failed boundary block can't corrupt the next page's anchor.
    last_slot: u64,
    last_epoch: u64,
}

impl SubjectReplay {
    /// Walk one block backward and return the exact pre-block `live_stake`. Undoes,
    /// in order: epoch reward accruals applied after this block (`spendable_epoch >
    /// block_epoch`), off-window withdrawals after it (`slot > block_slot`), then the
    /// block's own net stake change (`block_delta` = Σ of all its txs' stake_change).
    /// Must be called newest→oldest; the cursors advance monotonically.
    fn pre_block_stake(&mut self, block_epoch: u64, block_slot: u64, block_delta: i64) -> i64 {
        while self.reward_cursor < self.reward_deltas.len()
            && self.reward_deltas[self.reward_cursor].0 > block_epoch
        {
            self.running -= self.reward_deltas[self.reward_cursor].1;
            self.reward_cursor += 1;
        }
        while self.wd_cursor < self.withdrawals.len()
            && self.withdrawals[self.wd_cursor].0 > block_slot
        {
            self.running += self.withdrawals[self.wd_cursor].1;
            self.wd_cursor += 1;
        }
        self.running -= block_delta;
        self.last_slot = block_slot;
        self.last_epoch = block_epoch;
        self.running
    }

    /// Pagination cursor after the walk: `(oldest walked slot, its epoch, pre-block
    /// stake)`. The next page continues the walk from this stake/epoch below this slot.
    fn cursor(&self) -> (u64, u64, i64) {
        (self.last_slot, self.last_epoch, self.running)
    }
}

/// Fetch replay blocks via N2N and send as SSE events. Newest-first order.
/// Shared SSE transport + config for replay sends, reused across the per-feed
/// branches in `filtered_events`. Built once; the per-call inputs (blocks,
/// delegators, filter, deleg_info, threshold) stay separate arguments.
struct ReplaySse<'a> {
    sender: &'a Sender<Result<SseEvent, Infallible>>,
    nftcdn: &'a NftcdnConfig,
    genesis: &'a GenesisConfig,
    chain_state: &'a RwLock<State>,
    n2n_addr: SocketAddr,
    magic: u64,
    mainnet: bool,
    size: u16,
}

/// Build per-tx delegation info for a single stake credential from the feed
/// index, keyed by tx hash and ready to inject into replayed blocks on
/// stake/address feeds. The pool and DRep delegation events of a tx are merged
/// into one `DelegationInfo` (a tx may change both at once). `from`/`to` labels
/// are resolved against the current snapshot. Returns empty if the credential
/// has no delegation events in the (5-day) feed-index window.
fn build_stake_deleg_info(
    feed_index: &crate::state::FeedIndex,
    cred: &[u8],
    mainnet: bool,
    snap: Option<&BlockSnapshot>,
) -> HashMap<String, Vec<DelegationInfo>> {
    let (pool_entries, drep_entries) = feed_index.delegation_entries_by_cred(cred);
    if pool_entries.is_empty() && drep_entries.is_empty() {
        return HashMap::new();
    }

    let resolve_pool = |hash: &[u8]| -> (String, Option<String>) {
        let ticker = snap
            .and_then(|s| s.pools.get(&hex::encode(hash)))
            .and_then(|p| p.ticker.clone());
        (pool_bech32_id(hash), ticker)
    };
    let resolve_drep = |bytes: &[u8]| -> (String, Option<String>) {
        let name = match bytes.first() {
            Some(0x02) => Some("Always Abstain".to_string()),
            Some(0x03) => Some("Always No Confidence".to_string()),
            _ => snap
                .and_then(|s| s.dreps.get(bytes))
                .and_then(|d| d.given_name.clone()),
        };
        (drep_bech32_id(bytes), name)
    };

    let stake_address = crate::pallas::stake_address_from_cred_bytes(cred, mainnet);
    let blank = |live_stake: i64| DelegationInfo {
        stake_address: stake_address.clone(),
        from_pool_id: None,
        from_ticker: None,
        to_pool_id: None,
        to_ticker: None,
        from_drep_id: None,
        from_drep_name: None,
        to_drep_id: None,
        to_drep_name: None,
        live_stake,
    };

    let mut merged: HashMap<String, DelegationInfo> = HashMap::new();
    for e in pool_entries {
        let info = merged
            .entry(e.tx_hash.clone())
            .or_insert_with(|| blank(e.live_stake));
        info.live_stake = e.live_stake;
        if let Some(h) = &e.from {
            let (id, t) = resolve_pool(h);
            info.from_pool_id = Some(id);
            info.from_ticker = t;
        }
        if let Some(h) = &e.to {
            let (id, t) = resolve_pool(h);
            info.to_pool_id = Some(id);
            info.to_ticker = t;
        }
    }
    for e in drep_entries {
        let info = merged
            .entry(e.tx_hash.clone())
            .or_insert_with(|| blank(e.live_stake));
        if let Some(b) = &e.from {
            let (id, n) = resolve_drep(b);
            info.from_drep_id = Some(id);
            info.from_drep_name = n;
        }
        if let Some(b) = &e.to {
            let (id, n) = resolve_drep(b);
            info.to_drep_id = Some(id);
            info.to_drep_name = n;
        }
    }

    merged.into_iter().map(|(k, v)| (k, vec![v])).collect()
}

/// Build the backward stake/delegation reconstruction for a stake credential's
/// feed (29-byte `hash_raw`, 28-byte `cred`). Reads the anchor stake from the
/// snapshot, then runs the delegation-history / reward-delta / withdrawal queries
/// **off the lock**, and resolves delegation targets under a second short lock.
/// `from`/`to` are correct at any age (full db history); `live_stake` is filled in
/// per block during the walk in `send_replay_blocks`.
/// `anchor`: `None` for the first page — read the current snapshot live stake +
/// epoch; `Some((stake, epoch))` for an older page — continue the walk from the
/// previous page's cursor (no snapshot read; reward deltas are capped at `epoch`).
#[allow(clippy::too_many_arguments)]
async fn build_subject_replay(
    chain_state: &RwLock<State>,
    db: &crate::state::DbSync,
    hash_raw: &[u8],
    cred: &[u8],
    blocks: &[(u64, String, u64, u64)],
    exclude_slots: &HashSet<u64>,
    mainnet: bool,
    anchor: Option<(i64, u64)>,
) -> SubjectReplay {
    // Anchor: cursor (older page) or the current live stake + epoch (first page).
    let (anchor, current_epoch) = match anchor {
        Some(ac) => ac,
        None => {
            let guard = chain_state.read().await;
            let snap = guard.current();
            let stake = snap.map_or(0, |s| {
                s.stakes.get(cred).copied().unwrap_or(0) + s.rewards.get(cred).copied().unwrap_or(0)
            });
            let epoch = snap.and_then(|s| s.last_epoch).unwrap_or(0);
            (stake, epoch)
        }
    };

    // Window bounds from the oldest replayed block (blocks aren't yet sorted).
    let min_slot = blocks.iter().map(|b| b.0).min().unwrap_or(0);
    let min_epoch = blocks.iter().map(|b| b.3).min().unwrap_or(0);

    // Off-lock db queries (all addr_id-indexed).
    let pool_hist = db
        .pool_delegation_history(hash_raw)
        .await
        .unwrap_or_default();
    let drep_hist = db
        .drep_delegation_history(hash_raw)
        .await
        .unwrap_or_default();
    let reward_rows = db
        .stake_reward_deltas(hash_raw, min_epoch as i64, current_epoch as i64)
        .await
        .unwrap_or_default();
    let wd_rows = db
        .stake_withdrawals_since(hash_raw, min_slot as i64)
        .await
        .unwrap_or_default();

    // Resolve delegation target identities under a second short lock (no await).
    let deleg_by_tx = {
        let guard = chain_state.read().await;
        let snap = guard.current();
        let resolve_pool = |hash: &[u8]| -> (String, Option<String>) {
            let ticker = snap
                .and_then(|s| s.pools.get(&hex::encode(hash)))
                .and_then(|p| p.ticker.clone());
            (pool_bech32_id(hash), ticker)
        };
        let resolve_drep = |bytes: &[u8]| -> (String, Option<String>) {
            let name = match bytes.first() {
                Some(0x02) => Some("Always Abstain".to_string()),
                Some(0x03) => Some("Always No Confidence".to_string()),
                _ => snap
                    .and_then(|s| s.dreps.get(bytes))
                    .and_then(|d| d.given_name.clone()),
            };
            (drep_bech32_id(bytes), name)
        };
        let stake_address = crate::pallas::stake_address_from_cred_bytes(cred, mainnet);
        let blank = || DelegationInfo {
            stake_address: stake_address.clone(),
            from_pool_id: None,
            from_ticker: None,
            to_pool_id: None,
            to_ticker: None,
            from_drep_id: None,
            from_drep_name: None,
            to_drep_id: None,
            to_drep_name: None,
            live_stake: 0, // filled in during the backward walk
        };

        let mut merged: HashMap<String, DelegationInfo> = HashMap::new();
        for e in &pool_hist {
            if e.to == e.from {
                continue; // same-pool re-delegation: no change to show
            }
            let info = merged.entry(e.tx_hash.clone()).or_insert_with(blank);
            if let Some(h) = &e.from {
                let (id, t) = resolve_pool(h);
                info.from_pool_id = Some(id);
                info.from_ticker = t;
            }
            if let Some(h) = &e.to {
                let (id, t) = resolve_pool(h);
                info.to_pool_id = Some(id);
                info.to_ticker = t;
            }
        }
        for e in &drep_hist {
            if e.to == e.from {
                continue;
            }
            let info = merged.entry(e.tx_hash.clone()).or_insert_with(blank);
            if let Some(b) = &e.from {
                let (id, n) = resolve_drep(b);
                info.from_drep_id = Some(id);
                info.from_drep_name = n;
            }
            if let Some(b) = &e.to {
                let (id, n) = resolve_drep(b);
                info.to_drep_id = Some(id);
                info.to_drep_name = n;
            }
        }
        merged
    };

    // Reward deltas newest-epoch first; off-window withdrawals newest-slot first
    // (those in the replayed set are accounted for via each block's net stake change).
    let mut reward_deltas: Vec<(u64, i64)> = reward_rows
        .into_iter()
        .map(|(e, d)| (e as u64, d))
        .collect();
    reward_deltas.sort_by(|a, b| b.0.cmp(&a.0));
    let mut withdrawals: Vec<(u64, i64)> = wd_rows
        .into_iter()
        .map(|(s, a)| (s as u64, a))
        .filter(|(slot, _)| !exclude_slots.contains(slot))
        .collect();
    withdrawals.sort_by(|a, b| b.0.cmp(&a.0));

    SubjectReplay {
        running: anchor,
        subject_stake_address: crate::pallas::stake_address_from_cred_bytes(cred, mainnet),
        deleg_by_tx,
        reward_deltas,
        reward_cursor: 0,
        withdrawals,
        wd_cursor: 0,
        last_slot: 0,
        last_epoch: 0,
    }
}

/// Transport-less inputs shared by SSE replay and the `/older` HTTP handler when
/// turning one fetched block into an `Event::Block`.
struct ReplayCtx<'a> {
    nftcdn: &'a NftcdnConfig,
    genesis: &'a GenesisConfig,
    chain_state: &'a RwLock<State>,
    mainnet: bool,
}

/// Fetch one block via N2N, decode + resolve + inject delegations + (for
/// stake/address feeds) walk the stake backward, and build the `Event::Block` —
/// or `None` on fetch/decode failure or when it filters to no txs. `deleg_info`
/// maps tx_hash -> delegations to inject. Shared by `send_replay_blocks` and the
/// `/older` endpoint; the caller owns the (single-flight) N2N client.
#[allow(clippy::too_many_arguments)]
async fn process_replay_block(
    client: &mut PeerClient,
    ctx: &ReplayCtx<'_>,
    block: &ReplayBlock,
    delegators: &imbl::hashset::HashSet<Vec<u8>>,
    feed_filter: &filter::FeedFilter,
    deleg_info: &HashMap<String, Vec<DelegationInfo>>,
    stake_threshold: u64,
    subject: Option<&mut SubjectReplay>,
) -> Option<crate::event::Event> {
    let hash_bytes = hex::decode(&block.hash).ok()?;
    let point = Point::Specific(block.slot, hash_bytes);
    let cbor = match client.blockfetch().fetch_single(point).await {
        Ok(cbor) => cbor,
        Err(e) => {
            warn!(block.slot, "block-fetch failed: {}", e);
            return None;
        }
    };

    let state_guard = ctx.chain_state.read().await;
    let (mut txs, cbor_pool_id, cbor_pool_ticker) = decode_block_txs(
        &cbor,
        ctx.nftcdn,
        Some(&state_guard),
        ctx.mainnet,
        !block.filter_by_delegators,
    );
    drop(state_guard);
    resolve_block_inputs(&mut txs, ctx.chain_state, ctx.nftcdn).await;
    for tx in &mut txs {
        tx.stake_credentials = filter::extract_stake_credentials(tx);
    }

    if block.filter_by_delegators {
        // Computes UTXO changes + delegation impact in one pass. For pool/DRep feeds
        // this uses tx.delegations, so the feed-index injection must precede it;
        // stake/address feeds ignore delegations here (display-only) and inject after.
        if subject.is_none() {
            for tx in &mut txs {
                if let Some(delegations) = deleg_info.get(&tx.hash) {
                    tx.delegations = delegations.clone();
                }
            }
        }
        filter::apply_stake_changes(&mut txs, delegators, feed_filter);

        // Stake/address feeds: walk the stake backward to the exact pre-block value
        // and attach delegations from the full db history (correct from/to at any
        // age). Undo, newest→oldest: epoch reward accruals (epoch > this block's),
        // then off-window withdrawals (slot > this block's), then this block's own
        // net stake change (sum over all decoded txs, before the retain).
        if let Some(sr) = subject {
            let block_delta: i64 = txs.iter().filter_map(|t| t.stake_change).sum();
            let pre = sr.pre_block_stake(block.epoch, block.slot, block_delta);
            for tx in &mut txs {
                // Feed index wins (authoritative near the tip where db-sync may lag);
                // fall back to db history otherwise.
                if let Some(delegations) = deleg_info.get(&tx.hash) {
                    tx.delegations = delegations.clone();
                } else if let Some(info) = sr.deleg_by_tx.get(&tx.hash) {
                    let mut info = info.clone();
                    info.live_stake = pre;
                    tx.delegations = vec![info];
                }
                // A Catalyst registration of this same credential gets the same stake.
                if let Some(cat) = &mut tx.catalyst {
                    if cat.stake_address == sr.subject_stake_address {
                        cat.live_stake = Some(pre);
                    }
                }
            }
        }

        let single_subject = matches!(
            feed_filter,
            filter::FeedFilter::Stake(_) | filter::FeedFilter::Address(_)
        );
        txs.retain(|tx| {
            if single_subject {
                // Single stake/payment address: show every tx that touches it, like
                // the live path — not the pool/drep threshold.
                feed_filter.matches_tx(tx, delegators)
            } else {
                !tx.delegations.is_empty()
                    || tx
                        .stake_change
                        .is_some_and(|sc| sc.unsigned_abs() > stake_threshold)
            }
        });
        if txs.is_empty() {
            return None;
        }
    }

    let pool_id = block.pool_id.clone().or(cbor_pool_id);
    let pool_ticker = block.pool_ticker.clone().or(cbor_pool_ticker);
    Some(crate::event::Event::Block {
        slot: block.slot,
        hash: block.hash.clone(),
        number: block.number,
        timestamp: slot_to_timestamp(block.slot, ctx.genesis),
        pool_id,
        pool_ticker,
        txs,
    })
}

/// `deleg_info` maps tx_hash -> Vec<DelegationInfo> for injecting correct delegation data.
async fn send_replay_blocks(
    sse: &ReplaySse<'_>,
    blocks: &mut [ReplayBlock],
    delegators: &imbl::hashset::HashSet<Vec<u8>>,
    feed_filter: &filter::FeedFilter,
    deleg_info: &HashMap<String, Vec<DelegationInfo>>,
    stake_threshold: u64,
    mut subject: Option<&mut SubjectReplay>,
) {
    let &ReplaySse {
        sender,
        nftcdn,
        genesis,
        chain_state,
        n2n_addr,
        magic,
        mainnet,
        size,
    } = sse;
    if blocks.is_empty() {
        return;
    }
    // Sort newest-first so the feed builds immediately with recent activity
    blocks.sort_by(|a, b| b.slot.cmp(&a.slot));

    let mut client = match PeerClient::connect(n2n_addr, magic).await {
        Ok(c) => c,
        Err(_) => {
            warn!("N2N connect to {} failed", n2n_addr);
            return;
        }
    };
    let ctx = ReplayCtx {
        nftcdn,
        genesis,
        chain_state,
        mainnet,
    };
    let mut sent = 0usize;
    for block in blocks.iter() {
        let event = process_replay_block(
            &mut client,
            &ctx,
            block,
            delegators,
            feed_filter,
            deleg_info,
            stake_threshold,
            subject.as_deref_mut(),
        )
        .await;
        if let Some(event) = event {
            if let Some(sse) = serialize_event(event, size) {
                let _ = sender.send(sse).await;
                sent += 1;
                if sent >= MAX_REPLAY_BLOCKS {
                    break;
                }
            }
        }
    }
    let _ = client.abort().await;
}

/// Send filtered snapshot events, optionally deduplicating against known block slots.
async fn send_filtered_snapshot(
    sender: &Sender<Result<SseEvent, Infallible>>,
    snapshot: Vec<crate::event::Event>,
    filter: &filter::FeedFilter,
    delegators: &imbl::hashset::HashSet<Vec<u8>>,
    exclude_slots: &HashSet<u64>,
    size: u16,
) {
    for event in snapshot {
        if let Some(filtered) = filter.filter_event(&event, delegators) {
            if let crate::event::Event::Block { slot, .. } = &filtered {
                if exclude_slots.contains(slot) {
                    continue;
                }
            }
            if let Some(sse) = serialize_event(filtered, size) {
                let _ = sender.send(sse).await;
            }
        }
    }
}

// --- Pool metadata helpers ---

/// Extract pool id, ticker, and hash from current state. Fast (in-memory).
fn extract_pool_meta(
    snap: Option<&BlockSnapshot>,
    filter: &filter::FeedFilter,
) -> (Option<String>, Option<String>, Option<Vec<u8>>) {
    if let filter::FeedFilter::Pool(ref hash) = filter {
        let (pool_id, pool_ticker) = snap
            .and_then(|s| s.pools.get(&hex::encode(hash)))
            .map(|p| (Some(pool_bech32_id(&p.hash_raw)), p.ticker.clone()))
            .unwrap_or((None, None));
        (pool_id, pool_ticker, Some(hash.clone()))
    } else {
        (None, None, None)
    }
}

/// Send current pool info as an SSE event.
async fn send_pool_info(
    sender: &Sender<Result<SseEvent, Infallible>>,
    chain_state: &RwLock<State>,
    pool_hash: &[u8],
) {
    let guard = chain_state.read().await;
    if let Some(snap) = guard.current() {
        if let Some(pool) = snap.pools.get(&hex::encode(pool_hash)) {
            let _ = sender.send(pool_sse_event(pool, Some(snap))).await;
        }
    }
}

/// Send current DRep info as an SSE event.
async fn send_drep_info(
    sender: &Sender<Result<SseEvent, Infallible>>,
    chain_state: &RwLock<State>,
    drep_bytes: &[u8],
) {
    let guard = chain_state.read().await;
    let snap = guard.current();
    let _ = sender.send(drep_sse_event(drep_bytes, snap)).await;
}

/// Categories for feed index replay actions, by priority.
enum SlotAction {
    PoolMinted(BlockRef),
    StakeChange(BlockRef),
}

/// Build the live stream that detects pool parameter/stake changes and filters events.
/// Per-feed mutable state the live stream compares against between blocks to
/// decide whether to re-emit the info header. Different filter kinds use
/// different subsets: Pool tracks `(pool, balance)`; DRep tracks `balance`;
/// Stake / Address track `(balance, assets_count)`.
#[derive(Default, Clone)]
struct LiveState {
    pool: Option<Pool>,
    balance: Option<i64>,
    assets_count: Option<u32>,
}

fn build_live_stream(
    rx: tokio::sync::broadcast::Receiver<crate::event::Event>,
    filter: filter::FeedFilter,
    chain_state: Arc<RwLock<State>>,
    initial: LiveState,
    size: u16,
    mainnet: bool,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    futures::stream::unfold(
        (
            BroadcastStream::new(rx),
            filter,
            chain_state,
            initial,
            std::collections::VecDeque::<Result<SseEvent, Infallible>>::new(),
            size,
            mainnet,
        ),
        |(mut rx, filter, chain_state, mut live, mut buf, size, mainnet)| async move {
            loop {
                if let Some(sse) = buf.pop_front() {
                    return Some((sse, (rx, filter, chain_state, live, buf, size, mainnet)));
                }

                let event = rx.next().await?.ok()?;

                if matches!(
                    event,
                    crate::event::Event::Block { .. } | crate::event::Event::Rollback { .. }
                ) {
                    match &filter {
                        filter::FeedFilter::Pool(ref hash) => {
                            let (current_pool, current_balance, pool_event) = {
                                let guard = chain_state.read().await;
                                let snap = guard.current();
                                let pool =
                                    snap.and_then(|s| s.pools.get(&hex::encode(hash)).cloned());
                                let live_stake = snap.and_then(|s| State::pool_live_stake(s, hash));
                                let event = pool.as_ref().map(|p| pool_sse_event(p, snap));
                                (pool, live_stake, event)
                            };
                            if current_pool != live.pool || current_balance != live.balance {
                                if let Some(event) = pool_event {
                                    buf.push_back(event);
                                }
                                live.pool = current_pool;
                                live.balance = current_balance;
                            }
                        }
                        filter::FeedFilter::DRep(ref bytes) => {
                            let current_balance = {
                                let guard = chain_state.read().await;
                                guard
                                    .current()
                                    .and_then(|s| State::drep_live_stake(s, bytes))
                            };
                            if current_balance != live.balance {
                                let guard = chain_state.read().await;
                                buf.push_back(drep_sse_event(bytes, guard.current()));
                                live.balance = current_balance;
                            }
                        }
                        filter::FeedFilter::Stake(ref payload) => {
                            let cred = &payload[1..];
                            let current_balance = {
                                let guard = chain_state.read().await;
                                guard.current().and_then(|s| s.stakes.get(cred).copied())
                            };
                            if current_balance != live.balance {
                                let stake_address = filter.feed_id();
                                let guard = chain_state.read().await;
                                // assets_count is connect-only; pass the cached value.
                                buf.push_back(stake_sse_event(
                                    &stake_address,
                                    cred,
                                    guard.current(),
                                    live.assets_count.unwrap_or(0),
                                ));
                                live.balance = current_balance;
                            }
                        }
                        filter::FeedFilter::Address(ref addr) => {
                            if let Some(addr_b) = address_bytes(addr) {
                                let current_balance = {
                                    let guard = chain_state.read().await;
                                    guard
                                        .current()
                                        .and_then(|s| s.address_balances.get(&addr_b).copied())
                                };
                                if current_balance != live.balance {
                                    let guard = chain_state.read().await;
                                    buf.push_back(address_sse_event(
                                        addr,
                                        &addr_b,
                                        guard.current(),
                                        mainnet,
                                        live.assets_count.unwrap_or(0),
                                    ));
                                    live.balance = current_balance;
                                }
                            }
                        }
                    }
                }

                let delegators = {
                    let guard = chain_state.read().await;
                    guard
                        .current()
                        .map(|snap| filter.current_delegators(snap))
                        .unwrap_or_default()
                };
                if let Some(sse) = filter
                    .filter_event(&event, &delegators)
                    .and_then(|e| serialize_event(e, size))
                {
                    return Some((sse, (rx, filter, chain_state, live, buf, size, mainnet)));
                }
            }
        },
    )
}

// --- SSE endpoints ---

async fn events(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(query): axum::extract::Query<SseQuery>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let size = rung_for_dpr(query.dpr.unwrap_or(1.0));
    let (mut snapshot, rx) = state.bus.subscribe().await;

    let config = Some(config_event(
        state.nftcdn.subdomain,
        &state.genesis,
        state.magic,
    ));

    let init = if snapshot.is_empty() {
        None
    } else {
        for event in &mut snapshot {
            resolve_event_assets(event, size);
        }
        serde_json::to_string(&snapshot)
            .ok()
            .map(|json| Ok(SseEvent::default().data(json)))
    };
    let replay = futures::stream::iter(config.into_iter().chain(init));
    let live = BroadcastStream::new(rx)
        .filter_map(move |result| result.ok().and_then(|e| serialize_event(e, size)));
    let stream = replay.chain(live);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn filtered_events(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(feed_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<SseQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, StatusCode> {
    info!("/events/{feed_id}");
    let size = rung_for_dpr(query.dpr.unwrap_or(1.0));
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;
    let (snapshot, rx) = state.bus.subscribe().await;

    let (delegators, pool_id, pool_ticker, pool_hash) = {
        let guard = state.chain_state.read().await;
        let delegators = guard
            .current()
            .map(|snap| filter.current_delegators(snap))
            .unwrap_or_default();
        let (pool_id, pool_ticker, pool_hash) = extract_pool_meta(guard.current(), &filter);
        (delegators, pool_id, pool_ticker, pool_hash)
    };

    // Spawn replay task: config → pool info → feed index replay → snapshot
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(32);
    let replay_filter = filter.clone();
    let replay_delegators = delegators.clone();
    let replay_state = state.clone();

    tokio::spawn(async move {
        let _ = sender
            .send(config_event(
                replay_state.nftcdn.subdomain,
                &replay_state.genesis,
                replay_state.magic,
            ))
            .await;

        // Shared transport/config for every send_replay_blocks call below.
        let sse = ReplaySse {
            sender: &sender,
            nftcdn: &replay_state.nftcdn,
            genesis: &replay_state.genesis,
            chain_state: &replay_state.chain_state,
            n2n_addr: replay_state.n2n_addr,
            magic: replay_state.magic,
            mainnet: replay_state.mainnet,
            size,
        };

        let exclude_slots = if let Some(ref ph) = pool_hash {
            send_pool_info(&sender, &replay_state.chain_state, ph).await;

            // Read feed index data, pool live stake, and resolve delegation labels
            let (minted, stake_changes, deleg_info, deleg_slots, stake_threshold) = {
                let guard = replay_state.chain_state.read().await;
                let snap = guard.current();
                let live_stake = snap
                    .and_then(|s| State::pool_live_stake(s, ph))
                    .unwrap_or(0);
                let threshold = (live_stake as u64) / STAKE_CHANGE_DIVISOR;

                let resolve_pool = |hash: &[u8]| -> (String, Option<String>) {
                    let ticker = snap
                        .and_then(|s| s.pools.get(&hex::encode(hash)))
                        .and_then(|p| p.ticker.clone());
                    (pool_bech32_id(hash), ticker)
                };

                let delegations = guard.feed_index.pool_delegation_entries(ph);
                let mut deleg_info: HashMap<String, Vec<DelegationInfo>> = HashMap::new();
                for entry in &delegations {
                    let (from_pool_id, from_ticker) = entry
                        .from
                        .as_ref()
                        .map(|h| resolve_pool(h))
                        .map(|(id, t)| (Some(id), t))
                        .unwrap_or((None, None));
                    let (to_pool_id, to_ticker) = entry
                        .to
                        .as_ref()
                        .map(|h| resolve_pool(h))
                        .map(|(id, t)| (Some(id), t))
                        .unwrap_or((None, None));
                    let info = DelegationInfo {
                        stake_address: crate::pallas::stake_address_from_cred_bytes(
                            &entry.cred,
                            replay_state.mainnet,
                        ),
                        from_pool_id,
                        from_ticker,
                        to_pool_id,
                        to_ticker,
                        from_drep_id: None,
                        from_drep_name: None,
                        to_drep_id: None,
                        to_drep_name: None,
                        live_stake: entry.live_stake,
                    };
                    deleg_info
                        .entry(entry.tx_hash.clone())
                        .or_default()
                        .push(info);
                }

                let deleg_slots: Vec<BlockRef> = delegations
                    .iter()
                    .map(|e| BlockRef {
                        slot: e.slot,
                        hash: e.block_hash.clone(),
                        number: e.block_no,
                    })
                    .collect();

                (
                    guard.feed_index.pool_minted_blocks(ph).to_vec(),
                    guard.feed_index.pool_stake_change_blocks(ph).to_vec(),
                    deleg_info,
                    deleg_slots,
                    threshold,
                )
            };

            // Build block actions: PoolMinted > StakeChange priority per slot
            let mut slot_map: HashMap<u64, SlotAction> = HashMap::new();

            for r in &minted {
                slot_map.insert(r.slot, SlotAction::PoolMinted(r.clone()));
            }
            for r in &stake_changes {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::StakeChange(r.clone()));
            }
            for r in &deleg_slots {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::StakeChange(r.clone()));
            }

            let exclude_slots: HashSet<u64> = slot_map.keys().copied().collect();

            // Sort block actions newest-first
            let mut replay_blocks: Vec<ReplayBlock> = Vec::new();
            let mut actions: Vec<(u64, SlotAction)> = slot_map.into_iter().collect();
            actions.sort_by(|a, b| b.0.cmp(&a.0));

            for (_, action) in actions {
                match action {
                    SlotAction::PoolMinted(r) => replay_blocks.push(ReplayBlock {
                        slot: r.slot,
                        hash: r.hash,
                        number: r.number,
                        epoch: 0,
                        pool_id: pool_id.clone(),
                        pool_ticker: pool_ticker.clone(),
                        filter_by_delegators: false,
                    }),
                    SlotAction::StakeChange(r) => replay_blocks.push(ReplayBlock {
                        slot: r.slot,
                        hash: r.hash,
                        number: r.number,
                        epoch: 0,
                        pool_id: None,
                        pool_ticker: None,
                        filter_by_delegators: true,
                    }),
                }
            }

            // Send blocks via N2N with delegation info injected
            send_replay_blocks(
                &sse,
                &mut replay_blocks,
                &replay_delegators,
                &replay_filter,
                &deleg_info,
                stake_threshold,
                None,
            )
            .await;

            exclude_slots
        } else if let Some(ref db) = replay_filter.drep_bytes().cloned() {
            send_drep_info(&sender, &replay_state.chain_state, db).await;

            // Read DRep feed index data and resolve delegation labels
            let (stake_changes, deleg_info, deleg_slots, stake_threshold) = {
                let guard = replay_state.chain_state.read().await;
                let snap = guard.current();
                let live_stake = snap
                    .and_then(|s| State::drep_live_stake(s, db))
                    .unwrap_or(0);
                let threshold = (live_stake as u64) / STAKE_CHANGE_DIVISOR;

                let resolve_drep = |bytes: &[u8]| -> (String, Option<String>) {
                    let name = match bytes.first() {
                        Some(0x02) => Some("Always Abstain".to_string()),
                        Some(0x03) => Some("Always No Confidence".to_string()),
                        _ => snap
                            .and_then(|s| s.dreps.get(bytes))
                            .and_then(|d| d.given_name.clone()),
                    };
                    (drep_bech32_id(bytes), name)
                };

                let delegations = guard.feed_index.drep_delegation_entries(db);
                let mut deleg_info: HashMap<String, Vec<DelegationInfo>> = HashMap::new();
                for entry in &delegations {
                    let (from_drep_id, from_drep_name) = entry
                        .from
                        .as_ref()
                        .map(|h| resolve_drep(h))
                        .map(|(id, n)| (Some(id), n))
                        .unwrap_or((None, None));
                    let (to_drep_id, to_drep_name) = entry
                        .to
                        .as_ref()
                        .map(|h| resolve_drep(h))
                        .map(|(id, n)| (Some(id), n))
                        .unwrap_or((None, None));
                    let info = DelegationInfo {
                        stake_address: crate::pallas::stake_address_from_cred_bytes(
                            &entry.cred,
                            replay_state.mainnet,
                        ),
                        from_pool_id: None,
                        from_ticker: None,
                        to_pool_id: None,
                        to_ticker: None,
                        from_drep_id,
                        from_drep_name,
                        to_drep_id,
                        to_drep_name,
                        live_stake: entry.live_stake,
                    };
                    deleg_info
                        .entry(entry.tx_hash.clone())
                        .or_default()
                        .push(info);
                }

                let deleg_slots: Vec<BlockRef> = delegations
                    .iter()
                    .map(|e| BlockRef {
                        slot: e.slot,
                        hash: e.block_hash.clone(),
                        number: e.block_no,
                    })
                    .collect();

                (
                    guard.feed_index.drep_stake_change_blocks(db).to_vec(),
                    deleg_info,
                    deleg_slots,
                    threshold,
                )
            };

            // Build block actions: only StakeChange (no minting for DReps)
            let mut slot_map: HashMap<u64, SlotAction> = HashMap::new();
            for r in &stake_changes {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::StakeChange(r.clone()));
            }
            for r in &deleg_slots {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::StakeChange(r.clone()));
            }

            let exclude_slots: HashSet<u64> = slot_map.keys().copied().collect();

            let mut replay_blocks: Vec<ReplayBlock> = Vec::new();
            let mut actions: Vec<(u64, SlotAction)> = slot_map.into_iter().collect();
            actions.sort_by(|a, b| b.0.cmp(&a.0));

            for (_, action) in actions {
                if let SlotAction::StakeChange(r) = action {
                    replay_blocks.push(ReplayBlock {
                        slot: r.slot,
                        hash: r.hash,
                        number: r.number,
                        epoch: 0,
                        pool_id: None,
                        pool_ticker: None,
                        filter_by_delegators: true,
                    });
                }
            }

            send_replay_blocks(
                &sse,
                &mut replay_blocks,
                &replay_delegators,
                &replay_filter,
                &deleg_info,
                stake_threshold,
                None,
            )
            .await;

            exclude_slots
        } else if let Some(payload) = replay_filter.stake_payload().cloned() {
            let cred = payload[1..].to_vec();
            let stake_address = replay_filter.feed_id();
            // One short read lock: grab the db handle and build delegation info for
            // this credential from the feed index (no await held — see db_handle).
            let (db, deleg_info) = {
                let guard = replay_state.chain_state.read().await;
                let info = build_stake_deleg_info(
                    &guard.feed_index,
                    &cred,
                    replay_state.mainnet,
                    guard.current(),
                );
                (guard.db_handle(), info)
            };
            // Connect-time count, served off the lock via the two-step index path.
            let assets_count = match &db {
                Some(db) => db.stake_assets_count(&payload).await.unwrap_or(0) as u32,
                None => 0,
            };
            send_stake_info(
                &sender,
                &replay_state.chain_state,
                &stake_address,
                &cred,
                assets_count,
            )
            .await;

            let blocks = match &db {
                Some(db) => db
                    .stake_recent_blocks(&payload, i64::MAX, STAKE_REPLAY_BLOCKS)
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            };

            let exclude_slots: HashSet<u64> = blocks.iter().map(|(slot, ..)| *slot).collect();

            // Backward reconstruction: exact pre-block stake + full delegation history.
            let mut subject = match &db {
                Some(db) => Some(
                    build_subject_replay(
                        &replay_state.chain_state,
                        db,
                        &payload,
                        &cred,
                        &blocks,
                        &exclude_slots,
                        replay_state.mainnet,
                        None,
                    )
                    .await,
                ),
                None => None,
            };

            let mut replay_blocks: Vec<ReplayBlock> = blocks
                .into_iter()
                .map(|(slot, hash, number, epoch)| ReplayBlock {
                    slot,
                    hash,
                    number,
                    epoch,
                    pool_id: None,
                    pool_ticker: None,
                    filter_by_delegators: true,
                })
                .collect();
            let full_page = replay_blocks.len() >= STAKE_REPLAY_BLOCKS as usize;

            send_replay_blocks(
                &sse,
                &mut replay_blocks,
                &replay_delegators,
                &replay_filter,
                &deleg_info,
                0,
                subject.as_mut(),
            )
            .await;

            // Seed the client's pagination cursor (only if the page was full — else
            // there's nothing older). Stake feeds carry the walk anchor.
            if full_page {
                if let Some(sr) = &subject {
                    let (slot, epoch, stake) = sr.cursor();
                    let ev = crate::event::Event::ReplayCursor {
                        slot,
                        epoch: Some(epoch),
                        stake: Some(stake),
                    };
                    if let Some(e) = serialize_event(ev, sse.size) {
                        let _ = sse.sender.send(e).await;
                    }
                }
            }

            exclude_slots
        } else if let Some(addr) = replay_filter.address().map(str::to_string) {
            let addr_bytes = address_bytes(&addr).unwrap_or_default();
            // One short read lock: grab the db handle and build delegation info for
            // the address's stake credential (if any) from the feed index.
            let (db, deleg_info) = {
                let guard = replay_state.chain_state.read().await;
                let info = match filter::stake_credential(&addr) {
                    Some(cred) => build_stake_deleg_info(
                        &guard.feed_index,
                        &cred,
                        replay_state.mainnet,
                        guard.current(),
                    ),
                    None => HashMap::new(),
                };
                (guard.db_handle(), info)
            };
            // Connect-time count, served off the lock via the two-step index path.
            let assets_count = match &db {
                Some(db) => db.address_assets_count(&addr).await.unwrap_or(0) as u32,
                None => 0,
            };
            send_address_info(
                &sender,
                &replay_state.chain_state,
                &addr,
                &addr_bytes,
                replay_state.mainnet,
                assets_count,
            )
            .await;

            let blocks = match &db {
                Some(db) => db
                    .address_recent_blocks(&addr, i64::MAX, STAKE_REPLAY_BLOCKS)
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            };

            let exclude_slots: HashSet<u64> = blocks.iter().map(|(slot, ..)| *slot).collect();

            // No backward stake reconstruction for address feeds: the replayed blocks
            // and per-tx stake_change are scoped to this single payment address, but a
            // delegation's stake is credential-level (all addresses sharing the stake
            // key, plus rewards/withdrawals the address branch doesn't net out). The
            // walk would be wrong for multi-address credentials, so address feeds keep
            // the feed-index overlay (correct, recent-only) — see `SubjectReplay`.
            let mut subject: Option<SubjectReplay> = None;

            let mut replay_blocks: Vec<ReplayBlock> = blocks
                .into_iter()
                .map(|(slot, hash, number, epoch)| ReplayBlock {
                    slot,
                    hash,
                    number,
                    epoch,
                    pool_id: None,
                    pool_ticker: None,
                    filter_by_delegators: true,
                })
                .collect();
            // Slot-only cursor for address feeds (no walk); only if the page was full.
            let older_cursor_slot = (replay_blocks.len() >= STAKE_REPLAY_BLOCKS as usize)
                .then(|| replay_blocks.iter().map(|b| b.slot).min())
                .flatten();

            send_replay_blocks(
                &sse,
                &mut replay_blocks,
                &replay_delegators,
                &replay_filter,
                &deleg_info,
                0,
                subject.as_mut(),
            )
            .await;

            if let Some(slot) = older_cursor_slot {
                let ev = crate::event::Event::ReplayCursor {
                    slot,
                    epoch: None,
                    stake: None,
                };
                if let Some(e) = serialize_event(ev, sse.size) {
                    let _ = sse.sender.send(e).await;
                }
            }

            exclude_slots
        } else {
            HashSet::new()
        };

        send_filtered_snapshot(
            &sender,
            snapshot,
            &replay_filter,
            &replay_delegators,
            &exclude_slots,
            size,
        )
        .await;
    });

    // Seed the live stream's per-feed header-comparison state from the current
    // snapshot — pool/drep use balance only, stake/address use balance + count.
    let chain_state = state.chain_state.clone();
    let initial_live = {
        let guard = state.chain_state.read().await;
        let snap = guard.current();
        match &filter {
            filter::FeedFilter::Pool(ref hash) => LiveState {
                pool: snap.and_then(|s| s.pools.get(&hex::encode(hash)).cloned()),
                balance: snap.and_then(|s| State::pool_live_stake(s, hash)),
                assets_count: None,
            },
            filter::FeedFilter::DRep(ref bytes) => LiveState {
                pool: None,
                balance: snap.and_then(|s| State::drep_live_stake(s, bytes)),
                assets_count: None,
            },
            filter::FeedFilter::Stake(ref payload) => LiveState {
                pool: None,
                balance: snap.and_then(|s| s.stakes.get(&payload[1..]).copied()),
                // assets_count is queried fresh below (it's connect-only and
                // shouldn't drift across reads; the live-stream just passes it
                // through unchanged on each re-emit).
                assets_count: None,
            },
            filter::FeedFilter::Address(ref addr) => {
                let addr_b = address_bytes(addr).unwrap_or_default();
                LiveState {
                    pool: None,
                    balance: snap.and_then(|s| s.address_balances.get(&addr_b).copied()),
                    assets_count: None,
                }
            }
        }
    };

    // For stake/address feeds, query the static assets_count once and seed
    // the live state with it. The live stream re-emits the info event on
    // balance changes; it always passes this count through unchanged. The db
    // handle is taken under the chain_state read lock and the lock is
    // released *before* the query, so a slow count() can't queue every other
    // reader behind the sink's pending writer.
    let initial_live = if matches!(
        filter,
        filter::FeedFilter::Stake(_) | filter::FeedFilter::Address(_)
    ) {
        let db = state.chain_state.read().await.db_handle();
        let count = match (&filter, db) {
            (filter::FeedFilter::Stake(payload), Some(db)) => {
                db.stake_assets_count(payload).await.unwrap_or(0) as u32
            }
            (filter::FeedFilter::Address(addr), Some(db)) => {
                db.address_assets_count(addr).await.unwrap_or(0) as u32
            }
            _ => 0,
        };
        LiveState {
            assets_count: Some(count),
            ..initial_live
        }
    } else {
        initial_live
    };

    let replay = tokio_stream::wrappers::ReceiverStream::new(receiver);
    let live = build_live_stream(rx, filter, chain_state, initial_live, size, state.mainnet);
    let stream = replay.chain(live);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(serde::Serialize)]
struct AssetMedia {
    src: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    media_type: Option<String>,
    name: String,
}

#[derive(serde::Serialize)]
struct AssetMediaResponse {
    fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    media: Vec<AssetMedia>,
}

/// True for a syntactically valid CIP-14 fingerprint. Guards against injecting
/// `.`/`/`/`:` into the NFTCDN URL host (SSRF), since the fingerprint becomes the
/// request subdomain. NFTCDN does the real existence check.
fn is_valid_fingerprint(fp: &str) -> bool {
    fp.starts_with("asset1")
        && fp.len() <= 64
        && fp
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Resolve an asset's displayable media via NFTCDN. Fetches the (server-signed)
/// `/metadata`, then returns ready-signed URLs: one entry per `metadata.files`
/// entry when present (served from `/files/{i}/`), otherwise a single full-res
/// `/preview`. mediaType is passed through so the frontend media player can pick
/// the right renderer.
async fn asset_media(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(fingerprint): axum::extract::Path<String>,
) -> Result<axum::Json<AssetMediaResponse>, StatusCode> {
    if !is_valid_fingerprint(&fingerprint) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let meta_url = state.nftcdn.signed_url(&fingerprint, "metadata", "");
    let resp = state
        .http
        .get(&meta_url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(StatusCode::NOT_FOUND);
    }
    if !resp.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let body = resp.text().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let meta: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| StatusCode::BAD_GATEWAY)?;

    let inner = &meta["metadata"];
    let name = inner["name"]
        .as_str()
        .or_else(|| meta["name"].as_str())
        .map(str::to_string);

    let media = match inner["files"].as_array() {
        Some(files) if !files.is_empty() => files
            .iter()
            .enumerate()
            .map(|(i, f)| AssetMedia {
                src: state
                    .nftcdn
                    .signed_url(&fingerprint, &format!("files/{}/", i), ""),
                media_type: f["mediaType"].as_str().map(str::to_string),
                name: f["name"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}-{}", fingerprint, i)),
            })
            .collect(),
        _ => vec![AssetMedia {
            src: state.nftcdn.signed_url(&fingerprint, "preview", ""),
            media_type: inner["mediaType"].as_str().map(str::to_string),
            name: name.clone().unwrap_or_else(|| fingerprint.clone()),
        }],
    };

    Ok(axum::Json(AssetMediaResponse {
        fingerprint,
        name,
        media,
    }))
}

/// True for a syntactically valid Cardano policy id: exactly 56 lowercase hex
/// chars (28 bytes). Like `is_valid_fingerprint`, this rejects garbage before it
/// reaches the DB; the policy id itself never enters an NFTCDN host (only the
/// DB-sourced fingerprints do).
fn is_valid_policy_id(p: &str) -> bool {
    p.len() == 56
        && p.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(serde::Deserialize)]
struct PolicyQuery {
    cursor: Option<i64>,
}

#[derive(serde::Serialize)]
struct AssetItem {
    fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    src: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    srcset: String,
}

#[derive(serde::Serialize)]
struct AssetsResponse {
    assets: Vec<AssetItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<i64>,
    has_more: bool,
}

/// Assets returned per `/api/policy` page; the frontend keyset-paginates with
/// `?cursor=<last id>`.
///
/// Sized to fill the highest-resolution monitors with buffer in a single fetch:
/// the grid is a 136 px pitch (128 px cell + 8 px gap, see `AssetsGrid.svelte`),
/// so an 8K display (7680×4320) shows ~56×32 ≈ 1792 cells at once; 2048 covers
/// that plus headroom (≈4.5 screens on 4K), while the grid's windowing +
/// prefetch handle deeper scroll. The list query resolves metadata only for the
/// returned page (not the whole address), so this stays within the ~100 ms
/// per-query target even at this size.
const POLICY_PAGE_SIZE: i64 = 2048;

/// CSS px the policy-grid thumbnail is displayed at. The srcset density
/// descriptor for each nftcdn rung is `rung_size / POLICY_THUMB_PX`.
const POLICY_THUMB_PX: u16 = 128;

/// Decode an on-chain asset name to a display string. Strips the 4-byte
/// CIP-67 label prefix if present (so CIP-68 (222) "MyToken" reads as
/// "MyToken"), then UTF-8 decodes; returns None when the result is empty
/// or non-UTF-8 (caller falls back to the fingerprint).
fn decode_asset_name(name_bytes: &[u8]) -> Option<String> {
    let trimmed = crate::cip68::base_name(name_bytes);
    std::str::from_utf8(trimmed)
        .ok()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build the `(src, srcset)` pair of ready-signed nftcdn preview URLs for the
/// given fingerprint at the displayed CSS size, across the size ladder.
fn build_thumb_urls(nftcdn: &NftcdnConfig, fingerprint: &str) -> (String, String) {
    let rungs: Vec<(u16, String)> = SIZE_LADDER
        .iter()
        .map(|&size| {
            let url = nftcdn.signed_url(fingerprint, "preview", &format!("size={size}"));
            (size, url)
        })
        .collect();
    let src = rungs
        .first()
        .map(|(_, url)| url.clone())
        .unwrap_or_default();
    let srcset = if rungs.len() > 1 {
        rungs
            .iter()
            .map(|(size, url)| format!("{url} {}x", *size as f64 / POLICY_THUMB_PX as f64))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        String::new()
    };
    (src, srcset)
}

fn row_to_asset(state: &AppState, fingerprint: String, name_bytes: Vec<u8>) -> AssetItem {
    let name = decode_asset_name(&name_bytes);
    let (src, srcset) = build_thumb_urls(&state.nftcdn, &fingerprint);
    AssetItem {
        fingerprint,
        name,
        src,
        srcset,
    }
}

/// List a policy's assets, most-recently-first-minted first, keyset-paginated on
/// `multi_asset.id` (see `DbSync::assets_by_policy`). Returns ready-signed nftcdn
/// preview URLs — a `src` plus a multi-rung `srcset` so the browser picks the DPR
/// rung — meaning the frontend needs no signing key or subdomain. Stateless
/// db-sync read: no SSE, no in-memory state, no rollback path.
async fn policy_assets(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(policy_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<PolicyQuery>,
) -> Result<axum::Json<AssetsResponse>, StatusCode> {
    if !is_valid_policy_id(&policy_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let policy = hex::decode(&policy_id).map_err(|_| StatusCode::BAD_REQUEST)?;

    // Take only a db-handle clone under the lock; release before the slow
    // query so other readers/the sink aren't queued behind it.
    let db = state
        .chain_state
        .read()
        .await
        .db_handle()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let rows = db
        .assets_by_policy(&policy, query.cursor, POLICY_PAGE_SIZE)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let has_more = rows.len() as i64 == POLICY_PAGE_SIZE;
    let cursor = rows.last().map(|(id, ..)| *id);

    let assets = rows
        .into_iter()
        .map(|(_, fingerprint, name_bytes)| row_to_asset(&state, fingerprint, name_bytes))
        .collect();

    Ok(axum::Json(AssetsResponse {
        assets,
        cursor,
        has_more,
    }))
}

/// List assets currently owned by a payment address (`addr1…`) or stake
/// credential (`stake1…`). Same response shape and pagination scheme as
/// `policy_assets`, but **does not** filter CIP-68 reference NFTs — owned
/// listings show what the wallet actually holds. Only `Address` and `Stake`
/// filter kinds are accepted; pool/drep ids return 400.
async fn owned_assets(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(feed_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<PolicyQuery>,
) -> Result<axum::Json<AssetsResponse>, StatusCode> {
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;

    // Take the db handle under the read lock but release it before the query so a
    // page fetch never queues other readers behind the sink's pending writer.
    // The query itself is the bullet-proof two-step index path (see dbsync).
    let db = state
        .chain_state
        .read()
        .await
        .db_handle()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let rows = match &filter {
        filter::FeedFilter::Address(addr) => {
            db.address_assets(addr, query.cursor, POLICY_PAGE_SIZE)
                .await
        }
        filter::FeedFilter::Stake(payload) => {
            db.stake_assets(payload, query.cursor, POLICY_PAGE_SIZE)
                .await
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    }
    .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let has_more = rows.len() as i64 == POLICY_PAGE_SIZE;
    let cursor = rows.last().map(|(id, ..)| *id);
    let assets = rows
        .into_iter()
        .map(|(_, fingerprint, name_bytes)| row_to_asset(&state, fingerprint, name_bytes))
        .collect();

    Ok(axum::Json(AssetsResponse {
        assets,
        cursor,
        has_more,
    }))
}

#[derive(serde::Deserialize)]
struct OlderQuery {
    /// Fetch blocks strictly older than this slot (the client's current oldest).
    before: u64,
    /// Walk anchor from the previous page (stake feeds): the pre-block stake at
    /// `before`'s block, as a string (can exceed JS MAX_SAFE_INTEGER).
    stake: Option<String>,
    epoch: Option<u64>,
    dpr: Option<f64>,
}

/// Pagination cursor for the next (older) page. `stake`/`epoch` only for stake feeds.
#[derive(serde::Serialize)]
struct OlderCursor {
    slot: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stake: Option<String>,
}

#[derive(serde::Serialize)]
struct OlderResponse {
    blocks: Vec<crate::event::Event>,
    /// `None` ⇒ reached the address's first transaction (stop paginating).
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<OlderCursor>,
}

/// Infinite-scroll pagination: blocks older than `before` for a stake/address feed,
/// continuing the backward stake walk from the client's cursor. Mirrors the SSE
/// replay (reuses `process_replay_block`) but returns JSON. Stake/Address only.
async fn older_blocks(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(feed_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<OlderQuery>,
) -> Result<axum::Json<OlderResponse>, StatusCode> {
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;
    let limit = STAKE_REPLAY_BLOCKS;
    let size = rung_for_dpr(query.dpr.unwrap_or(1.0));

    // db handle under a short lock (released before the query); fetch the older page.
    let db = state
        .chain_state
        .read()
        .await
        .db_handle()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let (hash_raw, blocks) = match &filter {
        filter::FeedFilter::Stake(payload) => (
            Some(payload.clone()),
            db.stake_recent_blocks(payload, query.before as i64, limit)
                .await,
        ),
        filter::FeedFilter::Address(addr) => (
            stake_hash_raw_of(addr, state.mainnet),
            db.address_recent_blocks(addr, query.before as i64, limit)
                .await,
        ),
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    let blocks = blocks.map_err(|_| StatusCode::BAD_GATEWAY)?;
    if blocks.is_empty() {
        return Ok(axum::Json(OlderResponse {
            blocks: vec![],
            cursor: None,
        }));
    }
    // Reached the first tx when the db returns a short page (independent of filtering).
    let has_more = blocks.len() as i64 == limit;
    let exclude_slots: HashSet<u64> = blocks.iter().map(|(slot, ..)| *slot).collect();

    // Continue the walk from the cursor (stake feeds with a cursor). Older pages are
    // outside the 5-day feed-index window, so the (empty) overlay isn't needed.
    let mut subject = match (&filter, &hash_raw, &query.stake, query.epoch) {
        (filter::FeedFilter::Stake(_), Some(hr), Some(stake_str), Some(epoch)) => {
            let stake = stake_str
                .parse::<i64>()
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            let cred = hr[1..].to_vec();
            Some(
                build_subject_replay(
                    &state.chain_state,
                    &db,
                    hr,
                    &cred,
                    &blocks,
                    &exclude_slots,
                    state.mainnet,
                    Some((stake, epoch)),
                )
                .await,
            )
        }
        _ => None,
    };
    let deleg_info: HashMap<String, Vec<DelegationInfo>> = HashMap::new();
    let delegators: imbl::hashset::HashSet<Vec<u8>> = match &filter {
        filter::FeedFilter::Stake(payload) => imbl::hashset::HashSet::unit(payload[1..].to_vec()),
        _ => imbl::hashset::HashSet::new(),
    };

    let mut replay_blocks: Vec<ReplayBlock> = blocks
        .into_iter()
        .map(|(slot, hash, number, epoch)| ReplayBlock {
            slot,
            hash,
            number,
            epoch,
            pool_id: None,
            pool_ticker: None,
            filter_by_delegators: true,
        })
        .collect();
    replay_blocks.sort_by(|a, b| b.slot.cmp(&a.slot));

    let mut client = PeerClient::connect(state.n2n_addr, state.magic)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let ctx = ReplayCtx {
        nftcdn: &state.nftcdn,
        genesis: &state.genesis,
        chain_state: &state.chain_state,
        mainnet: state.mainnet,
    };
    let mut events = Vec::new();
    for block in &replay_blocks {
        if let Some(mut ev) = process_replay_block(
            &mut client,
            &ctx,
            block,
            &delegators,
            &filter,
            &deleg_info,
            0,
            subject.as_mut(),
        )
        .await
        {
            resolve_event_assets(&mut ev, size);
            events.push(ev);
        }
    }
    let _ = client.abort().await;

    let cursor = if !has_more {
        None
    } else if let Some(sr) = &subject {
        let (slot, epoch, stake) = sr.cursor();
        Some(OlderCursor {
            slot,
            epoch: Some(epoch),
            stake: Some(stake.to_string()),
        })
    } else {
        // Address feeds (no walk): slot-only cursor at the oldest block.
        replay_blocks
            .iter()
            .map(|b| b.slot)
            .min()
            .map(|slot| OlderCursor {
                slot,
                epoch: None,
                stake: None,
            })
    };

    Ok(axum::Json(OlderResponse {
        blocks: events,
        cursor,
    }))
}

/// Everything the SSE server needs to run. Bundled so `serve` takes one arg.
pub struct ServeConfig {
    pub addr: SocketAddr,
    pub bus: Arc<EventBus>,
    pub chain_state: Arc<RwLock<State>>,
    pub nftcdn: NftcdnConfig,
    pub genesis: GenesisConfig,
    pub n2n_addr: SocketAddr,
    pub magic: u64,
    pub mainnet: bool,
    pub catching_up: Arc<std::sync::atomic::AtomicBool>,
}

pub async fn serve(config: ServeConfig) {
    let ServeConfig {
        addr,
        bus,
        chain_state,
        nftcdn,
        genesis,
        n2n_addr,
        magic,
        mainnet,
        catching_up,
    } = config;
    // Wait for catch-up to complete before accepting SSE connections
    if catching_up.load(std::sync::atomic::Ordering::Relaxed) {
        info!("waiting for catch-up before starting SSE server");
        while catching_up.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        info!("catch-up complete, starting SSE server");
    }

    let state = AppState {
        bus,
        chain_state,
        nftcdn,
        http: reqwest::Client::new(),
        genesis,
        n2n_addr,
        magic,
        mainnet,
    };
    let app = Router::new()
        .route("/events", get(events))
        .route("/events/{feed_id}", get(filtered_events))
        .route("/api/asset/{fingerprint}", get(asset_media))
        .route("/api/policy/{policy_id}", get(policy_assets))
        .route("/api/assets/{feed_id}", get(owned_assets))
        .route("/api/feed/{feed_id}/older", get(older_blocks))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "starting SSE server");
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backward walk: anchor=1000 now. Rewards of 50/30 became spendable in epochs
    /// 640/639; an off-window withdrawal of 20 at slot 900. Two blocks, newest-first.
    #[test]
    fn pre_block_stake_undoes_rewards_withdrawals_and_block_delta() {
        let mut sr = SubjectReplay {
            running: 1000,
            subject_stake_address: String::new(),
            deleg_by_tx: HashMap::new(),
            reward_deltas: vec![(640, 50), (639, 30)], // epoch desc
            reward_cursor: 0,
            withdrawals: vec![(900, 20)], // slot desc
            wd_cursor: 0,
            last_slot: 0,
            last_epoch: 0,
        };

        // B0 in epoch 640 at slot 1000, net stake change +100. Nothing accrued after
        // it (640 not > 640; slot 900 not > 1000) → pre = 1000 - 100.
        assert_eq!(sr.pre_block_stake(640, 1000, 100), 900);

        // B1 in epoch 638 at slot 800, net change -40. Between B1 and B0: epoch
        // deltas 640(+50) & 639(+30) and the withdrawal(-20) are undone, then -(-40):
        // 900 - 50 - 30 + 20 + 40 = 880.
        assert_eq!(sr.pre_block_stake(638, 800, -40), 880);
    }

    /// Pagination continuity: walking B1 on a fresh page seeded with page 1's cursor
    /// (running = pre(B0), reward deltas capped at the cursor epoch, withdrawals below
    /// the cursor slot) reaches the same pre(B1) as a single deep walk over [B0, B1].
    #[test]
    fn cursor_continues_the_walk() {
        // Page 1: walk only B0 → cursor.
        let mut p1 = SubjectReplay {
            running: 1000,
            subject_stake_address: String::new(),
            deleg_by_tx: HashMap::new(),
            reward_deltas: vec![(640, 50), (639, 30)],
            reward_cursor: 0,
            withdrawals: vec![(900, 20)],
            wd_cursor: 0,
            last_slot: 0,
            last_epoch: 0,
        };
        assert_eq!(p1.pre_block_stake(640, 1000, 100), 900);
        let (cur_slot, cur_epoch, cur_stake) = p1.cursor();
        assert_eq!((cur_slot, cur_epoch, cur_stake), (1000, 640, 900));

        // Page 2: fresh walk seeded from the cursor — reward deltas with
        // spendable_epoch <= cur_epoch, off-window withdrawals below cur_slot.
        let mut p2 = SubjectReplay {
            running: cur_stake,
            subject_stake_address: String::new(),
            deleg_by_tx: HashMap::new(),
            reward_deltas: vec![(640, 50), (639, 30)], // epoch <= 640
            reward_cursor: 0,
            withdrawals: vec![(900, 20)], // slot < 1000
            wd_cursor: 0,
            last_slot: 0,
            last_epoch: 0,
        };
        assert_eq!(p2.pre_block_stake(638, 800, -40), 880);
    }
}
