use axum::{
    http::StatusCode,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::get,
    Router,
};
use futures::stream::Stream;
use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::facades::PeerClient;
use pallas::network::miniprotocols::Point;
use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

use crate::event::{AssetInfo, BlockTx, TxInput, TxOutputInfo};
use crate::event_bus::EventBus;
use crate::filter;
use crate::model::{asset_fingerprint, pool_bech32_id, Pool};
use crate::nftcdn::NftcdnConfig;
use crate::state::{BlockSnapshot, State};

#[derive(Clone)]
struct AppState {
    bus: Arc<EventBus>,
    chain_state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,
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

// --- SSE event builders ---

fn config_event(nftcdn_subdomain: &str, genesis: &GenesisConfig) -> Result<SseEvent, Infallible> {
    let genesis_json = serde_json::to_string(genesis).unwrap();
    Ok(SseEvent::default().data(format!(
        "{{\"type\":\"Config\",\"nftcdn\":\"{}\",\"genesis\":{}}}",
        nftcdn_subdomain, genesis_json
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

fn serialize_event(event: crate::event::Event) -> Option<Result<SseEvent, Infallible>> {
    serde_json::to_string(&event)
        .ok()
        .map(|json| Ok(SseEvent::default().data(json)))
}

// --- Block decoding ---

/// Decode a block CBOR into a BlockTx list.
fn decode_block_txs(cbor: &[u8], nftcdn: &NftcdnConfig) -> Vec<BlockTx> {
    let block = match MultiEraBlock::decode(cbor) {
        Ok(b) => b,
        Err(_) => return vec![],
    };

    block
        .txs()
        .iter()
        .map(|tx| {
            let inputs = tx
                .inputs()
                .iter()
                .map(|input| TxInput {
                    tx_hash: input.hash().to_string(),
                    index: input.index() as i16,
                    address: None,
                    lovelace: 0,
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
                                    let fp = asset_fingerprint(&policy_id, asset.name());
                                    let name = std::str::from_utf8(asset.name())
                                        .ok()
                                        .filter(|s| !s.is_empty())
                                        .map(String::from);
                                    let tk = nftcdn.compute_tk(&fp, "preview", 128);
                                    Some(AssetInfo {
                                        fingerprint: fp,
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
                hash: tx.hash().to_string(),
                fee: tx.fee().unwrap_or(0),
                size: tx.size(),
                inputs,
                outputs,
                expiry: None,
                delegations: vec![],
                stake_credentials: vec![],
            }
        })
        .collect()
}

/// Resolve input addresses for a list of transactions via batch db-sync query.
async fn resolve_block_inputs(txs: &mut Vec<BlockTx>, chain_state: &RwLock<State>) {
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
    let resolved = {
        let guard = chain_state.read().await;
        guard.resolve_utxos_batch(&input_keys).await
    };
    for tx in txs {
        for inp in &mut tx.inputs {
            let key = (
                hex::decode(&inp.tx_hash).unwrap_or_default(),
                inp.index,
            );
            if let Some((addr, lovelace)) = resolved.get(&key) {
                inp.address = Some(addr.clone());
                inp.lovelace = *lovelace;
            }
        }
    }
}

// --- Replay: send historical events through mpsc channel ---

/// Send pool block history fetched via N2N block-fetch protocol.
async fn send_pool_history(
    sender: &Sender<Result<SseEvent, Infallible>>,
    block_points: &[(u64, String, u64)],
    pool_id: &Option<String>,
    pool_ticker: &Option<String>,
    nftcdn: &NftcdnConfig,
    genesis: &GenesisConfig,
    chain_state: &RwLock<State>,
    n2n_addr: SocketAddr,
    magic: u64,
) {
    if block_points.is_empty() {
        return;
    }
    let mut client = match PeerClient::connect(n2n_addr, magic).await {
        Ok(c) => c,
        Err(_) => {
            warn!("N2N connect to {} failed", n2n_addr);
            return;
        }
    };
    // block_points is newest-first; iterate in reverse for oldest-first
    for &(slot, ref hash_hex, number) in block_points.iter().rev() {
        let hash_bytes = match hex::decode(hash_hex) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let point = Point::Specific(slot, hash_bytes);
        match client.blockfetch().fetch_single(point).await {
            Ok(cbor) => {
                let mut txs = decode_block_txs(&cbor, nftcdn);
                resolve_block_inputs(&mut txs, chain_state).await;
                let event = crate::event::Event::Block {
                    slot,
                    hash: hash_hex.clone(),
                    number,
                    timestamp: slot_to_timestamp(slot, genesis),
                    pool_id: pool_id.clone(),
                    pool_ticker: pool_ticker.clone(),
                    txs,
                };
                if let Some(sse) = serialize_event(event) {
                    let _ = sender.send(sse).await;
                }
            }
            Err(e) => {
                warn!(slot, "block-fetch failed: {}", e);
            }
        }
    }
    let _ = client.abort().await;
}

/// Fetch blocks with large outputs to pool delegators, filter txs, and send.
async fn send_stake_change_blocks(
    sender: &Sender<Result<SseEvent, Infallible>>,
    blocks: &[(u64, String, u64, Option<Vec<u8>>, Option<String>)],
    delegators: &imbl::hashset::HashSet<Vec<u8>>,
    _filter: &filter::FeedFilter,
    nftcdn: &NftcdnConfig,
    genesis: &GenesisConfig,
    chain_state: &RwLock<State>,
    n2n_addr: SocketAddr,
    magic: u64,
) {
    if blocks.is_empty() {
        return;
    }
    let mut client = match PeerClient::connect(n2n_addr, magic).await {
        Ok(c) => c,
        Err(_) => {
            warn!("N2N connect for stake blocks failed");
            return;
        }
    };
    // Iterate oldest-first
    for (slot, hash_hex, number, pool_hash, pool_ticker) in blocks.iter().rev() {
        let hash_bytes = match hex::decode(hash_hex) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let point = Point::Specific(*slot, hash_bytes);
        match client.blockfetch().fetch_single(point).await {
            Ok(cbor) => {
                let mut txs = decode_block_txs(&cbor, nftcdn);
                resolve_block_inputs(&mut txs, chain_state).await;
                for tx in &mut txs {
                    tx.stake_credentials = filter::extract_stake_credentials(tx);
                }
                let filtered: Vec<BlockTx> = txs
                    .into_iter()
                    .filter(|tx| {
                        tx.stake_credentials
                            .iter()
                            .any(|c| delegators.contains(c))
                    })
                    .collect();
                if filtered.is_empty() {
                    continue;
                }
                let pool_id = pool_hash.as_ref().map(|h| pool_bech32_id(h));
                let event = crate::event::Event::Block {
                    slot: *slot,
                    hash: hash_hex.clone(),
                    number: *number,
                    timestamp: slot_to_timestamp(*slot, genesis),
                    pool_id,
                    pool_ticker: pool_ticker.clone(),
                    txs: filtered,
                };
                if let Some(sse) = serialize_event(event) {
                    let _ = sender.send(sse).await;
                }
            }
            Err(e) => {
                warn!(slot, "stake block-fetch failed: {}", e);
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
) {
    for event in snapshot {
        if let Some(filtered) = filter.filter_event(&event, delegators) {
            if let crate::event::Event::Block { slot, .. } = &filtered {
                if exclude_slots.contains(slot) {
                    continue;
                }
            }
            if let Some(sse) = serialize_event(filtered) {
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

/// Build the live stream that detects pool parameter/stake changes and filters events.
fn build_live_stream(
    rx: tokio::sync::broadcast::Receiver<crate::event::Event>,
    filter: filter::FeedFilter,
    chain_state: Arc<RwLock<State>>,
    last_pool: Option<Pool>,
    last_live_stake: Option<i64>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    futures::stream::unfold(
        (
            BroadcastStream::new(rx),
            filter,
            chain_state,
            last_pool,
            last_live_stake,
            std::collections::VecDeque::<Result<SseEvent, Infallible>>::new(),
        ),
        |(mut rx, filter, chain_state, mut last_pool, mut last_live_stake, mut buf)| async move {
            loop {
                if let Some(sse) = buf.pop_front() {
                    return Some((sse, (rx, filter, chain_state, last_pool, last_live_stake, buf)));
                }

                let event = rx.next().await?.ok()?;

                if let filter::FeedFilter::Pool(ref hash) = filter {
                    if matches!(
                        event,
                        crate::event::Event::Block { .. } | crate::event::Event::Rollback { .. }
                    ) {
                        let (current_pool, current_live_stake, pool_event) = {
                            let guard = chain_state.read().await;
                            let snap = guard.current();
                            let pool = snap
                                .and_then(|s| s.pools.get(&hex::encode(hash)).cloned());
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
                }

                let delegators = {
                    let guard = chain_state.read().await;
                    guard
                        .current()
                        .and_then(|snap| filter.delegators(snap))
                        .cloned()
                        .unwrap_or_default()
                };
                if let Some(sse) = filter
                    .filter_event(&event, &delegators)
                    .and_then(serialize_event)
                {
                    return Some((sse, (rx, filter, chain_state, last_pool, last_live_stake, buf)));
                }
            }
        },
    )
}

// --- SSE endpoints ---

async fn events(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (snapshot, rx) = state.bus.subscribe().await;

    let config = Some(config_event(state.nftcdn.subdomain, &state.genesis));

    let init = if snapshot.is_empty() {
        None
    } else {
        serde_json::to_string(&snapshot)
            .ok()
            .map(|json| Ok(SseEvent::default().data(json)))
    };
    let replay = futures::stream::iter(config.into_iter().chain(init));
    let live = BroadcastStream::new(rx).filter_map(|result| result.ok().and_then(serialize_event));
    let stream = replay.chain(live);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn filtered_events(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(feed_id): axum::extract::Path<String>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, StatusCode> {
    info!("/events/{feed_id}");
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;
    let (snapshot, rx) = state.bus.subscribe().await;

    let (delegators, pool_id, pool_ticker, pool_hash) = {
        let guard = state.chain_state.read().await;
        let delegators = guard
            .current()
            .and_then(|snap| filter.delegators(snap))
            .cloned()
            .unwrap_or_default();
        let (pool_id, pool_ticker, pool_hash) = extract_pool_meta(guard.current(), &filter);
        (delegators, pool_id, pool_ticker, pool_hash)
    };

    // Spawn replay task: config → pool info → history blocks → snapshot
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(32);
    let replay_filter = filter.clone();
    let replay_delegators = delegators.clone();
    let replay_state = state.clone();

    tokio::spawn(async move {
        let _ = sender
            .send(config_event(replay_state.nftcdn.subdomain, &replay_state.genesis))
            .await;

        let exclude_slots = if let Some(ref ph) = pool_hash {
            let block_points = {
                let guard = replay_state.chain_state.read().await;
                guard.pool_recent_blocks(ph, 20).await
            };
            send_pool_info(&sender, &replay_state.chain_state, ph).await;

            let slots: HashSet<u64> = block_points.iter().map(|(s, _, _)| *s).collect();
            send_pool_history(
                &sender, &block_points, &pool_id, &pool_ticker,
                &replay_state.nftcdn, &replay_state.genesis, &replay_state.chain_state,
                replay_state.n2n_addr, replay_state.magic,
            )
            .await;

            // Fetch blocks containing the largest outputs to pool delegators (last epoch)
            let boundary_slot = {
                let guard = replay_state.chain_state.read().await;
                guard.current().map(|s| s.slot).unwrap_or(0)
            }
            .saturating_sub(replay_state.genesis.shelley_epoch_length as u64);
            let stake_header: u8 = if replay_state.mainnet { 0xe1 } else { 0xe0 };
            let delegator_hash_raws: Vec<Vec<u8>> = replay_delegators
                .iter()
                .map(|cred| [&[stake_header][..], cred].concat())
                .collect();
            let stake_blocks: Vec<_> = {
                let guard = replay_state.chain_state.read().await;
                guard
                    .pool_stake_change_blocks(boundary_slot, &delegator_hash_raws, 30)
                    .await
            }
            .into_iter()
            .filter(|(s, _, _, _, _)| !slots.contains(s))
            .collect();
            let mut all_slots = slots;
            all_slots.extend(stake_blocks.iter().map(|(s, _, _, _, _)| *s));
            send_stake_change_blocks(
                &sender,
                &stake_blocks,
                &replay_delegators,
                &replay_filter,
                &replay_state.nftcdn,
                &replay_state.genesis,
                &replay_state.chain_state,
                replay_state.n2n_addr,
                replay_state.magic,
            )
            .await;

            all_slots
        } else {
            HashSet::new()
        };

        send_filtered_snapshot(&sender, snapshot, &replay_filter, &replay_delegators, &exclude_slots).await;
    });

    // Build live stream with pool change detection
    let chain_state = state.chain_state.clone();
    let (last_pool, last_live_stake) = if let filter::FeedFilter::Pool(ref hash) = filter {
        let guard = state.chain_state.read().await;
        let snap = guard.current();
        let pool = snap.and_then(|s| s.pools.get(&hex::encode(hash)).cloned());
        let live_stake = snap.and_then(|s| State::pool_live_stake(s, hash));
        (pool, live_stake)
    } else {
        (None, None)
    };

    let replay = tokio_stream::wrappers::ReceiverStream::new(receiver);
    let live = build_live_stream(rx, filter, chain_state, last_pool, last_live_stake);
    let stream = replay.chain(live);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
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
) {
    let state = AppState {
        bus,
        chain_state,
        nftcdn,
        genesis,
        n2n_addr,
        magic,
        mainnet,
    };
    let app = Router::new()
        .route("/events", get(events))
        .route("/events/{feed_id}", get(filtered_events))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "starting SSE server");
    axum::serve(listener, app).await.unwrap();
}
