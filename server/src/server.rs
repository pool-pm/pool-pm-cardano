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
use crate::model::{asset_fingerprint, drep_bech32_id, pool_bech32_id, Pool};
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
}

/// Build a `Stake` info event for a stake feed: ADA balance, available rewards,
/// and current pool/drep delegation — all read from the snapshot by the 28-byte
/// credential (`cred`).
fn stake_sse_event(
    stake_address: &str,
    cred: &[u8],
    snap: Option<&BlockSnapshot>,
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
) {
    let guard = chain_state.read().await;
    let _ = sender
        .send(stake_sse_event(stake_address, cred, guard.current()))
        .await;
}

/// The bech32 stake (reward) address of a payment address, or `None` for an
/// address with no stake part (enterprise/pointer). Preserves the key/script
/// credential type and network so it round-trips to db-sync.
fn stake_address_of(address: &str, mainnet: bool) -> Option<String> {
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
}

/// Send a payment-address info event: balance (sum of unspent UTXOs, no rewards)
/// and its stake address (for linking to the stake feed).
async fn send_address_info(
    sender: &Sender<Result<SseEvent, Infallible>>,
    chain_state: &RwLock<State>,
    address: &str,
    mainnet: bool,
) {
    let balance = {
        let guard = chain_state.read().await;
        guard.address_balance(address).await.unwrap_or(0)
    };
    let json = serde_json::to_string(&AddressEvent {
        kind: "Address",
        address,
        balance: balance.to_string(),
        stake_address: stake_address_of(address, mainnet),
    })
    .unwrap();
    let _ = sender.send(Ok(SseEvent::default().data(json))).await;
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
        crate::event::Event::Rollback { .. } | crate::event::Event::MempoolPrune { .. } => return,
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

            let message = crate::pallas::extract_cip20_message(tx);

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
    let (resolved, to_cache, decimals, handle_by_address) = {
        let guard = chain_state.read().await;
        let (resolved, to_cache) = guard.resolve_utxos_batch(&input_keys).await;
        let snap = guard.current();
        let decimals = snap.map(|s| s.decimals.clone()).unwrap_or_default();
        let handle_by_address = snap
            .map(|s| s.handle_by_address.clone())
            .unwrap_or_default();
        (resolved, to_cache, decimals, handle_by_address)
    };

    // Cache unspent UTXOs so subsequent feed loads skip db-sync
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
    pool_id: Option<String>,
    pool_ticker: Option<String>,
    /// If true, filter txs to only those involving pool delegators.
    filter_by_delegators: bool,
}

/// Fetch replay blocks via N2N and send as SSE events. Newest-first order.
/// `deleg_info` maps tx_hash -> Vec<DelegationInfo> for injecting correct delegation data.
async fn send_replay_blocks(
    sender: &Sender<Result<SseEvent, Infallible>>,
    blocks: &mut [ReplayBlock],
    delegators: &imbl::hashset::HashSet<Vec<u8>>,
    feed_filter: &filter::FeedFilter,
    deleg_info: &HashMap<String, Vec<DelegationInfo>>,
    stake_threshold: u64,
    nftcdn: &NftcdnConfig,
    genesis: &GenesisConfig,
    chain_state: &RwLock<State>,
    n2n_addr: SocketAddr,
    magic: u64,
    mainnet: bool,
    size: u16,
) {
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
    let mut sent = 0usize;
    for block in blocks.iter() {
        let hash_bytes = match hex::decode(&block.hash) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let point = Point::Specific(block.slot, hash_bytes);
        match client.blockfetch().fetch_single(point).await {
            Ok(cbor) => {
                let state_guard = chain_state.read().await;
                let (mut txs, cbor_pool_id, cbor_pool_ticker) = decode_block_txs(
                    &cbor,
                    nftcdn,
                    Some(&state_guard),
                    mainnet,
                    !block.filter_by_delegators,
                );
                drop(state_guard);
                resolve_block_inputs(&mut txs, chain_state, nftcdn).await;
                for tx in &mut txs {
                    tx.stake_credentials = filter::extract_stake_credentials(tx);
                }

                // Inject delegation info from feed index (correct from/to)
                for tx in &mut txs {
                    if let Some(delegations) = deleg_info.get(&tx.hash) {
                        tx.delegations = delegations.clone();
                    }
                }

                if block.filter_by_delegators {
                    // Computes UTXO changes + delegation impact in one pass
                    filter::apply_stake_changes(&mut txs, delegators, feed_filter);
                    let single_subject = matches!(
                        feed_filter,
                        filter::FeedFilter::Stake(_) | filter::FeedFilter::Address(_)
                    );
                    txs.retain(|tx| {
                        if single_subject {
                            // Single stake/payment address: show every tx that touches
                            // it, like the live path — not the pool/drep threshold.
                            feed_filter.matches_tx(tx, delegators)
                        } else {
                            !tx.delegations.is_empty()
                                || tx
                                    .stake_change
                                    .map_or(false, |sc| sc.unsigned_abs() > stake_threshold)
                        }
                    });
                }
                if txs.is_empty() && block.filter_by_delegators {
                    continue;
                }

                let pool_id = block.pool_id.clone().or(cbor_pool_id);
                let pool_ticker = block.pool_ticker.clone().or(cbor_pool_ticker);

                let event = crate::event::Event::Block {
                    slot: block.slot,
                    hash: block.hash.clone(),
                    number: block.number,
                    timestamp: slot_to_timestamp(block.slot, genesis),
                    pool_id,
                    pool_ticker,
                    txs,
                };
                if let Some(sse) = serialize_event(event, size) {
                    let _ = sender.send(sse).await;
                    sent += 1;
                    if sent >= MAX_REPLAY_BLOCKS {
                        break;
                    }
                }
            }
            Err(e) => {
                warn!(block.slot, "block-fetch failed: {}", e);
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
fn build_live_stream(
    rx: tokio::sync::broadcast::Receiver<crate::event::Event>,
    filter: filter::FeedFilter,
    chain_state: Arc<RwLock<State>>,
    last_pool: Option<Pool>,
    last_live_stake: Option<i64>,
    size: u16,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    futures::stream::unfold(
        (
            BroadcastStream::new(rx),
            filter,
            chain_state,
            last_pool,
            last_live_stake,
            std::collections::VecDeque::<Result<SseEvent, Infallible>>::new(),
            size,
        ),
        |(mut rx, filter, chain_state, mut last_pool, mut last_live_stake, mut buf, size)| async move {
            loop {
                if let Some(sse) = buf.pop_front() {
                    return Some((
                        sse,
                        (
                            rx,
                            filter,
                            chain_state,
                            last_pool,
                            last_live_stake,
                            buf,
                            size,
                        ),
                    ));
                }

                let event = rx.next().await?.ok()?;

                if matches!(
                    event,
                    crate::event::Event::Block { .. } | crate::event::Event::Rollback { .. }
                ) {
                    match &filter {
                        filter::FeedFilter::Pool(ref hash) => {
                            let (current_pool, current_live_stake, pool_event) = {
                                let guard = chain_state.read().await;
                                let snap = guard.current();
                                let pool =
                                    snap.and_then(|s| s.pools.get(&hex::encode(hash)).cloned());
                                let live_stake = snap.and_then(|s| State::pool_live_stake(s, hash));
                                let event = pool.as_ref().map(|p| pool_sse_event(p, snap));
                                (pool, live_stake, event)
                            };
                            if current_pool != last_pool || current_live_stake != last_live_stake {
                                if let Some(event) = pool_event {
                                    buf.push_back(event);
                                }
                                last_pool = current_pool;
                                last_live_stake = current_live_stake;
                            }
                        }
                        filter::FeedFilter::DRep(ref bytes) => {
                            let current_live_stake = {
                                let guard = chain_state.read().await;
                                guard
                                    .current()
                                    .and_then(|s| State::drep_live_stake(s, bytes))
                            };
                            if current_live_stake != last_live_stake {
                                let guard = chain_state.read().await;
                                buf.push_back(drep_sse_event(bytes, guard.current()));
                                last_live_stake = current_live_stake;
                            }
                        }
                        filter::FeedFilter::Stake(ref payload) => {
                            let cred = &payload[1..];
                            let current_balance = {
                                let guard = chain_state.read().await;
                                guard.current().and_then(|s| s.stakes.get(cred).copied())
                            };
                            if current_balance != last_live_stake {
                                let stake_address = filter.feed_id();
                                let guard = chain_state.read().await;
                                buf.push_back(stake_sse_event(
                                    &stake_address,
                                    cred,
                                    guard.current(),
                                ));
                                last_live_stake = current_balance;
                            }
                        }
                        // Address balance needs a db query, too costly to recompute
                        // per block; the header balance is set once at connect.
                        filter::FeedFilter::Address(_) => {}
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
                    return Some((
                        sse,
                        (
                            rx,
                            filter,
                            chain_state,
                            last_pool,
                            last_live_stake,
                            buf,
                            size,
                        ),
                    ));
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
                        pool_id: pool_id.clone(),
                        pool_ticker: pool_ticker.clone(),
                        filter_by_delegators: false,
                    }),
                    SlotAction::StakeChange(r) => replay_blocks.push(ReplayBlock {
                        slot: r.slot,
                        hash: r.hash,
                        number: r.number,
                        pool_id: None,
                        pool_ticker: None,
                        filter_by_delegators: true,
                    }),
                }
            }

            // Send blocks via N2N with delegation info injected
            send_replay_blocks(
                &sender,
                &mut replay_blocks,
                &replay_delegators,
                &replay_filter,
                &deleg_info,
                stake_threshold,
                &replay_state.nftcdn,
                &replay_state.genesis,
                &replay_state.chain_state,
                replay_state.n2n_addr,
                replay_state.magic,
                replay_state.mainnet,
                size,
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
                        pool_id: None,
                        pool_ticker: None,
                        filter_by_delegators: true,
                    });
                }
            }

            send_replay_blocks(
                &sender,
                &mut replay_blocks,
                &replay_delegators,
                &replay_filter,
                &deleg_info,
                stake_threshold,
                &replay_state.nftcdn,
                &replay_state.genesis,
                &replay_state.chain_state,
                replay_state.n2n_addr,
                replay_state.magic,
                replay_state.mainnet,
                size,
            )
            .await;

            exclude_slots
        } else if let Some(payload) = replay_filter.stake_payload().cloned() {
            let cred = payload[1..].to_vec();
            let stake_address = replay_filter.feed_id();
            send_stake_info(&sender, &replay_state.chain_state, &stake_address, &cred).await;

            let blocks = {
                let guard = replay_state.chain_state.read().await;
                guard
                    .stake_recent_blocks(&payload, STAKE_REPLAY_BLOCKS)
                    .await
            }
            .unwrap_or_default();

            let exclude_slots: HashSet<u64> = blocks.iter().map(|(slot, _, _)| *slot).collect();

            let mut replay_blocks: Vec<ReplayBlock> = blocks
                .into_iter()
                .map(|(slot, hash, number)| ReplayBlock {
                    slot,
                    hash,
                    number,
                    pool_id: None,
                    pool_ticker: None,
                    filter_by_delegators: true,
                })
                .collect();

            send_replay_blocks(
                &sender,
                &mut replay_blocks,
                &replay_delegators,
                &replay_filter,
                &HashMap::new(),
                0,
                &replay_state.nftcdn,
                &replay_state.genesis,
                &replay_state.chain_state,
                replay_state.n2n_addr,
                replay_state.magic,
                replay_state.mainnet,
                size,
            )
            .await;

            exclude_slots
        } else if let Some(addr) = replay_filter.address().map(str::to_string) {
            send_address_info(
                &sender,
                &replay_state.chain_state,
                &addr,
                replay_state.mainnet,
            )
            .await;

            let blocks = {
                let guard = replay_state.chain_state.read().await;
                guard
                    .address_recent_blocks(&addr, STAKE_REPLAY_BLOCKS)
                    .await
            }
            .unwrap_or_default();

            let exclude_slots: HashSet<u64> = blocks.iter().map(|(slot, _, _)| *slot).collect();

            let mut replay_blocks: Vec<ReplayBlock> = blocks
                .into_iter()
                .map(|(slot, hash, number)| ReplayBlock {
                    slot,
                    hash,
                    number,
                    pool_id: None,
                    pool_ticker: None,
                    filter_by_delegators: true,
                })
                .collect();

            send_replay_blocks(
                &sender,
                &mut replay_blocks,
                &replay_delegators,
                &replay_filter,
                &HashMap::new(),
                0,
                &replay_state.nftcdn,
                &replay_state.genesis,
                &replay_state.chain_state,
                replay_state.n2n_addr,
                replay_state.magic,
                replay_state.mainnet,
                size,
            )
            .await;

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

    // Build live stream with pool/drep change detection
    let chain_state = state.chain_state.clone();
    let (last_pool, last_live_stake) = match &filter {
        filter::FeedFilter::Pool(ref hash) => {
            let guard = state.chain_state.read().await;
            let snap = guard.current();
            let pool = snap.and_then(|s| s.pools.get(&hex::encode(hash)).cloned());
            let live_stake = snap.and_then(|s| State::pool_live_stake(s, hash));
            (pool, live_stake)
        }
        filter::FeedFilter::DRep(ref bytes) => {
            let guard = state.chain_state.read().await;
            let snap = guard.current();
            let live_stake = snap.and_then(|s| State::drep_live_stake(s, bytes));
            (None, live_stake)
        }
        filter::FeedFilter::Stake(ref payload) => {
            let guard = state.chain_state.read().await;
            let balance = guard
                .current()
                .and_then(|s| s.stakes.get(&payload[1..]).copied());
            (None, balance)
        }
        // Address feed has no live header tracking (balance set once at connect).
        filter::FeedFilter::Address(_) => (None, None),
    };

    let replay = tokio_stream::wrappers::ReceiverStream::new(receiver);
    let live = build_live_stream(rx, filter, chain_state, last_pool, last_live_stake, size);
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
struct PolicyAsset {
    fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    src: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    srcset: String,
}

#[derive(serde::Serialize)]
struct PolicyResponse {
    policy_id: String,
    assets: Vec<PolicyAsset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<i64>,
    has_more: bool,
}

/// Assets returned per `/api/policy` page; the frontend keyset-paginates with
/// `?cursor=<last id>`.
const POLICY_PAGE_SIZE: i64 = 60;

/// CSS px the policy-grid thumbnail is displayed at. The srcset density
/// descriptor for each nftcdn rung is `rung_size / POLICY_THUMB_PX`.
const POLICY_THUMB_PX: u16 = 128;

/// List a policy's assets, most-recently-first-minted first, keyset-paginated on
/// `multi_asset.id` (see `DbSync::assets_by_policy`). Returns ready-signed nftcdn
/// preview URLs — a `src` plus a multi-rung `srcset` so the browser picks the DPR
/// rung — meaning the frontend needs no signing key or subdomain. Stateless
/// db-sync read: no SSE, no in-memory state, no rollback path.
async fn policy_assets(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(policy_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<PolicyQuery>,
) -> Result<axum::Json<PolicyResponse>, StatusCode> {
    if !is_valid_policy_id(&policy_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let policy = hex::decode(&policy_id).map_err(|_| StatusCode::BAD_REQUEST)?;

    let rows = {
        let guard = state.chain_state.read().await;
        guard
            .assets_by_policy(&policy, query.cursor, POLICY_PAGE_SIZE)
            .await
    }
    .ok_or(StatusCode::BAD_GATEWAY)?;

    let has_more = rows.len() as i64 == POLICY_PAGE_SIZE;
    let cursor = rows.last().map(|(id, ..)| *id);

    let assets = rows
        .into_iter()
        .map(|(_, fingerprint, name_bytes)| {
            let name = std::str::from_utf8(&name_bytes)
                .ok()
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let rungs: Vec<(u16, String)> = SIZE_LADDER
                .iter()
                .map(|&size| {
                    let url =
                        state
                            .nftcdn
                            .signed_url(&fingerprint, "preview", &format!("size={size}"));
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
            PolicyAsset {
                fingerprint,
                name,
                src,
                srcset,
            }
        })
        .collect();

    Ok(axum::Json(PolicyResponse {
        policy_id,
        assets,
        cursor,
        has_more,
    }))
}

pub async fn serve(
    addr: SocketAddr,
    bus: Arc<EventBus>,
    chain_state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,
    genesis: GenesisConfig,
    n2n_addr: SocketAddr,
    magic: u64,
    mainnet: bool,
    catching_up: Arc<std::sync::atomic::AtomicBool>,
) {
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
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "starting SSE server");
    axum::serve(listener, app).await.unwrap();
}
