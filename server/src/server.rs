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

use crate::event::{AssetInfo, BlockTx, DelegationInfo, TxInput, TxOutputInfo};
use crate::event_bus::EventBus;
use crate::filter;
use crate::model::{asset_fingerprint, pool_bech32_id, Pool};
use crate::nftcdn::NftcdnConfig;
use crate::state::feed_index::{BlockRef, DelegationEntry};
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

/// Decode a block CBOR into a BlockTx list and extract the minting pool info.
fn decode_block_txs(
    cbor: &[u8],
    nftcdn: &NftcdnConfig,
    state: Option<&State>,
    mainnet: bool,
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

            let delegations = state
                .map(|s| crate::mempool::extract_delegations(tx, s, mainnet))
                .unwrap_or_default();

            BlockTx {
                hash: tx.hash().to_string(),
                fee: tx.fee().unwrap_or(0),
                size: tx.size(),
                inputs,
                outputs,
                expiry: None,
                delegations,
                stake_change: None,
                stake_credentials: vec![],
            }
        })
        .collect();

    (txs, block_pool_id, block_pool_ticker)
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
            let key = (hex::decode(&inp.tx_hash).unwrap_or_default(), inp.index);
            if let Some((addr, lovelace)) = resolved.get(&key) {
                inp.address = Some(addr.clone());
                inp.lovelace = *lovelace;
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
async fn send_replay_blocks(
    sender: &Sender<Result<SseEvent, Infallible>>,
    blocks: &mut [ReplayBlock],
    delegators: &imbl::hashset::HashSet<Vec<u8>>,
    nftcdn: &NftcdnConfig,
    genesis: &GenesisConfig,
    chain_state: &RwLock<State>,
    n2n_addr: SocketAddr,
    magic: u64,
    mainnet: bool,
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
    for block in blocks.iter() {
        let hash_bytes = match hex::decode(&block.hash) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let point = Point::Specific(block.slot, hash_bytes);
        match client.blockfetch().fetch_single(point).await {
            Ok(cbor) => {
                let state_guard = chain_state.read().await;
                let (mut txs, cbor_pool_id, cbor_pool_ticker) =
                    decode_block_txs(&cbor, nftcdn, Some(&state_guard), mainnet);
                drop(state_guard);
                resolve_block_inputs(&mut txs, chain_state).await;

                let txs = if block.filter_by_delegators {
                    txs.into_iter()
                        .filter(|tx| {
                            tx.stake_credentials
                                .iter()
                                .any(|cred| delegators.contains(cred))
                        })
                        .collect()
                } else {
                    txs
                };
                if txs.is_empty() {
                    continue;
                }

                // Use provided pool_id or fall back to what we extracted from CBOR
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
                if let Some(sse) = serialize_event(event) {
                    let _ = sender.send(sse).await;
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

/// Construct a delegation-only SSE event from a DelegationEntry.
fn delegation_entry_to_event(
    entry: &DelegationEntry,
    genesis: &GenesisConfig,
) -> crate::event::Event {
    let deleg_info = DelegationInfo {
        stake_address: entry.stake_address.clone(),
        from_pool_id: entry.from.as_ref().map(|t| t.id.clone()),
        from_ticker: entry.from.as_ref().and_then(|t| t.label.clone()),
        to_pool_id: entry.to.as_ref().map(|t| t.id.clone()),
        to_ticker: entry.to.as_ref().and_then(|t| t.label.clone()),
        live_stake: entry.live_stake,
    };
    let tx = BlockTx {
        hash: entry.tx_hash.clone(),
        fee: 0,
        size: 0,
        inputs: vec![],
        outputs: vec![],
        expiry: None,
        delegations: vec![deleg_info],
        stake_change: None,
        stake_credentials: vec![],
    };
    let block_pool_id = if entry.block_pool_hash.is_empty() {
        None
    } else {
        Some(pool_bech32_id(&entry.block_pool_hash))
    };
    crate::event::Event::Block {
        slot: entry.slot,
        hash: entry.block_hash.clone(),
        number: entry.block_no,
        timestamp: slot_to_timestamp(entry.slot, genesis),
        pool_id: block_pool_id,
        pool_ticker: entry.block_pool_ticker.clone(),
        txs: vec![tx],
    }
}

/// Categories for feed index replay actions, by priority.
enum SlotAction {
    PoolMinted(BlockRef),
    StakeChange(BlockRef),
    DelegationOnly(Vec<DelegationEntry>),
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
                    return Some((
                        sse,
                        (rx, filter, chain_state, last_pool, last_live_stake, buf),
                    ));
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
                            let pool = snap.and_then(|s| s.pools.get(&hex::encode(hash)).cloned());
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
                    return Some((
                        sse,
                        (rx, filter, chain_state, last_pool, last_live_stake, buf),
                    ));
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
            ))
            .await;

        let exclude_slots = if let Some(ref ph) = pool_hash {
            send_pool_info(&sender, &replay_state.chain_state, ph).await;

            // Read feed index data
            let (minted, stake_changes, delegations) = {
                let guard = replay_state.chain_state.read().await;
                (
                    guard.feed_index.pool_minted_blocks(ph).to_vec(),
                    guard.feed_index.pool_stake_change_blocks(ph).to_vec(),
                    guard
                        .feed_index
                        .pool_delegation_entries(ph)
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            };

            // Build slot actions with priority: PoolMinted > StakeChange > DelegationOnly
            let mut slot_map: HashMap<u64, SlotAction> = HashMap::new();

            for r in &minted {
                slot_map.insert(r.slot, SlotAction::PoolMinted(r.clone()));
            }
            for r in &stake_changes {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::StakeChange(r.clone()));
            }
            let mut deleg_by_slot: HashMap<u64, Vec<DelegationEntry>> = HashMap::new();
            for d in delegations {
                deleg_by_slot.entry(d.slot).or_default().push(d);
            }
            for (slot, entries) in deleg_by_slot {
                slot_map
                    .entry(slot)
                    .or_insert(SlotAction::DelegationOnly(entries));
            }

            let exclude_slots: HashSet<u64> = slot_map.keys().copied().collect();

            // Separate block fetches from delegation-only events
            let mut replay_blocks: Vec<ReplayBlock> = Vec::new();
            let mut deleg_only: Vec<(u64, Vec<DelegationEntry>)> = Vec::new();

            // Sort newest-first
            let mut actions: Vec<(u64, SlotAction)> = slot_map.into_iter().collect();
            actions.sort_by(|a, b| b.0.cmp(&a.0));

            for (_slot, action) in actions {
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
                        pool_id: None, // extracted from CBOR during fetch
                        pool_ticker: None,
                        filter_by_delegators: true,
                    }),
                    SlotAction::DelegationOnly(entries) => {
                        deleg_only.push((_slot, entries));
                    }
                }
            }

            // Send blocks via N2N (newest-first)
            send_replay_blocks(
                &sender,
                &mut replay_blocks,
                &replay_delegators,
                &replay_state.nftcdn,
                &replay_state.genesis,
                &replay_state.chain_state,
                replay_state.n2n_addr,
                replay_state.magic,
                replay_state.mainnet,
            )
            .await;

            // Send delegation-only events (already newest-first from sorted actions)
            for (_, entries) in deleg_only {
                for entry in entries {
                    let event = delegation_entry_to_event(&entry, &replay_state.genesis);
                    if let Some(sse) = serialize_event(event) {
                        let _ = sender.send(sse).await;
                    }
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
        )
        .await;
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
