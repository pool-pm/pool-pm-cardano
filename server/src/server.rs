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
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
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

fn config_event(nftcdn_subdomain: &str, genesis: &GenesisConfig) -> Result<SseEvent, Infallible> {
    let genesis_json = serde_json::to_string(genesis).unwrap();
    Ok(SseEvent::default().data(format!(
        "{{\"type\":\"Config\",\"nftcdn\":\"{}\",\"genesis\":{}}}",
        nftcdn_subdomain, genesis_json
    )))
}

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
        let (pool_id, pool_ticker, pool_hash) =
            if let filter::FeedFilter::Pool(ref hash) = filter {
                let (pool_id, pool_ticker) = guard
                    .current()
                    .and_then(|snap| snap.pools.get(&hex::encode(hash)))
                    .map(|p| (Some(pool_bech32_id(&p.hash_raw)), p.ticker.clone()))
                    .unwrap_or((None, None));
                (pool_id, pool_ticker, Some(hash.clone()))
            } else {
                (None, None, None)
            };
        (delegators, pool_id, pool_ticker, pool_hash)
    };

    // Stream replay via mpsc: blocks arrive one by one as they're fetched
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(32);

    let nftcdn = state.nftcdn.clone();
    let genesis = state.genesis.clone();
    let n2n_addr = state.n2n_addr;
    let magic = state.magic;
    let nftcdn_sub = state.nftcdn.subdomain;
    let chain_state_clone = state.chain_state.clone();
    let replay_filter = filter.clone();
    let replay_delegators = delegators.clone();

    tokio::spawn(async move {
        // Config
        let _ = sender.send(config_event(nftcdn_sub, &genesis)).await;

        // Historical blocks via db-sync query + N2N block-fetch (streamed)
        if let Some(ref ph) = pool_hash {
            let block_points = {
                let guard = chain_state_clone.read().await;
                guard.pool_recent_blocks(ph, 20).await
            };

            // Pool info
            {
                let guard = chain_state_clone.read().await;
                if let Some(snap) = guard.current() {
                    if let Some(pool) = snap.pools.get(&hex::encode(ph)) {
                        let _ = sender
                            .send(pool_sse_event(pool, Some(snap)))
                            .await;
                    }
                }
            }
            let history_slots: std::collections::HashSet<u64> =
                block_points.iter().map(|(slot, _, _)| *slot).collect();

            if !block_points.is_empty() {
                if let Ok(mut client) = PeerClient::connect(n2n_addr, magic).await {
                    for &(slot, ref hash_hex, number) in block_points.iter().rev() {
                        let hash_bytes = match hex::decode(hash_hex) {
                            Ok(b) => b,
                            Err(_) => continue,
                        };
                        let point = Point::Specific(slot, hash_bytes);
                        match client.blockfetch().fetch_single(point).await {
                            Ok(cbor) => {
                                let mut txs = decode_block_txs(&cbor, &nftcdn);

                                // Batch-resolve input addresses
                                let input_keys: Vec<(Vec<u8>, i16)> = txs
                                    .iter()
                                    .flat_map(|tx| {
                                        tx.inputs.iter().map(|inp| {
                                            (hex::decode(&inp.tx_hash).unwrap_or_default(), inp.index)
                                        })
                                    })
                                    .collect();
                                if !input_keys.is_empty() {
                                    let resolved = {
                                        let guard = chain_state_clone.read().await;
                                        guard.resolve_utxos_batch(&input_keys).await
                                    };
                                    for tx in &mut txs {
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
                                let event = crate::event::Event::Block {
                                    slot,
                                    hash: hash_hex.clone(),
                                    number,
                                    timestamp: slot_to_timestamp(slot, &genesis),
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
                } else {
                    warn!("N2N connect to {} failed", n2n_addr);
                }
            }

            // EventBus snapshot events, deduped against history
            let filtered_snapshot: Vec<crate::event::Event> = snapshot
                .into_iter()
                .filter_map(|e| replay_filter.filter_event(&e, &replay_delegators))
                .filter(|e| match e {
                    crate::event::Event::Block { slot, .. } => !history_slots.contains(slot),
                    _ => true,
                })
                .collect();
            for event in filtered_snapshot {
                if let Some(sse) = serialize_event(event) {
                    let _ = sender.send(sse).await;
                }
            }
        } else {
            // Non-pool feeds: just send filtered snapshot
            let filtered_snapshot: Vec<crate::event::Event> = snapshot
                .into_iter()
                .filter_map(|e| replay_filter.filter_event(&e, &replay_delegators))
                .collect();
            for event in filtered_snapshot {
                if let Some(sse) = serialize_event(event) {
                    let _ = sender.send(sse).await;
                }
            }
        }
        // sender dropped here → receiver stream ends → chains to live
    });

    let replay = tokio_stream::wrappers::ReceiverStream::new(receiver);

    let chain_state = state.chain_state.clone();

    // For pool feeds, track the last-seen pool, live stake, and blocks_minted to detect changes.
    let (last_pool, last_live_stake) = if let filter::FeedFilter::Pool(ref hash) = filter {
        let guard = state.chain_state.read().await;
        let snap = guard.current();
        let pool = snap.and_then(|s| s.pools.get(&hex::encode(hash)).cloned());
        let live_stake = snap.and_then(|s| State::pool_live_stake(s, hash));
        (pool, live_stake)
    } else {
        (None, None)
    };

    let live = futures::stream::unfold(
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
                // Drain buffered events first (pool update events).
                if let Some(sse) = buf.pop_front() {
                    return Some((sse, (rx, filter, chain_state, last_pool, last_live_stake, buf)));
                }

                let event = rx.next().await?.ok()?;

                // After Block or Rollback, check if pool params or live stake changed.
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
    );

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
) {
    let state = AppState {
        bus,
        chain_state,
        nftcdn,
        genesis,
        n2n_addr,
        magic,
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
