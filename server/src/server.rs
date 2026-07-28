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
use crate::filter::FeedFilter;
use crate::model::{asset_fingerprint, drep_bech32_id, pool_bech32_id, DRep, Pool, TxOutput};
use crate::nftcdn::{rung_for_dpr, NftcdnConfig, SIZE_LADDER};
use crate::og;

mod assets;
mod subject;
use subject::*;
mod decode;
use decode::*;
mod replay;
use replay::*;
mod cards;
mod search;
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
    /// Per-epoch cache of the homepage Cardano event's slow-changing fields (reserves,
    /// active pool/drep counts). All settle at epoch boundaries, so we refetch only
    /// when the snapshot's epoch advances.
    cardano_cache: Arc<tokio::sync::Mutex<Option<CardanoCache>>>,
}

/// Slow-changing homepage stats, refreshed once per epoch (see `cardano_stats_json`).
#[derive(Clone, Copy)]
struct CardanoCache {
    epoch: u64,
    /// Not-yet-minted ADA; circulating supply (displayed) = max supply − reserves.
    reserves: i64,
    /// Delegatable ADA (`utxo + rewards + fees`) — the % staked denominator.
    stakeable: i64,
    drep_count: i64,
}

#[derive(Clone, Copy, serde::Serialize)]
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

/// First slot of a (Shelley) epoch — the epoch-change boundary. Inverse of
/// `State::epoch_for_slot`; used to position per-epoch reward items on the feed.
fn slot_for_epoch(epoch: u64, genesis: &GenesisConfig) -> u64 {
    let shelley_start_epoch = genesis.shelley_known_slot * genesis.byron_slot_length as u64
        / genesis.byron_epoch_length as u64;
    genesis.shelley_known_slot
        + epoch.saturating_sub(shelley_start_epoch) * genesis.shelley_epoch_length as u64
}

/// Maximum number of blocks to replay on feed connection. Must match
/// `MAX_BLOCKS` in `web/src/lib/components/Feed.svelte`.
const MAX_REPLAY_BLOCKS: usize = 30;

/// Minimum stake change (as fraction of live stake) to include a block in feed
/// replay. Must match `STAKE_CHANGE_PRUNE_DIVISOR` in Feed.svelte.

/// Recent blocks to replay on a stake-address feed connection (fetched from
/// db-sync, since stake addresses are not pre-indexed in memory).
const STAKE_REPLAY_BLOCKS: i64 = 30;

/// Cardano max supply in lovelace (45 G ADA). Circulating supply = this − reserves.
const MAX_LOVELACE_SUPPLY: i64 = 45_000_000_000_000_000;

/// Max pool/DRep results returned by `/api/search`.
const SEARCH_LIMIT: usize = 12;
/// Shortest query that triggers a pool/DRep search.
const SEARCH_MIN_QUERY_LEN: usize = 2;
/// Jaro-Winkler similarity below which a non-substring candidate is dropped.
const SEARCH_FUZZY_THRESHOLD: f32 = 0.7;
/// Hex length of a 28-byte blake2b-224 hash — the format shared by a raw pool hash
/// and a minting policy id (so a bare hex of this length is ambiguous between them).
const POOL_HASH_HEX_LEN: usize = 56;

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

/// Build the homepage `Cardano` event JSON: total ADA in circulation, active pool/drep
/// counts, and % of ADA staked. `total_staked` is read from the snapshot in O(1)
/// (updated per block); reserves and the active counts come from a per-epoch cache
/// (one db query each per epoch — they settle at epoch boundaries — run off-lock).
/// Returns the JSON string (so callers can dedup live updates) or `None` before the
/// first snapshot exists.
/// Homepage "CARDANO" header figures — shared by the SSE `Cardano` event and the home card.
struct CardanoStats {
    /// ADA in circulation (lovelace) = max supply − reserves.
    circulation: i64,
    pool_count: usize,
    drep_count: i64,
    staked_percent: f64,
}

async fn cardano_stats(state: &AppState) -> Option<CardanoStats> {
    let (total_staked, epoch, pool_count, db) = {
        let guard = state.chain_state.read().await;
        let snap = guard.current()?;
        let epoch = snap.last_epoch.unwrap_or(0);
        // Active pools = registered, not (yet) retired (per-block `retiring_epoch`), and
        // with live stake > 0. Since every stake/reward term is non-negative, stake > 0
        // ⟺ some delegator has a positive balance — so `any(..)` short-circuits and the
        // scan stays ~O(pools).
        let pool_count = snap
            .pools
            .values()
            .filter(|p| p.retiring_epoch.is_none_or(|e| (e as u64) > epoch))
            .filter(|p| {
                snap.pool_delegators.get(&p.hash_raw).is_some_and(|creds| {
                    creds.iter().any(|c| {
                        snap.stakes.get(c).copied().unwrap_or(0)
                            + snap.rewards.get(c).copied().unwrap_or(0)
                            > 0
                    })
                })
            })
            .count();
        (snap.total_staked, epoch, pool_count, guard.db_handle())
    };

    // Slow-changing fields refreshed once per epoch (they settle at epoch boundaries).
    // The mutex serializes the refresh so concurrent homepage clients at an epoch
    // boundary issue the queries once.
    let cached = {
        let mut cache = state.cardano_cache.lock().await;
        match *cache {
            Some(c) if c.epoch == epoch => c,
            _ => {
                let c = match &db {
                    Some(db) => {
                        let (reserves, stakeable) =
                            db.reserves_and_stakeable().await.unwrap_or((0, 0));
                        CardanoCache {
                            epoch,
                            reserves,
                            stakeable,
                            drep_count: db.active_drep_count(epoch as i64).await.unwrap_or(0),
                        }
                    }
                    None => CardanoCache {
                        epoch,
                        reserves: 0,
                        stakeable: 0,
                        drep_count: 0,
                    },
                };
                *cache = Some(c);
                c
            }
        }
    };

    // Displayed "ADA in circulation" = max supply − reserves. The % staked denominator
    // is the delegatable supply (utxo + rewards + fees) — everything except the locked
    // protocol pots (reserves, treasury, deposits) — so the ratio → 100% when all
    // stakeable ADA is delegated.
    let circulation = MAX_LOVELACE_SUPPLY - cached.reserves;
    let staked_percent = if cached.stakeable > 0 {
        total_staked as f64 / cached.stakeable as f64 * 100.0
    } else {
        0.0
    };
    Some(CardanoStats {
        circulation,
        pool_count,
        drep_count: cached.drep_count,
        staked_percent,
    })
}

/// The homepage `Cardano` SSE event JSON.
async fn cardano_stats_json(state: &AppState) -> Option<String> {
    let s = cardano_stats(state).await?;
    Some(format!(
        r#"{{"type":"Cardano","circulation":"{}","pool_count":{},"drep_count":{},"staked_percent":{:.1}}}"#,
        s.circulation, s.pool_count, s.drep_count, s.staked_percent
    ))
}

/// Build a `Stake` info event for a stake feed: ADA balance, available rewards,
/// current pool/drep delegation (all snapshot-live), plus a connect-time
/// `assets_count` the caller passes through unchanged on every emit.
/// A stake credential's pool + DRep delegation for the header events, shared by the stake
/// feed and the (delegating) address feed. Returns `(pool_id bech32, pool_ticker, drep_id
/// bech32, drep_name)`, each `None` when not delegated.
fn pool_drep_info(
    snap: Option<&BlockSnapshot>,
    cred: &[u8],
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
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
    (pool_id, pool_ticker, drep_id, drep_name)
}

/// Decode a payment-address bech32 string to its raw bytes — the key used by
/// `BlockSnapshot.address_balances`. None on parse failure (e.g. byron-style).
fn address_bytes(address: &str) -> Option<Vec<u8>> {
    pallas::ledger::addresses::Address::from_bech32(address)
        .ok()
        .map(|a| a.to_vec())
}

/// Query string for SSE endpoints. `dpr` is the client's
/// `window.devicePixelRatio`, used to negotiate the thumbnail image size.
#[derive(serde::Deserialize)]
struct SseQuery {
    dpr: Option<f64>,
}

/// The `AssetDelta` SSE message: a block's watched-asset changes for this connection's
/// subject. `added` carries ready-to-render tiles (same shape as the grid's HTTP
/// page); `removed` carries fingerprints. `slot` lets the client revert on rollback.
/// A removed tile: its `fingerprint` (which tile to drop) and `policy` (which group to
/// decrement on the owned-assets grid — a fingerprint can't be mapped back to a policy).
#[derive(serde::Serialize)]
struct AssetRef {
    policy: String,
    fingerprint: String,
}

#[derive(serde::Serialize)]
struct AssetDeltaWire<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    slot: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    added: Vec<AssetItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    removed: Vec<AssetRef>,
}

/// Build this connection's `AssetDelta` SSE from the tile changes it derived by diffing
/// its subject's holdings between two snapshots (see `state::address_tile_diff` /
/// `state::stake_tile_diff`). `added` carries ready-to-render tiles, `removed` their
/// fingerprints. `None` when nothing changed. The CIP-14 fingerprint is derived from
/// each token's policy+name. On rollback this is just the corrective diff — no special
/// case (the client applies adds/removes the same way).
fn asset_delta_event(
    added: Vec<(Vec<u8>, Vec<u8>, u128)>,
    removed: Vec<crate::state::Token>,
    slot: u64,
    nftcdn: &NftcdnConfig,
    decimals: &imbl::HashMap<String, u8>,
) -> Option<Result<SseEvent, Infallible>> {
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    let added: Vec<AssetItem> = added
        .into_iter()
        .map(|(policy, name, qty)| {
            let policy_hex = hex::encode(&policy);
            build_owned_tile(nftcdn, &policy_hex, &policy, name, qty, decimals)
        })
        .collect();
    let removed: Vec<AssetRef> = removed
        .into_iter()
        .map(|(policy, name)| AssetRef {
            fingerprint: crate::model::asset_fingerprint(&policy, &name),
            policy: hex::encode(&policy),
        })
        .collect();
    let json = serde_json::to_string(&AssetDeltaWire {
        kind: "AssetDelta",
        slot,
        added,
        removed,
    })
    .ok()?;
    Some(Ok(SseEvent::default().data(json)))
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

#[allow(clippy::too_many_arguments)]
fn build_live_stream(
    rx: tokio::sync::broadcast::Receiver<crate::event::Event>,
    filter: filter::FeedFilter,
    chain_state: Arc<RwLock<State>>,
    initial: LiveState,
    size: u16,
    nftcdn: NftcdnConfig,
    mainnet: bool,
    // Genesis params for `slot_for_epoch` when re-emitting a pool's current-epoch block count
    // (captured by the `move` closure below; `GenesisConfig` is `Copy`).
    genesis: GenesisConfig,
    // True for the assets-page endpoint: this connection also emits live grid tile
    // deltas (derived by diffing its subject's holdings between snapshots).
    wants_tiles: bool,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    futures::stream::unfold(
        (
            BroadcastStream::new(rx),
            filter,
            chain_state,
            initial,
            std::collections::VecDeque::<Result<SseEvent, Infallible>>::new(),
            size,
            nftcdn,
            mainnet,
            None::<crate::state::AssetHoldings>,
            wants_tiles,
        ),
        move |(
            mut rx,
            filter,
            chain_state,
            mut live,
            mut buf,
            size,
            nftcdn,
            mainnet,
            mut prev_holdings,
            wants_tiles,
        )| async move {
            loop {
                if let Some(sse) = buf.pop_front() {
                    return Some((
                        sse,
                        (
                            rx,
                            filter,
                            chain_state,
                            live,
                            buf,
                            size,
                            nftcdn,
                            mainnet,
                            prev_holdings,
                            wants_tiles,
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
                            let (current_pool, current_balance, pool_event) = {
                                let guard = chain_state.read().await;
                                let snap = guard.current();
                                let pool =
                                    snap.and_then(|s| s.pools.get(&hex::encode(hash)).cloned());
                                let live_stake = snap.and_then(|s| State::pool_live_stake(s, hash));
                                let epoch = snap.and_then(|s| s.last_epoch).unwrap_or(0);
                                let epoch_blocks =
                                    pool_epoch_blocks(&guard.feed_index, hash, epoch, &genesis);
                                let event = pool
                                    .as_ref()
                                    .map(|p| pool_sse_event(p, snap, epoch, epoch_blocks));
                                (pool, live_stake, event)
                            };
                            // `current_pool != live.pool` also catches a block count bump
                            // (Pool::blocks is part of the struct), re-emitting on a mint.
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
                            let (current_balance, current_count) = {
                                let guard = chain_state.read().await;
                                let snap = guard.current();
                                let bal = snap.and_then(|s| s.stakes.get(cred).copied());
                                // Distinct-asset count straight from the live global
                                // holdings map (a prefix-union over the credential's
                                // addresses) — always current, no tracking needed.
                                let cnt = snap.map(|s| s.stake_asset_count(cred));
                                (bal, cnt)
                            };
                            if current_balance != live.balance || current_count != live.assets_count
                            {
                                let stake_address = filter.feed_id();
                                let guard = chain_state.read().await;
                                buf.push_back(stake_sse_event(
                                    &stake_address,
                                    cred,
                                    guard.current(),
                                    current_count.unwrap_or(0),
                                ));
                                live.balance = current_balance;
                                live.assets_count = current_count;
                            }
                        }
                        filter::FeedFilter::Address(ref addr) => {
                            if let Some(addr_b) = address_bytes(addr) {
                                let (current_balance, current_count) = {
                                    let guard = chain_state.read().await;
                                    let snap = guard.current();
                                    let bal =
                                        snap.and_then(|s| s.address_balances.get(&addr_b).copied());
                                    let cnt = snap.map(|s| s.address_asset_count(&addr_b));
                                    (bal, cnt)
                                };
                                if current_balance != live.balance
                                    || current_count != live.assets_count
                                {
                                    let guard = chain_state.read().await;
                                    buf.push_back(address_sse_event(
                                        addr,
                                        &addr_b,
                                        guard.current(),
                                        mainnet,
                                        current_count.unwrap_or(0),
                                    ));
                                    live.balance = current_balance;
                                    live.assets_count = current_count;
                                }
                            }
                        }
                    }

                    // Live grid tile deltas for an open assets page: diff this subject's
                    // holdings against the previous snapshot. A rollback is just the
                    // corrective diff (curr = the reverted snapshot), so no special case.
                    if wants_tiles {
                        let slot = match &event {
                            crate::event::Event::Block { slot, .. }
                            | crate::event::Event::Rollback { slot } => *slot,
                            _ => 0,
                        };
                        let curr_dec = {
                            let guard = chain_state.read().await;
                            guard
                                .current()
                                .map(|s| (s.asset_holdings.clone(), s.decimals.clone()))
                        };
                        if let Some((curr, decimals)) = curr_dec {
                            if let Some(prev) = &prev_holdings {
                                let (added, removed) = match &filter {
                                    filter::FeedFilter::Address(addr) => {
                                        match address_bytes(addr) {
                                            Some(addr_b) => {
                                                let cred =
                                                crate::pallas::stake_credential_from_address_bytes(
                                                    &addr_b,
                                                );
                                                crate::state::address_tile_diff(
                                                    prev,
                                                    &curr,
                                                    &(cred, addr_b),
                                                )
                                            }
                                            None => (Vec::new(), Vec::new()),
                                        }
                                    }
                                    filter::FeedFilter::Stake(payload) => {
                                        crate::state::stake_tile_diff(prev, &curr, &payload[1..])
                                    }
                                    _ => (Vec::new(), Vec::new()),
                                };
                                // Resolve each added tile's current owned quantity from the
                                // snapshot (the diff only carries which (policy, name) changed).
                                let added: Vec<(Vec<u8>, Vec<u8>, u128)> = added
                                    .into_iter()
                                    .map(|(policy, name)| {
                                        let qty = match &filter {
                                            filter::FeedFilter::Address(addr) => address_bytes(addr)
                                                .map(|ab| {
                                                    let cred = crate::pallas::stake_credential_from_address_bytes(&ab);
                                                    crate::state::address_token_qty(&curr, &(cred, ab), &policy, &name)
                                                })
                                                .unwrap_or(0),
                                            filter::FeedFilter::Stake(payload) => {
                                                crate::state::stake_token_qty(&curr, &payload[1..], &policy, &name)
                                            }
                                            _ => 0,
                                        };
                                        (policy, name, qty)
                                    })
                                    .collect();
                                if let Some(sse) =
                                    asset_delta_event(added, removed, slot, &nftcdn, &decimals)
                                {
                                    buf.push_back(sse);
                                }
                            }
                            prev_holdings = Some(curr);
                        }
                    }
                }

                // Filter under the read lock: `filter_event` is synchronous (no await), so
                // holding the guard is brief, and it needs the snapshot to resolve delegator
                // ADA Handles for the folded stake-address summary.
                let filtered = {
                    let guard = chain_state.read().await;
                    let snap = guard.current();
                    let delegators = snap
                        .map(|s| filter.current_delegators(s))
                        .unwrap_or_default();
                    filter.filter_event(&event, &delegators, mainnet, snap)
                };
                if let Some(sse) = filtered.and_then(|e| serialize_event(e, size)) {
                    return Some((
                        sse,
                        (
                            rx,
                            filter,
                            chain_state,
                            live,
                            buf,
                            size,
                            nftcdn,
                            mainnet,
                            prev_holdings,
                            wants_tiles,
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

    let cardano_json = cardano_stats_json(&state).await;
    let cardano = cardano_json
        .clone()
        .map(|json| Ok(SseEvent::default().data(json)));

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
    let replay = futures::stream::iter(config.into_iter().chain(cardano).chain(init));

    // Live: forward each broadcast event, and after every Block recompute the network
    // stats — re-emitting a `Cardano` event only when the JSON changed (i.e. total ADA,
    // % staked, pool or drep count moved), so an open homepage updates live. `buf`
    // lets one input yield two outputs (the block + the stats); `last` dedups.
    let live = futures::stream::unfold(
        (
            BroadcastStream::new(rx),
            state.clone(),
            cardano_json,
            std::collections::VecDeque::<Result<SseEvent, Infallible>>::new(),
        ),
        move |(mut rx, state, mut last, mut buf)| async move {
            loop {
                if let Some(item) = buf.pop_front() {
                    return Some((item, (rx, state, last, buf)));
                }
                let event = match rx.next().await {
                    Some(Ok(event)) => event,
                    Some(Err(_)) => continue,
                    None => return None,
                };
                let is_block = matches!(event, crate::event::Event::Block { .. });
                if let Some(sse) = serialize_event(event, size) {
                    buf.push_back(sse);
                }
                if is_block {
                    if let Some(json) = cardano_stats_json(&state).await {
                        if last.as_deref() != Some(json.as_str()) {
                            last = Some(json.clone());
                            buf.push_back(Ok(SseEvent::default().data(json)));
                        }
                    }
                }
            }
        },
    );
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

    // Connect-time assets_count (Stake/Address feeds only; 0 otherwise), queried once
    // off the chain_state lock and shared by both the replay header send below and the
    // live-stream seed (`initial_live`) — the live stream re-emits the header on every
    // balance change but always passes this connect-time count through unchanged.
    let assets_count = subject_assets_count(&filter, &state.chain_state).await;

    // Spawn replay task: config → pool info → feed index replay → snapshot
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(32);
    let replay_filter = filter.clone();
    let replay_delegators = delegators.clone();
    let replay_state = state.clone();

    tokio::spawn(async move {
        // If the client already disconnected before we start, skip the whole replay (its
        // db-fill queries + N2N block fetches) — a burst of short-lived connections (bots
        // reconnecting every few seconds) must not each do the full replay work.
        if sender
            .send(config_event(
                replay_state.nftcdn.subdomain,
                &replay_state.genesis,
                replay_state.magic,
            ))
            .await
            .is_err()
        {
            return;
        }

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
            send_pool_info(
                &sender,
                &replay_state.chain_state,
                ph,
                &replay_state.genesis,
            )
            .await;

            // Read feed index data, pool live stake, and resolve delegation labels
            let (minted, stake_changes, mut deleg_info, deleg_slots, pool_votes, stake_threshold) = {
                let guard = replay_state.chain_state.read().await;
                let snap = guard.current();
                // Significance threshold from the epoch-stable active stake (matches the sink's
                // index-time filter), not the O(delegators) live stake. Live stake is still shown
                // in the feed header — computed separately in the live-stream event builder.
                let threshold = guard.pool_stake_threshold(ph);

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
                    guard.feed_index.pool_vote_blocks(ph).to_vec(),
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
            // SPO governance votes (rendered via `matches_vote`, like DRep votes).
            for r in &pool_votes {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::StakeChange(r.clone()));
            }

            // Empty-feed fill: a pool dormant for >5 days has little or nothing in the
            // feed index, leaving a blank feed. Top up to MAX_REPLAY_BLOCKS from db-sync
            // (most-recent minted blocks + gained delegations) via the sub-ms indexed
            // top-K queries. Runs off the chain_state lock; delegation `from`/exact
            // live_stake aren't reconstructed (the accepted trade-off) — `to` is this
            // pool and live_stake is the delegator's current snapshot stake.
            //
            // Trigger on *guaranteed-render* content (minted blocks + delegations), NOT
            // slot_map.len(): a pool with many delegators has lots of `pool_stake_change`
            // candidate blocks in the window, but almost all are sub-threshold and get
            // filtered out at send time — so slot_map.len() ≥ 30 while the feed still
            // renders ~empty. The newest-first send places any fill blocks after the
            // (few) rendering candidates, so over-triggering just wastes a sub-ms query.
            if minted.len() + deleg_slots.len() + pool_votes.len() < MAX_REPLAY_BLOCKS {
                if let Some(dbh) = { replay_state.chain_state.read().await.db_handle() } {
                    let limit = MAX_REPLAY_BLOCKS as i64;
                    for b in dbh
                        .pool_recent_blocks(ph, i64::MAX, limit)
                        .await
                        .unwrap_or_default()
                    {
                        slot_map
                            .entry(b.slot)
                            .or_insert(SlotAction::PoolMinted(BlockRef {
                                slot: b.slot,
                                hash: b.block_hash,
                                number: b.block_no,
                            }));
                    }
                    // SPO governance votes (rendered via `matches_vote`).
                    for b in dbh
                        .pool_recent_votes(ph, i64::MAX, limit)
                        .await
                        .unwrap_or_default()
                    {
                        slot_map
                            .entry(b.slot)
                            .or_insert(SlotAction::StakeChange(BlockRef {
                                slot: b.slot,
                                hash: b.block_hash,
                                number: b.block_no,
                            }));
                    }
                    let deleg_fill = dbh
                        .pool_recent_delegations(ph, i64::MAX, limit)
                        .await
                        .unwrap_or_default();
                    if !deleg_fill.is_empty() {
                        let guard = replay_state.chain_state.read().await;
                        let snap = guard.current();
                        let to_pool_id = pool_bech32_id(ph);
                        let to_ticker = snap
                            .and_then(|s| s.pools.get(&hex::encode(ph)))
                            .and_then(|p| p.ticker.clone());
                        for f in &deleg_fill {
                            // A feed-index overlay for this tx is richer (real from/live_stake) — keep it.
                            if deleg_info.contains_key(&f.tx_hash) {
                                continue;
                            }
                            let live_stake = snap
                                .map(|s| {
                                    s.stakes.get(&f.cred).copied().unwrap_or(0)
                                        + s.rewards.get(&f.cred).copied().unwrap_or(0)
                                })
                                .unwrap_or(0);
                            deleg_info
                                .entry(f.tx_hash.clone())
                                .or_default()
                                .push(DelegationInfo {
                                    stake_address: crate::pallas::stake_address_from_cred_bytes(
                                        &f.cred,
                                        replay_state.mainnet,
                                    ),
                                    from_pool_id: None,
                                    from_ticker: None,
                                    to_pool_id: Some(to_pool_id.clone()),
                                    to_ticker: to_ticker.clone(),
                                    from_drep_id: None,
                                    from_drep_name: None,
                                    to_drep_id: None,
                                    to_drep_name: None,
                                    live_stake,
                                });
                            slot_map
                                .entry(f.slot)
                                .or_insert(SlotAction::StakeChange(BlockRef {
                                    slot: f.slot,
                                    hash: f.block_hash.clone(),
                                    number: f.block_no,
                                }));
                        }
                    }
                }
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

            // Enable infinite scroll: pagination pages older history from the tip by
            // keyset id and dedups this replay's overlap (see `older_pool_drep`).
            if let Some(e) = serialize_event(
                crate::event::Event::ReplayCursor {
                    slot: None,
                    epoch: None,
                    stake: None,
                },
                sse.size,
            ) {
                let _ = sse.sender.send(e).await;
            }

            exclude_slots
        } else if let Some(ref db) = replay_filter.drep_bytes().cloned() {
            send_drep_info(&sender, &replay_state.chain_state, db).await;

            // Read DRep feed index data and resolve delegation labels
            let (stake_changes, mut deleg_info, deleg_slots, drep_votes, stake_threshold) = {
                let guard = replay_state.chain_state.read().await;
                let snap = guard.current();
                // Threshold from epoch-stable active stake; live stake stays in the header.
                let threshold = guard.drep_stake_threshold(db);

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
                    guard.feed_index.drep_vote_blocks(db).to_vec(),
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
            // Governance votes cast by this DRep (rendered via `matches_vote`).
            for r in &drep_votes {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::StakeChange(r.clone()));
            }

            // Empty-feed fill for a dormant DRep: DReps mint no blocks, so top up from
            // db-sync with the DRep's most-recent governance votes (block refs — the vote
            // tx is retained via `matches_vote` and decoded from the fetched block) and
            // gained delegations. Sub-ms indexed top-K, off the chain_state lock.
            //
            // Trigger on delegations only (guaranteed to render), NOT slot_map.len():
            // `drep_stake_change` fills the window with sub-threshold candidate blocks
            // that get filtered at send time, same as the pool case.
            if deleg_slots.len() + drep_votes.len() < MAX_REPLAY_BLOCKS {
                if let Some(dbh) = { replay_state.chain_state.read().await.db_handle() } {
                    let limit = MAX_REPLAY_BLOCKS as i64;
                    for b in dbh
                        .drep_recent_votes(db, i64::MAX, limit)
                        .await
                        .unwrap_or_default()
                    {
                        slot_map
                            .entry(b.slot)
                            .or_insert(SlotAction::StakeChange(BlockRef {
                                slot: b.slot,
                                hash: b.block_hash,
                                number: b.block_no,
                            }));
                    }
                    let deleg_fill = dbh
                        .drep_recent_delegations(db, i64::MAX, limit)
                        .await
                        .unwrap_or_default();
                    if !deleg_fill.is_empty() {
                        let guard = replay_state.chain_state.read().await;
                        let snap = guard.current();
                        let to_drep_id = drep_bech32_id(db);
                        let to_drep_name = match db.first() {
                            Some(0x02) => Some("Always Abstain".to_string()),
                            Some(0x03) => Some("Always No Confidence".to_string()),
                            _ => snap
                                .and_then(|s| s.dreps.get(db.as_slice()))
                                .and_then(|d| d.given_name.clone()),
                        };
                        for f in &deleg_fill {
                            if deleg_info.contains_key(&f.tx_hash) {
                                continue;
                            }
                            let live_stake = snap
                                .map(|s| {
                                    s.stakes.get(&f.cred).copied().unwrap_or(0)
                                        + s.rewards.get(&f.cred).copied().unwrap_or(0)
                                })
                                .unwrap_or(0);
                            deleg_info
                                .entry(f.tx_hash.clone())
                                .or_default()
                                .push(DelegationInfo {
                                    stake_address: crate::pallas::stake_address_from_cred_bytes(
                                        &f.cred,
                                        replay_state.mainnet,
                                    ),
                                    from_pool_id: None,
                                    from_ticker: None,
                                    to_pool_id: None,
                                    to_ticker: None,
                                    from_drep_id: None,
                                    from_drep_name: None,
                                    to_drep_id: Some(to_drep_id.clone()),
                                    to_drep_name: to_drep_name.clone(),
                                    live_stake,
                                });
                            slot_map
                                .entry(f.slot)
                                .or_insert(SlotAction::StakeChange(BlockRef {
                                    slot: f.slot,
                                    hash: f.block_hash.clone(),
                                    number: f.block_no,
                                }));
                        }
                    }
                }
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

            // Enable infinite scroll (see the pool branch / `older_pool_drep`).
            if let Some(e) = serialize_event(
                crate::event::Event::ReplayCursor {
                    slot: None,
                    epoch: None,
                    stake: None,
                },
                sse.size,
            ) {
                let _ = sse.sender.send(e).await;
            }

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
            // assets_count was queried once before the spawn and captured here.
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

            // Per-epoch reward capsules at their epoch-change slot/timestamp.
            if let Some(sr) = &subject {
                for (epoch, rows) in &sr.reward_capsules {
                    let slot = slot_for_epoch(*epoch, &replay_state.genesis);
                    let ev = crate::event::Event::Reward {
                        epoch: *epoch,
                        slot,
                        timestamp: slot_to_timestamp(slot, &replay_state.genesis),
                        rows: rows.clone(),
                    };
                    if let Some(e) = serialize_event(ev, sse.size) {
                        let _ = sse.sender.send(e).await;
                    }
                }
            }

            // Seed the client's pagination cursor (only if the page was full — else
            // there's nothing older). Stake feeds carry the walk anchor.
            if full_page {
                if let Some(sr) = &subject {
                    let (slot, epoch, stake) = sr.cursor();
                    let ev = crate::event::Event::ReplayCursor {
                        slot: Some(slot),
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
            // assets_count was queried once before the spawn and captured here.
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
                    slot: Some(slot),
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
            replay_state.mainnet,
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

    // For stake/address feeds, seed the live state with the connect-time
    // assets_count queried once above (shared with the replay header send). The live
    // stream re-emits the info event on balance changes, always passing this count
    // through unchanged.
    let initial_live = if matches!(
        filter,
        filter::FeedFilter::Stake(_) | filter::FeedFilter::Address(_)
    ) {
        LiveState {
            assets_count: Some(assets_count),
            ..initial_live
        }
    } else {
        initial_live
    };

    let replay = tokio_stream::wrappers::ReceiverStream::new(receiver);
    let live = build_live_stream(
        rx,
        filter,
        chain_state,
        initial_live,
        size,
        state.nftcdn.clone(),
        state.mainnet,
        state.genesis,
        false, // regular feed: header/count only, no grid tiles
    );
    let stream = replay.chain(live);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// SSE feed backing the owned-assets page (`/<bech32>/assets`). A lean variant of
/// `filtered_events`: it sends only `Config` + the Stake/Address header (with
/// `assets_count`) and then keeps the connection open via the shared live stream,
/// which re-emits the header on every balance change (and is where future live asset
/// add/remove deltas will arrive). It deliberately skips the tx replay history and
/// snapshot — the assets grid loads its tiles over HTTP (`/api/assets`). Only Stake
/// and Address feeds have an assets page; pool/drep ids return 400.
async fn asset_feed_events(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(feed_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<SseQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, StatusCode> {
    info!("/events/{feed_id}/assets");
    let size = rung_for_dpr(query.dpr.unwrap_or(1.0));
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;
    if !matches!(
        filter,
        filter::FeedFilter::Stake(_) | filter::FeedFilter::Address(_)
    ) {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Subscribe for live events; the snapshot (tx history) is intentionally dropped.
    let (_snapshot, rx) = state.bus.subscribe().await;

    // Header (config + Stake/Address info) built synchronously: the `assets_count` is an
    // in-memory read of `asset_holdings` (O(1) for an address, a ms-scale union for a
    // stake), so there's no slow seed to defer to a background task — config + header go
    // straight into the replay stream.
    let assets_count = subject_assets_count(&filter, &state.chain_state).await;
    let config = config_event(state.nftcdn.subdomain, &state.genesis, state.magic);
    let header = {
        let guard = state.chain_state.read().await;
        let snap = guard.current();
        match &filter {
            filter::FeedFilter::Stake(payload) => {
                stake_sse_event(&filter.feed_id(), &payload[1..], snap, assets_count)
            }
            filter::FeedFilter::Address(addr) => {
                let addr_bytes = address_bytes(addr).unwrap_or_default();
                address_sse_event(addr, &addr_bytes, snap, state.mainnet, assets_count)
            }
            // Guarded above: only Stake / Address feeds reach here.
            _ => unreachable!(),
        }
    };

    // No registration needed: this connection derives its own live grid tile deltas in
    // `build_live_stream` (wants_tiles=true) by diffing its subject's holdings between
    // snapshots — count and tiles both come from the always-current `asset_holdings` map.

    // Seed the live stream so the header re-emits on balance/asset changes.
    let initial_live = {
        let guard = state.chain_state.read().await;
        let snap = guard.current();
        let balance = match &filter {
            filter::FeedFilter::Stake(payload) => {
                snap.and_then(|s| s.stakes.get(&payload[1..]).copied())
            }
            filter::FeedFilter::Address(addr) => {
                let addr_b = address_bytes(addr).unwrap_or_default();
                snap.and_then(|s| s.address_balances.get(&addr_b).copied())
            }
            _ => None,
        };
        LiveState {
            pool: None,
            balance,
            assets_count: Some(assets_count),
        }
    };

    let replay = futures::stream::iter([config, header]);
    let live = build_live_stream(
        rx,
        filter,
        state.chain_state.clone(),
        initial_live,
        size,
        state.nftcdn.clone(),
        state.mainnet,
        state.genesis,
        true, // assets page: emit live grid tile deltas via per-block holdings diff
    );
    let stream = replay.chain(live);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
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

// ---------------------------------------------------------------------------------------------
/// True for a syntactically valid Cardano policy id: exactly 56 lowercase hex
/// chars (28 bytes). Like `is_valid_fingerprint`, this rejects garbage before it
/// reaches the DB; the policy id itself never enters an NFTCDN host (only the
/// DB-sourced fingerprints do).
fn is_valid_policy_id(p: &str) -> bool {
    p.len() == 56
        && p.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(serde::Serialize)]
struct AssetItem {
    fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Policy id (hex) — lets the owned-assets grid group/route tiles by policy.
    policy: String,
    /// Owned quantity, decimals-formatted, present only when it isn't 1 (owned-assets
    /// tiles only; absent on the policy-browse grid).
    #[serde(skip_serializing_if = "Option::is_none")]
    quantity: Option<String>,
    src: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    srcset: String,
}

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

fn row_to_asset(
    nftcdn: &NftcdnConfig,
    policy: &str,
    fingerprint: String,
    name_bytes: Vec<u8>,
) -> AssetItem {
    let name = decode_asset_name(&name_bytes);
    let (src, srcset) = build_thumb_urls(nftcdn, &fingerprint);
    AssetItem {
        fingerprint,
        name,
        policy: policy.to_string(),
        quantity: None,
        src,
        srcset,
    }
}

/// The owned quantity formatted with the asset's decimals, or `None` when it's exactly 1
/// (NFTs / single units — not worth showing).
fn fmt_owned_qty(qty: u128, decimals: u8) -> Option<String> {
    let s = crate::event::format_quantity(qty, decimals);
    (s != "1").then_some(s)
}

/// Build an owned-assets tile: signed thumbnail URLs plus the decimals-formatted owned
/// quantity (shown only when it isn't 1).
fn build_owned_tile(
    nftcdn: &NftcdnConfig,
    policy_hex: &str,
    policy: &[u8],
    name: Vec<u8>,
    qty: u128,
    decimals: &imbl::HashMap<String, u8>,
) -> AssetItem {
    let fingerprint = crate::model::asset_fingerprint(policy, &name);
    let dec = decimals.get(&fingerprint).copied().unwrap_or(0);
    let mut item = row_to_asset(nftcdn, policy_hex, fingerprint, name);
    item.quantity = fmt_owned_qty(qty, dec);
    item
}

/// One policy's owned tokens while grouping: `(policy, held count, up to `GROUP_SAMPLES`
/// `(name, quantity)` sample tiles, oldest mint_time in the group)`.
type PolicyGroup = (Vec<u8>, usize, Vec<(Vec<u8>, u128)>, u32);

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
        cardano_cache: Arc::new(tokio::sync::Mutex::new(None)),
    };
    let app = Router::new()
        // Any unmatched path → social-card HTML (nginx routes only crawler UAs here). All methods,
        // so a crawler's HEAD probe is answered too.
        .fallback(cards::og_page)
        .route("/events", get(events))
        .route("/events/{feed_id}/assets", get(asset_feed_events))
        .route("/events/{feed_id}", get(filtered_events))
        .route("/api/asset/{fingerprint}", get(assets::asset_media))
        .route("/api/policy/{policy_id}", get(assets::policy_assets))
        .route("/api/assets/{feed_id}", get(assets::owned_assets))
        .route(
            "/api/assets/{feed_id}/{policy}",
            get(assets::owned_assets_by_policy),
        )
        .route("/api/feed/{feed_id}/older", get(replay::older_blocks))
        .route("/api/search", get(search::search))
        .route("/api/handle/{name}", get(search::resolve_handle))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind SSE server to {addr}: {e}"));
    info!(%addr, "starting SSE server");
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| panic!("SSE server on {addr} stopped with error: {e}"));
}
