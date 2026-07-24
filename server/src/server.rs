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
const STAKE_CHANGE_DIVISOR: u64 = 1_000; // 0.1%

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

/// Exact count of blocks this pool minted in the current epoch. Read from the feed index,
/// which holds the pool's full minted-block list for the whole 5-day window (≥ one epoch, and
/// uncapped — the 30-block cap is only on what's *sent* to a client), filtered to slots at or
/// after the current epoch's start. Rollback-safe: the feed index reverts `pool_minted` on a
/// rollback (see `FeedIndex::rollback`), so this recomputes correctly.
fn pool_epoch_blocks(
    feed_index: &crate::state::FeedIndex,
    pool_hash: &[u8],
    epoch: u64,
    genesis: &GenesisConfig,
) -> u64 {
    let epoch_start = slot_for_epoch(epoch, genesis);
    feed_index
        .pool_minted_blocks(pool_hash)
        .iter()
        .filter(|b| b.slot >= epoch_start)
        .count() as u64
}

fn pool_sse_event(
    pool: &Pool,
    snap: Option<&BlockSnapshot>,
    epoch: u64,
    epoch_blocks: u64,
) -> Result<SseEvent, Infallible> {
    // A pool with no delegators has live stake 0 / 0 delegators — send 0, not absent.
    let live_stake = snap
        .and_then(|s| State::pool_live_stake(s, &pool.hash_raw))
        .unwrap_or(0);
    let delegators = snap
        .and_then(|s| s.pool_delegators.get(&pool.hash_raw))
        .map(|d| d.len())
        .unwrap_or(0);
    // `epoch_blocks` is exact for `epoch`; the frontend shows it while its epoch matches and
    // resets to 0 once the epoch rolls over (the pool has minted 0 in the new epoch until its
    // next block re-emits an exact count). `blocks` stays the lifetime total.
    Ok(SseEvent::default().data(format!(
        r#"{{"type":"Pool","pool_id":"{}","ticker":{},"pledge":"{}","margin":{},"fixed_cost":"{}","live_stake":"{}","delegators":{},"blocks":{},"epoch":{},"epoch_blocks":{}}}"#,
        pool_bech32_id(&pool.hash_raw),
        serde_json::to_string(&pool.ticker).unwrap(),
        pool.pledge,
        pool.margin,
        pool.fixed_cost,
        live_stake,
        delegators,
        pool.blocks,
        epoch,
        epoch_blocks,
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
    // A DRep with no delegators has live stake 0 / 0 delegators — send 0, not absent.
    let live_stake = snap
        .and_then(|s| State::drep_live_stake(s, drep_bytes))
        .unwrap_or(0);
    let delegators = snap
        .and_then(|s| s.drep_delegators.get(drep_bytes))
        .map(|d| d.len())
        .unwrap_or(0);
    Ok(SseEvent::default().data(format!(
        r#"{{"type":"DRep","drep_id":"{}","given_name":{},"live_stake":"{}","delegators":{}}}"#,
        drep_id,
        serde_json::to_string(&given_name).unwrap(),
        live_stake,
        delegators
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

// --- Pool ticker / DRep name search (`/api/search`) ---

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(serde::Serialize)]
struct SearchResult {
    /// What the frontend colors and links by: a bech32 pool/drep id, or — for a
    /// handle hit — the holder's payment address (so the row links to its feed).
    id: String,
    /// Raw ticker / given name, or the handle name (without the leading `$`).
    label: String,
    kind: &'static str,
    /// Delegator count (pool/drep only; absent for handles).
    #[serde(skip_serializing_if = "Option::is_none")]
    delegators: Option<usize>,
    /// Live stake in lovelace, serialized as a string (can exceed 2^53).
    /// Pool/drep only; absent for handles.
    #[serde(skip_serializing_if = "Option::is_none")]
    live_stake: Option<String>,
}

/// Score a candidate (ticker / name) against the query, case-insensitively. Higher is
/// better; `None` drops it. Tiers don't overlap: exact (4) > prefix (3–4) > substring
/// (2–3) > fuzzy Jaro-Winkler (≥ threshold). Within prefix/substring, a closer length
/// ratio wins. Pure — unit-tested.
fn search_score(query: &str, candidate: &str) -> Option<f32> {
    let q = query.trim().to_lowercase();
    let c = candidate.trim().to_lowercase();
    if q.is_empty() || c.is_empty() {
        return None;
    }
    if c == q {
        Some(4.0)
    } else if c.starts_with(&q) {
        Some(3.0 + q.len() as f32 / c.len() as f32)
    } else if c.contains(&q) {
        Some(2.0 + q.len() as f32 / c.len() as f32)
    } else {
        let sim = strsim::jaro_winkler(&q, &c) as f32;
        (sim >= SEARCH_FUZZY_THRESHOLD).then_some(sim)
    }
}

/// Search active pools by ticker and active DReps by name, ranked by string distance.
/// Retired pools (`retiring_epoch <= epoch`) and expired/deregistered DReps
/// (`active_until` absent or `< epoch`) are hidden.
async fn search(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(query): axum::extract::Query<SearchQuery>,
) -> axum::Json<Vec<SearchResult>> {
    let q = query.q.trim().to_string();
    if q.len() < SEARCH_MIN_QUERY_LEN {
        return axum::Json(vec![]);
    }
    // O(1)-clone the whole snapshot under a brief read lock, then score off-lock.
    let (snap, epoch) = {
        let guard = state.chain_state.read().await;
        let Some(snap) = guard.current() else {
            return axum::Json(vec![]);
        };
        (snap.clone(), snap.last_epoch.unwrap_or(0) as i64)
    };

    // A `$`-prefixed query searches ADA Handles by string distance instead of
    // pools/DReps. `address_by_handle` (handle name → resolved holder address, kept
    // live by the sink) is scanned off-lock with the same scorer; each hit links to
    // the holder's address feed (`id` = address).
    if let Some(hq) = q.strip_prefix('$') {
        let mut scored: Vec<(f32, &String, &String)> = Vec::new();
        for (handle, address) in snap.address_by_handle.iter() {
            if let Some(score) = search_score(hq, handle) {
                scored.push((score, handle, address));
            }
        }
        // Best score first; break ties toward the shorter (then alphabetically lower)
        // handle so results are stable and the closest match leads.
        scored.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.len().cmp(&b.1.len()))
                .then_with(|| a.1.cmp(b.1))
        });
        scored.truncate(SEARCH_LIMIT);
        let results: Vec<SearchResult> = scored
            .into_iter()
            .map(|(_, handle, address)| SearchResult {
                id: address.clone(),
                label: handle.clone(),
                kind: "handle",
                delegators: None,
                live_stake: None,
            })
            .collect();
        return axum::Json(results);
    }

    // A bare 56-hex query is an ambiguous 28-byte hash — a raw pool hash and a
    // minting policy id are indistinguishable by format. Resolve it against the live
    // pool registry (`pools` is keyed by hex hash): if it's a registered pool, return
    // it so the frontend opens the pool feed; otherwise return nothing and the
    // frontend falls back to treating the hex as a policy id (`/policy/{hex}`).
    if q.len() == POOL_HASH_HEX_LEN && q.bytes().all(|b| b.is_ascii_hexdigit()) {
        let hex = q.to_ascii_lowercase();
        let results = snap
            .pools
            .get(&hex)
            .map(|pool| {
                vec![SearchResult {
                    id: pool_bech32_id(&pool.hash_raw),
                    label: pool.ticker.clone().unwrap_or_default(),
                    kind: "pool",
                    delegators: Some(
                        snap.pool_delegators
                            .get(&pool.hash_raw)
                            .map(|d| d.len())
                            .unwrap_or(0),
                    ),
                    live_stake: Some(
                        State::pool_live_stake(&snap, &pool.hash_raw)
                            .unwrap_or(0)
                            .to_string(),
                    ),
                }]
            })
            .unwrap_or_default();
        return axum::Json(results);
    }

    // Score each active pool/drep, carrying a reference. `live_stake` is O(delegators), so
    // it's resolved only for the truncated top results below, not every match.
    enum Hit<'a> {
        Pool(&'a Pool),
        DRep(&'a DRep),
    }
    let mut scored: Vec<(f32, Hit)> = Vec::new();
    for pool in snap.pools.values() {
        if pool.retiring_epoch.is_some_and(|e| e <= epoch) {
            continue; // retired
        }
        let Some(ticker) = &pool.ticker else { continue };
        if let Some(score) = search_score(&q, ticker) {
            scored.push((score, Hit::Pool(pool)));
        }
    }
    for drep in snap.dreps.values() {
        if drep.active_until.is_none_or(|e| e < epoch) {
            continue; // expired / deregistered
        }
        let Some(name) = &drep.given_name else {
            continue;
        };
        if let Some(score) = search_score(&q, name) {
            scored.push((score, Hit::DRep(drep)));
        }
    }
    // Best score first; truncate, then resolve delegators + live stake for the survivors.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(SEARCH_LIMIT);
    let results: Vec<SearchResult> = scored
        .into_iter()
        .map(|(_, hit)| match hit {
            Hit::Pool(pool) => SearchResult {
                id: pool_bech32_id(&pool.hash_raw),
                label: pool.ticker.clone().unwrap_or_default(),
                kind: "pool",
                delegators: Some(
                    snap.pool_delegators
                        .get(&pool.hash_raw)
                        .map(|d| d.len())
                        .unwrap_or(0),
                ),
                live_stake: Some(
                    State::pool_live_stake(&snap, &pool.hash_raw)
                        .unwrap_or(0)
                        .to_string(),
                ),
            },
            Hit::DRep(drep) => SearchResult {
                id: drep_bech32_id(&drep.hash_bytes),
                label: drep.given_name.clone().unwrap_or_default(),
                kind: "drep",
                delegators: Some(
                    snap.drep_delegators
                        .get(&drep.hash_bytes)
                        .map(|d| d.len())
                        .unwrap_or(0),
                ),
                live_stake: Some(
                    State::drep_live_stake(&snap, &drep.hash_bytes)
                        .unwrap_or(0)
                        .to_string(),
                ),
            },
        })
        .collect();
    axum::Json(results)
}

#[derive(serde::Serialize)]
struct HandleAddress {
    address: String,
}

/// Resolve an exact ADA Handle name to its holder's payment address — the deterministic
/// lookup behind the `pool.pm/$handle` URL redirect (the fuzzy `$`-prefixed `/api/search`
/// stays the search-dropdown path). The stored handle name carries no `$` (it's just the
/// display sigil), so a single leading `$` is stripped if the caller included it; matching is
/// case-insensitive against `address_by_handle` (handle name → holder address, kept live by
/// the sink). `404` if no such handle — the frontend renders its Not Found page.
async fn resolve_handle(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<axum::Json<HandleAddress>, StatusCode> {
    let trimmed = name.trim();
    let name = trimmed.strip_prefix('$').unwrap_or(trimmed).to_lowercase();
    if name.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    // O(1) await-free lookup — safe to hold the read guard (doesn't block other readers,
    // and never spans an await, per the never-block-the-feeds rule).
    let guard = state.chain_state.read().await;
    let snap = guard.current().ok_or(StatusCode::NOT_FOUND)?;
    match snap.address_by_handle.get(&name) {
        Some(address) => Ok(axum::Json(HandleAddress {
            address: address.clone(),
        })),
        None => Err(StatusCode::NOT_FOUND),
    }
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
    /// Shortest ADA Handle owned across this stake credential's payment addresses, if any
    /// (snapshot-live). The stake page shows it as "$handle's stake".
    #[serde(skip_serializing_if = "Option::is_none")]
    handle: Option<String>,
    /// Distinct multi-assets held across every payment address sharing this
    /// credential. Computed once via db-sync at connect; not updated live.
    assets_count: u32,
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

fn stake_sse_event(
    stake_address: &str,
    cred: &[u8],
    snap: Option<&BlockSnapshot>,
    assets_count: u32,
) -> Result<SseEvent, Infallible> {
    let balance = snap.and_then(|s| s.stakes.get(cred).copied()).unwrap_or(0);
    let rewards = snap.and_then(|s| s.rewards.get(cred).copied()).unwrap_or(0);
    let (pool_id, pool_ticker, drep_id, drep_name) = pool_drep_info(snap, cred);
    let handle = snap.and_then(|s| s.handle_for_stake(cred));
    let json = serde_json::to_string(&StakeEvent {
        kind: "Stake",
        stake_address,
        balance: balance.to_string(),
        rewards: rewards.to_string(),
        pool_id,
        pool_ticker,
        drep_id,
        drep_name,
        handle,
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
    /// Total live stake of this address's stake credential (`balance + rewards`,
    /// across all of the credential's addresses), lovelace as a string. `None` for
    /// enterprise/pointer addresses with no stake part. Snapshot-live like `balance`.
    #[serde(skip_serializing_if = "Option::is_none")]
    stake_value: Option<String>,
    /// Distinct multi-assets across every address of this address's stake credential
    /// (the same union the stake feed shows). `None` for addresses with no stake part.
    #[serde(skip_serializing_if = "Option::is_none")]
    stake_assets_count: Option<u32>,
    /// ADA Handle currently held by this address, if any (without the `$`).
    #[serde(skip_serializing_if = "Option::is_none")]
    handle: Option<String>,
    /// Pool + DRep this address's stake credential delegates to (same as the linked stake
    /// feed). `None` when not delegated / no stake part.
    #[serde(skip_serializing_if = "Option::is_none")]
    pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pool_ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drep_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drep_name: Option<String>,
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
    // Stake-credential-level values, keyed by the reward-address `hash_raw` minus its
    // 1-byte header. Both summed/unioned across all of the credential's addresses, the
    // same figures the linked stake feed shows: `stake_value` = balance + rewards;
    // `stake_assets_count` = distinct multi-assets. Computed only when the address
    // actually re-emits (its own balance/asset change), not every block.
    let stake_hash_raw = stake_hash_raw_of(address, mainnet);
    let stake_value = stake_hash_raw.as_deref().map(|hash_raw| {
        let cred = &hash_raw[1..];
        snap.map(|s| {
            s.stakes.get(cred).copied().unwrap_or(0) + s.rewards.get(cred).copied().unwrap_or(0)
        })
        .unwrap_or(0)
        .to_string()
    });
    let stake_assets_count = stake_hash_raw
        .as_deref()
        .and_then(|hash_raw| snap.map(|s| s.stake_asset_count(&hash_raw[1..])));
    // Pool/DRep of the address's stake credential (same as the linked stake feed shows).
    let (pool_id, pool_ticker, drep_id, drep_name) = match stake_hash_raw.as_deref() {
        Some(hash_raw) => pool_drep_info(snap, &hash_raw[1..]),
        None => (None, None, None, None),
    };
    let json = serde_json::to_string(&AddressEvent {
        kind: "Address",
        address,
        balance: balance.to_string(),
        stake_address: stake_address_of(address, mainnet),
        stake_value,
        stake_assets_count,
        handle,
        pool_id,
        pool_ticker,
        drep_id,
        drep_name,
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

/// Distinct held-asset count for a Stake/Address feed header (0 for pool/drep). Read
/// straight from the always-current global `asset_holdings` in the snapshot — an O(1)
/// lookup for an address, a ~ms in-memory prefix-union for a stake — replacing the old
/// tens-of-seconds `COUNT(DISTINCT)` db query. Shared by `filtered_events` (regular
/// feed) and `asset_feed_events` (assets page).
async fn subject_assets_count(filter: &filter::FeedFilter, chain_state: &RwLock<State>) -> u32 {
    let guard = chain_state.read().await;
    let Some(snap) = guard.current() else {
        return 0;
    };
    match filter {
        filter::FeedFilter::Stake(payload) => snap.stake_asset_count(&payload[1..]),
        filter::FeedFilter::Address(addr) => address_bytes(addr)
            .map(|b| snap.address_asset_count(&b))
            .unwrap_or(0),
        _ => 0,
    }
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
        | crate::event::Event::ReplayCursor { .. }
        | crate::event::Event::Reward { .. } => return,
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
                                        quantity: format_quantity(raw as u128, decimals),
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
            (String, u64, crate::model::PolicyAssets),
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
                inp.assets = crate::event::policy_assets_to_info(
                    raw_assets,
                    |fp| decimals.get(fp).copied().unwrap_or(0),
                    |fp| nftcdn.compute_ladder(fp, "preview"),
                );
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
    /// Per-epoch reward rows for display (`(epoch, rows)`), pool tickers resolved.
    /// Emitted as `Event::Reward` capsules; independent of the backward walk.
    reward_capsules: Vec<(u64, Vec<crate::event::RewardRow>)>,
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
async fn build_subject_replay(
    chain_state: &RwLock<State>,
    db: &crate::state::DbSync,
    hash_raw: &[u8],
    blocks: &[(u64, String, u64, u64)],
    exclude_slots: &HashSet<u64>,
    mainnet: bool,
    anchor: Option<(i64, u64)>,
) -> SubjectReplay {
    // The stake credential is the reward address minus its 1-byte header.
    let cred = &hash_raw[1..];
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
        .stake_epoch_rewards(hash_raw, min_epoch as i64, current_epoch as i64)
        .await
        .unwrap_or_default();
    let wd_rows = db
        .stake_withdrawals_since(hash_raw, min_slot as i64)
        .await
        .unwrap_or_default();

    // Resolve delegation target identities under a second short lock (no await).
    let (deleg_by_tx, reward_capsules) = {
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

        // Per-epoch reward capsules for display: resolve pool tickers under this lock.
        let mut caps: std::collections::BTreeMap<u64, Vec<crate::event::RewardRow>> =
            std::collections::BTreeMap::new();
        for (epoch, label, pool_hash, amount) in &reward_rows {
            let (pool_id, pool_ticker) = match pool_hash {
                Some(h) => {
                    let (id, t) = resolve_pool(h);
                    (Some(id), t)
                }
                None => (None, None),
            };
            caps.entry(*epoch as u64)
                .or_default()
                .push(crate::event::RewardRow {
                    label: label.clone(),
                    amount: (*amount).max(0) as u64,
                    pool_id,
                    pool_ticker,
                });
        }
        // Rows within a capsule: pool rewards first, then by amount descending.
        for rows in caps.values_mut() {
            rows.sort_by(|a, b| {
                b.pool_id
                    .is_some()
                    .cmp(&a.pool_id.is_some())
                    .then(b.amount.cmp(&a.amount))
            });
        }
        let reward_capsules: Vec<(u64, Vec<crate::event::RewardRow>)> = caps.into_iter().collect();

        (merged, reward_capsules)
    };

    // Reward deltas newest-epoch first; off-window withdrawals newest-slot first
    // (those in the replayed set are accounted for via each block's net stake change).
    // Sum every reward source per epoch — identical to the old `stake_reward_deltas`.
    let mut delta_by_epoch: HashMap<u64, i64> = HashMap::new();
    for (epoch, _label, _pool, amount) in &reward_rows {
        *delta_by_epoch.entry(*epoch as u64).or_insert(0) += *amount;
    }
    let mut reward_deltas: Vec<(u64, i64)> = delta_by_epoch.into_iter().collect();
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
        reward_capsules,
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

/// Per-feed replay parameters, constant across every block of one replay: the
/// credential set to filter by, the feed filter, the tx_hash → delegations overlay to
/// inject, and the minimum stake change a pool/DRep feed shows.
struct ReplayParams<'a> {
    delegators: &'a imbl::hashset::HashSet<Vec<u8>>,
    feed_filter: &'a filter::FeedFilter,
    deleg_info: &'a HashMap<String, Vec<DelegationInfo>>,
    stake_threshold: u64,
}

/// Fetch one block via N2N, decode + resolve + inject delegations + (for
/// stake/address feeds) walk the stake backward, and build the `Event::Block` —
/// or `None` on fetch/decode failure or when it filters to no txs. `deleg_info`
/// maps tx_hash -> delegations to inject. Shared by `send_replay_blocks` and the
/// `/older` endpoint; the caller owns the (single-flight) N2N client.
async fn process_replay_block(
    client: &mut PeerClient,
    ctx: &ReplayCtx<'_>,
    block: &ReplayBlock,
    params: &ReplayParams<'_>,
    subject: Option<&mut SubjectReplay>,
) -> Option<crate::event::Event> {
    let &ReplayParams {
        delegators,
        feed_filter,
        deleg_info,
        stake_threshold,
    } = params;
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
    let params = ReplayParams {
        delegators,
        feed_filter,
        deleg_info,
        stake_threshold,
    };
    let mut sent = 0usize;
    for block in blocks.iter() {
        let event =
            process_replay_block(&mut client, &ctx, block, &params, subject.as_deref_mut()).await;
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
    genesis: &GenesisConfig,
) {
    let guard = chain_state.read().await;
    if let Some(snap) = guard.current() {
        if let Some(pool) = snap.pools.get(&hex::encode(pool_hash)) {
            let epoch = snap.last_epoch.unwrap_or(0);
            let epoch_blocks = pool_epoch_blocks(&guard.feed_index, pool_hash, epoch, genesis);
            let _ = sender
                .send(pool_sse_event(pool, Some(snap), epoch, epoch_blocks))
                .await;
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
            send_pool_info(
                &sender,
                &replay_state.chain_state,
                ph,
                &replay_state.genesis,
            )
            .await;

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
    /// Policy id (hex) — links to the policy page. From db-sync.
    #[serde(skip_serializing_if = "Option::is_none")]
    policy: Option<String>,
    /// Minted supply (Σ mints; string since it can exceed JS safe-int / i64).
    #[serde(skip_serializing_if = "Option::is_none")]
    quantity: Option<String>,
    /// First / last mint times (unix seconds); equal for a single-mint asset, a range
    /// when minted across several txs.
    #[serde(skip_serializing_if = "Option::is_none")]
    first_mint: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_mint: Option<i64>,
    /// The raw on-chain CIP-25/68 `metadata` object from NFTCDN, passed through for the
    /// page to format (the frontend drops the media-technical keys).
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
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

    // Chain facts (policy, supply, mint dates) run concurrently with the NFTCDN media
    // fetch; the db handle is cloned off the lock so the query never holds it.
    let db = state.chain_state.read().await.db_handle();
    let info_fut = async {
        match db {
            Some(db) => db.asset_chain_info(&fingerprint).await.unwrap_or(None),
            None => None,
        }
    };

    // NFTCDN /metadata → display name + media file URLs. Non-fatal: an asset NFTCDN
    // doesn't know (old fungible tokens with no CIP-25 media) yields empty media rather
    // than failing the whole page — the chain facts below still render.
    let media_fut = async {
        let empty = (None, None, Vec::new());
        let meta_url = state.nftcdn.signed_url(&fingerprint, "metadata", "");
        let Ok(resp) = state.http.get(&meta_url).send().await else {
            return empty;
        };
        if !resp.status().is_success() {
            return empty;
        }
        let Ok(body) = resp.text().await else {
            return empty;
        };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&body) else {
            return empty;
        };

        let inner = &meta["metadata"];
        let name = inner["name"]
            .as_str()
            .or_else(|| meta["name"].as_str())
            .map(str::to_string);
        let metadata = inner.is_object().then(|| inner.clone());

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
        (name, metadata, media)
    };

    let ((nftcdn_name, metadata, media), info) = tokio::join!(media_fut, info_fut);
    let (policy, name_bytes, quantity, first_mint, last_mint) = match info {
        Some((p, n, q, f, l)) => (Some(p), Some(n), q, f, l),
        None => (None, None, None, None, None),
    };

    // Nothing on NFTCDN *and* not a known asset → genuinely not found.
    if media.is_empty() && policy.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    // Display name: NFTCDN's, else the decoded on-chain asset name (e.g. a token ticker).
    let name = nftcdn_name.or_else(|| name_bytes.as_deref().and_then(decode_asset_name));

    Ok(axum::Json(AssetMediaResponse {
        fingerprint,
        name,
        policy,
        quantity,
        first_mint,
        last_mint,
        metadata,
        media,
    }))
}

// ---------------------------------------------------------------------------------------------
// Social-media cards (Open Graph / Twitter). Crawlers don't run the SPA's JS, so these tags must
// be server-rendered; nginx routes only crawler User-Agents to this axum fallback. The pure card
// model + HTML renderer + formatting live in `crate::og`.
// ---------------------------------------------------------------------------------------------

/// axum fallback: a social-card HTML document for any page path. The Host header gives the
/// absolute base for `og:url` / `og:image` (works across pool.pm / preprod / preview).
async fn og_page(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Html<String> {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("pool.pm");
    let base_url = format!("https://{host}");
    let path = uri.path().trim_start_matches('/');
    let url = if path.is_empty() {
        format!("{base_url}/")
    } else {
        format!("{base_url}/{path}")
    };
    let card = build_card(&state, &base_url, path).await;
    axum::response::Html(og::render(&card, &url))
}

/// Pick the card for a page path, mirroring the frontend's route parsing (`App.svelte`).
async fn build_card(state: &AppState, base_url: &str, path: &str) -> og::Card {
    // Single asset (asset1…, optionally /files/N) — the only image card.
    let head = path.split('/').next().unwrap_or("");
    if head.starts_with("asset1") && is_valid_fingerprint(head) {
        return asset_card(state, head).await;
    }
    // Policy grid.
    if let Some(policy) = path.strip_prefix("policy/") {
        if is_valid_policy_id(policy) {
            let count = match hex::decode(policy) {
                Ok(bytes) => match state.chain_state.read().await.db_handle() {
                    Some(db) => db.policy_asset_count(&bytes).await.ok(),
                    None => None,
                },
                Err(_) => None,
            };
            let desc = match count {
                Some(n) => format!("{} assets", og::commas(n)),
                None => "Cardano minting policy".to_string(),
            };
            return og::Card::branded(base_url, format!("Policy {}", og::short_id(policy)), desc);
        }
    }
    // Owned-assets grid: <addr|stake subject>/assets[/<policy>].
    if let Some((subj, _)) = path.split_once("/assets") {
        if let Some(filter) = FeedFilter::from_path(subj) {
            let guard = state.chain_state.read().await;
            return subject_card(base_url, &filter, guard.current(), true);
        }
    }
    // $handle → resolve to the holder address and show its card.
    if let Some(rest) = path.strip_prefix('$') {
        let name = rest.split('/').next().unwrap_or("").to_lowercase();
        let guard = state.chain_state.read().await;
        if let Some(snap) = guard.current() {
            if let Some(addr) = snap.address_by_handle.get(&name).cloned() {
                if let Some(filter) = FeedFilter::from_path(&addr) {
                    return subject_card(base_url, &filter, Some(snap), false);
                }
            }
        }
        return home_card(state, base_url).await;
    }
    // Feed subject: pool / drep / stake / addr bech32.
    if let Some(filter) = FeedFilter::from_path(path) {
        let guard = state.chain_state.read().await;
        return subject_card(base_url, &filter, guard.current(), false);
    }
    home_card(state, base_url).await
}

/// Home card. The social card (og:/twitter:) is the live "CARDANO" header — title + pool/DRep
/// counts; the search snippet (`<title>` / meta description) is a stable brand tagline instead,
/// since those are independent tags.
async fn home_card(state: &AppState, base_url: &str) -> og::Card {
    // Social card: pools and DReps on their own lines (newline → a break on Telegram/Discord/
    // Slack; X collapses it to a space). Falls back to a tagline if there's no snapshot yet.
    let description = match cardano_stats(state).await {
        Some(s) => format!(
            "{} pools\n{} DReps",
            og::commas(s.pool_count as i64),
            og::commas(s.drep_count)
        ),
        None => "Stake pools, wallets, native assets and DReps.".to_string(),
    };
    let mut card = og::Card::branded(base_url, "Cardano".to_string(), description);
    card.seo_title = Some("pool.pm — explore Cardano in real time".to_string());
    card.seo_description = Some(
        "Explore the Cardano blockchain in real time — stake pools, wallets, stake accounts, \
         native assets and DReps, with live blocks and mempool."
            .to_string(),
    );
    card
}

/// Card for a feed subject (pool/drep/stake/addr), read synchronously from the snapshot (no await
/// while the chain-state guard is held). `owned` = the `…/assets` grid variant.
fn subject_card(
    base_url: &str,
    filter: &FeedFilter,
    snap: Option<&BlockSnapshot>,
    owned: bool,
) -> og::Card {
    let (mut title, description) = match filter {
        FeedFilter::Pool(hash) => {
            let pool = snap.and_then(|s| s.pools.get(&hex::encode(hash)));
            let pool_id = pool_bech32_id(hash);
            let ticker = pool
                .and_then(|p| p.ticker.clone())
                .unwrap_or_else(|| pool_id.get(5..10).unwrap_or_default().to_string());
            let live = snap
                .and_then(|s| State::pool_live_stake(s, hash))
                .unwrap_or(0);
            let delegators = snap
                .and_then(|s| s.pool_delegators.get(hash))
                .map(|d| d.len())
                .unwrap_or(0);
            let blocks = pool.map(|p| p.blocks).unwrap_or(0);
            (
                og::format_ticker(&ticker),
                format!(
                    "STAKE POOL\n{}",
                    og::join(&[
                        format!("Live stake {}", og::fmt_ada(live)),
                        format!("{delegators} delegators"),
                        format!("{blocks} blocks"),
                    ])
                ),
            )
        }
        FeedFilter::DRep(bytes) => {
            let drep_id = drep_bech32_id(bytes);
            let name = match bytes.first() {
                Some(0x02) => Some("Always Abstain".to_string()),
                Some(0x03) => Some("Always No Confidence".to_string()),
                _ => snap
                    .and_then(|s| s.dreps.get(bytes))
                    .and_then(|d| d.given_name.clone()),
            };
            let live = snap
                .and_then(|s| State::drep_live_stake(s, bytes))
                .unwrap_or(0);
            let delegators = snap
                .and_then(|s| s.drep_delegators.get(bytes))
                .map(|d| d.len())
                .unwrap_or(0);
            (
                name.unwrap_or_else(|| og::short_id(&drep_id)),
                format!(
                    "DREP\n{}",
                    og::join(&[
                        format!("Live stake {}", og::fmt_ada(live)),
                        format!("{delegators} delegators"),
                    ])
                ),
            )
        }
        FeedFilter::Stake(payload) => {
            let cred = &payload[1..];
            let handle = snap.and_then(|s| s.handle_for_stake(cred));
            let balance = snap.and_then(|s| s.stakes.get(cred).copied()).unwrap_or(0);
            let rewards = snap.and_then(|s| s.rewards.get(cred).copied()).unwrap_or(0);
            let (pool_id, pool_ticker, drep_id, drep_name) = pool_drep_info(snap, cred);
            let assets = snap.map(|s| s.stake_asset_count(cred)).unwrap_or(0);
            let title = match handle {
                Some(h) => format!("${h}'s stake"),
                None => og::short_id(&filter.feed_id()),
            };
            (
                title,
                // Balance (+ delegation) on the first line, then the asset count — same shape as
                // the address card (distinct assets across all of the credential's addresses).
                format!(
                    "{}\n{} assets",
                    og::join(&[
                        og::fmt_ada(balance + rewards),
                        pool_line(&pool_id, &pool_ticker),
                        drep_line(&drep_id, &drep_name),
                    ]),
                    og::commas(assets as i64)
                ),
            )
        }
        FeedFilter::Address(addr) => {
            let handle = snap.and_then(|s| s.handle_for(addr));
            let addr_bytes = address_bytes(addr);
            let balance = addr_bytes
                .as_deref()
                .and_then(|b| snap.and_then(|s| s.address_balances.get(b).copied()))
                .unwrap_or(0);
            let assets = addr_bytes
                .as_deref()
                .and_then(|b| snap.map(|s| s.address_asset_count(b)))
                .unwrap_or(0);
            let cred = crate::pallas::stake_credential_from_bech32(addr);
            let (pool_id, pool_ticker, drep_id, drep_name) = match cred.as_deref() {
                Some(c) => pool_drep_info(snap, c),
                None => (None, None, None, None),
            };
            let title = match handle {
                Some(h) => format!("${h}"),
                None => og::short_id(addr),
            };
            (
                title,
                // Balance (+ delegation, if any) on the first line, then the asset count.
                format!(
                    "{}\n{} assets",
                    og::join(&[
                        og::fmt_ada(balance),
                        pool_line(&pool_id, &pool_ticker),
                        drep_line(&drep_id, &drep_name),
                    ]),
                    og::commas(assets as i64)
                ),
            )
        }
    };
    if owned {
        title.push_str(" assets");
    }
    og::Card::branded(base_url, title, description)
}

fn pool_line(pool_id: &Option<String>, ticker: &Option<String>) -> String {
    match pool_id {
        Some(id) => {
            let t = ticker
                .clone()
                .unwrap_or_else(|| id.get(5..10).unwrap_or_default().to_string());
            format!("pool {}", og::format_ticker(&t))
        }
        None => String::new(),
    }
}

fn drep_line(drep_id: &Option<String>, name: &Option<String>) -> String {
    match drep_id {
        Some(id) => format!("DRep {}", name.clone().unwrap_or_else(|| og::short_id(id))),
        None => String::new(),
    }
}

/// Card for a single asset: NFTCDN display name + `/image` @1024, plus on-chain quantity/policy
/// (reuses the same NFTCDN-metadata + `asset_chain_info` merge as `asset_media`).
async fn asset_card(state: &AppState, fingerprint: &str) -> og::Card {
    let image = state.nftcdn.signed_url(fingerprint, "image", "size=1024");
    let db = state.chain_state.read().await.db_handle();
    let info_fut = async {
        match db {
            Some(db) => db.asset_chain_info(fingerprint).await.unwrap_or(None),
            None => None,
        }
    };
    let meta_url = state.nftcdn.signed_url(fingerprint, "metadata", "");
    let name_fut = async {
        let resp = state.http.get(&meta_url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let meta = serde_json::from_str::<serde_json::Value>(&resp.text().await.ok()?).ok()?;
        meta["metadata"]["name"]
            .as_str()
            .or_else(|| meta["name"].as_str())
            .map(str::to_string)
    };
    let (nftcdn_name, info) = tokio::join!(name_fut, info_fut);
    let (name_bytes, quantity, first_mint) = match info {
        Some((_policy, n, q, first, _last)) => (Some(n), q, first),
        None => (None, None, None),
    };
    let name = nftcdn_name
        .or_else(|| name_bytes.as_deref().and_then(decode_asset_name))
        .unwrap_or_else(|| fingerprint.to_string());
    let mut parts = Vec::new();
    if let Some(q) = quantity {
        parts.push(format!("Quantity {q}"));
    }
    // First mint date (day-numeric / short-month / year, e.g. "15 Jan 2022"), matching the
    // asset page's placard — more telling than the policy id.
    if let Some(minted) = first_mint.and_then(fmt_mint_date) {
        parts.push(format!("Minted {minted}"));
    }
    let description = if parts.is_empty() {
        "Cardano native asset".to_string()
    } else {
        og::join(&parts)
    };
    og::Card::with_image(name, description, image)
}

/// A unix timestamp (seconds) as a `"15 Jan 2022"` date, or `None` if out of range.
fn fmt_mint_date(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.format("%-d %b %Y").to_string())
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
    /// `desc` (default) or `asc` — the assets grid's sort direction.
    order: Option<String>,
    /// Optional case-insensitive substring filter on the asset name. Absent/empty = no
    /// filter (unchanged query path); only sent by the flat grids when the box is non-empty.
    q: Option<String>,
}

/// Normalize a `?q=` name filter: trimmed + lowercased, `None` when absent or empty (so the
/// unfiltered query path is taken untouched).
fn name_filter(q: &Option<String>) -> Option<String> {
    q.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
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

#[derive(serde::Serialize)]
struct AssetsResponse {
    assets: Vec<AssetItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<i64>,
    has_more: bool,
}

/// One policy's tile on the owned-assets grid: its held-asset `count` and up to
/// `GROUP_SAMPLES` sample tiles for the stacked-card thumbnail. A `count` of 1
/// renders as a plain asset tile on the frontend.
#[derive(serde::Serialize)]
struct AssetGroup {
    policy: String,
    count: usize,
    samples: Vec<AssetItem>,
}

#[derive(serde::Serialize)]
struct GroupsResponse {
    groups: Vec<AssetGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<i64>,
    has_more: bool,
}

/// Sample thumbnails shown in a multi-asset policy's stacked-card tile.
/// Must match `GROUP_SAMPLES` in the frontend `AssetsGrid.svelte`.
const GROUP_SAMPLES: usize = 5;
/// Policy groups returned per owned-assets page.
const GROUP_PAGE_SIZE: usize = 512;

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
    // Policy browse has no per-owner quantity, so "quantity then mint date" collapses to
    // mint date: descending (default) = newest first, ascending = oldest first.
    let rows = db
        .assets_by_policy(
            &policy,
            query.cursor,
            POLICY_PAGE_SIZE,
            !is_descending(&query.order),
            name_filter(&query.q).as_deref(),
        )
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let has_more = rows.len() as i64 == POLICY_PAGE_SIZE;
    let cursor = rows.last().map(|(id, ..)| *id);

    let assets = rows
        .into_iter()
        .map(|(_, fingerprint, name_bytes)| {
            row_to_asset(&state.nftcdn, &policy_id, fingerprint, name_bytes)
        })
        .collect();

    Ok(axum::Json(AssetsResponse {
        assets,
        cursor,
        has_more,
    }))
}

/// Held `(policy, name)` tokens for an address/stake subject, cloned off the
/// `chain_state` lock (the clone is sync — no await held). Errs 400 for a
/// non-address/stake filter or an unparseable address, 503 before the first snapshot.
type HeldList = Vec<(Vec<u8>, Vec<u8>, u128, u32)>;

/// Sort direction from the `?order=` query param. Defaults to descending (highest quantity /
/// newest mint first); `?order=asc` reverses it.
fn is_descending(order: &Option<String>) -> bool {
    order.as_deref() != Some("asc")
}

async fn collect_held(
    state: &AppState,
    filter: &filter::FeedFilter,
) -> Result<(HeldList, imbl::HashMap<String, u8>), StatusCode> {
    let guard = state.chain_state.read().await;
    let snap = guard.current().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let held = match filter {
        filter::FeedFilter::Address(addr) => {
            let bytes = address_bytes(addr).ok_or(StatusCode::BAD_REQUEST)?;
            snap.address_held_assets(&bytes)
        }
        filter::FeedFilter::Stake(payload) => snap.stake_held_assets(&payload[1..]),
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    // Clone the (small, non-zero-only) decimals map so quantities can be formatted off
    // the lock alongside URL signing.
    Ok((held, snap.decimals.clone()))
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

/// Assets owned by a payment address (`addr1…`) or stake credential (`stake1…`),
/// **grouped by policy** — one tile per policy with its held `count` and up to
/// `GROUP_SAMPLES` sample tiles (the frontend renders a stacked-card thumbnail and
/// drills into `/{subject}/assets/{policy}`). Served from the in-memory
/// `asset_holdings` map (no db scan); CIP-68 reference NFTs are *not* filtered — owned
/// listings show what the wallet actually holds. `cursor` is an integer offset into the
/// `(policy, name)`-sorted policy list. Only `Address`/`Stake` filters; others 400.
async fn owned_assets(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(feed_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<PolicyQuery>,
) -> Result<axum::Json<GroupsResponse>, StatusCode> {
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;
    let (mut held, decimals) = collect_held(&state, &filter).await?;
    // Optional name filter (only when the box is non-empty): drop non-matching assets before
    // grouping, so each remaining policy tile stacks/counts just its matching assets and
    // policies with no match fall away. Nothing when unfiltered.
    if let Some(q) = name_filter(&query.q) {
        held.retain(|(_, name, _, _)| {
            decode_asset_name(name).is_some_and(|n| n.to_lowercase().contains(&q))
        });
    }
    held.sort_unstable();

    // held is sorted by (policy, name), so each policy's tokens are contiguous: count
    // them all, keeping up to GROUP_SAMPLES (name, quantity) samples for the thumbnail and
    // the group's oldest mint_time for the sort below.
    let mut groups: Vec<PolicyGroup> = Vec::new();
    for (policy, name, qty, mint_time) in held {
        if let Some((p, count, samples, min_mint)) = groups.last_mut() {
            if *p == policy {
                *count += 1;
                if samples.len() < GROUP_SAMPLES {
                    samples.push((name, qty));
                }
                *min_mint = (*min_mint).min(mint_time);
                continue;
            }
        }
        groups.push((policy, 1, vec![(name, qty)], mint_time));
    }

    // Sort the policy tiles: a single-asset tile by its quantity, a multi-asset stack (NFTs,
    // quantity 1) by the group's oldest mint. (sort_qty, mint, policy) is a total order;
    // reversed for the default descending.
    let descending = is_descending(&query.order);
    groups.sort_unstable_by(|a, b| {
        let ka = (if a.1 == 1 { a.2[0].1 } else { 1 }, a.3, &a.0);
        let kb = (if b.1 == 1 { b.2[0].1 } else { 1 }, b.3, &b.0);
        let ord = ka.cmp(&kb);
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });

    let total = groups.len();
    let offset = query.cursor.unwrap_or(0).max(0) as usize;
    let groups: Vec<AssetGroup> = groups
        .into_iter()
        .skip(offset)
        .take(GROUP_PAGE_SIZE)
        .map(|(policy, count, samples, _min_mint)| {
            let policy_hex = hex::encode(&policy);
            let samples = samples
                .into_iter()
                .map(|(name, qty)| {
                    build_owned_tile(&state.nftcdn, &policy_hex, &policy, name, qty, &decimals)
                })
                .collect();
            AssetGroup {
                policy: policy_hex,
                count,
                samples,
            }
        })
        .collect();
    let next = offset + groups.len();
    let has_more = next < total;
    let cursor = has_more.then_some(next as i64);

    Ok(axum::Json(GroupsResponse {
        groups,
        cursor,
        has_more,
    }))
}

/// One policy's held assets for a subject — the grouped grid's drill-down
/// (`/{subject}/assets/{policy}`). Same in-memory source as `owned_assets`, filtered to
/// the policy and returned flat (one tile per asset), offset-paginated.
async fn owned_assets_by_policy(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path((feed_id, policy_id)): axum::extract::Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<PolicyQuery>,
) -> Result<axum::Json<AssetsResponse>, StatusCode> {
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;
    if !is_valid_policy_id(&policy_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let policy = hex::decode(&policy_id).map_err(|_| StatusCode::BAD_REQUEST)?;

    let (mut held, decimals) = collect_held(&state, &filter).await?;
    held.retain(|(p, _, _, _)| *p == policy);
    // Optional name filter (only when the box is non-empty): the full per-policy held set is
    // already in memory, so this is an extra in-place pass — nothing when unfiltered.
    if let Some(q) = name_filter(&query.q) {
        held.retain(|(_, name, _, _)| {
            decode_asset_name(name).is_some_and(|n| n.to_lowercase().contains(&q))
        });
    }
    // Sort by (quantity, mint_time, name) — name makes it a total order for stable offset
    // pagination; reversed for the default descending (highest qty / newest mint first).
    let descending = is_descending(&query.order);
    held.sort_unstable_by(|a, b| {
        let ord = (a.2, a.3, &a.1).cmp(&(b.2, b.3, &b.1));
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });

    let total = held.len();
    let offset = query.cursor.unwrap_or(0).max(0) as usize;
    let assets: Vec<AssetItem> = held
        .into_iter()
        .skip(offset)
        .take(POLICY_PAGE_SIZE as usize)
        .map(|(_, name, qty, _)| {
            build_owned_tile(&state.nftcdn, &policy_id, &policy, name, qty, &decimals)
        })
        .collect();
    let next = offset + assets.len();
    let has_more = next < total;
    let cursor = has_more.then_some(next as i64);

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
            Some(
                build_subject_replay(
                    &state.chain_state,
                    &db,
                    hr,
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
    let params = ReplayParams {
        delegators: &delegators,
        feed_filter: &filter,
        deleg_info: &deleg_info,
        stake_threshold: 0,
    };
    let mut events = Vec::new();
    for block in &replay_blocks {
        if let Some(mut ev) =
            process_replay_block(&mut client, &ctx, block, &params, subject.as_mut()).await
        {
            resolve_event_assets(&mut ev, size);
            events.push(ev);
        }
    }
    let _ = client.abort().await;

    // Per-epoch reward capsules for this page, at their epoch-change slot/timestamp.
    if let Some(sr) = &subject {
        for (epoch, rows) in &sr.reward_capsules {
            let slot = slot_for_epoch(*epoch, &state.genesis);
            events.push(crate::event::Event::Reward {
                epoch: *epoch,
                slot,
                timestamp: slot_to_timestamp(slot, &state.genesis),
                rows: rows.clone(),
            });
        }
    }

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
        cardano_cache: Arc::new(tokio::sync::Mutex::new(None)),
    };
    let app = Router::new()
        // Any unmatched path → social-card HTML (nginx routes only crawler UAs here). All methods,
        // so a crawler's HEAD probe is answered too.
        .fallback(og_page)
        .route("/events", get(events))
        .route("/events/{feed_id}/assets", get(asset_feed_events))
        .route("/events/{feed_id}", get(filtered_events))
        .route("/api/asset/{fingerprint}", get(asset_media))
        .route("/api/policy/{policy_id}", get(policy_assets))
        .route("/api/assets/{feed_id}", get(owned_assets))
        .route(
            "/api/assets/{feed_id}/{policy}",
            get(owned_assets_by_policy),
        )
        .route("/api/feed/{feed_id}/older", get(older_blocks))
        .route("/api/search", get(search))
        .route("/api/handle/{name}", get(resolve_handle))
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
            reward_capsules: Vec::new(),
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
            reward_capsules: Vec::new(),
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
            reward_capsules: Vec::new(),
        };
        assert_eq!(p2.pre_block_stake(638, 800, -40), 880);
    }

    #[test]
    fn search_score_tiers_and_case() {
        // Case-insensitive: lowercase query matches an uppercase ticker.
        assert!(search_score("ccv", "CCVAULT").is_some());
        // Exact > prefix > substring > fuzzy.
        let exact = search_score("ccv", "CCV").unwrap();
        let prefix = search_score("ccv", "CCVAULT").unwrap();
        let substring = search_score("vault", "CCVAULT").unwrap();
        assert!(exact > prefix && prefix > substring);
        // "card" ranks "Cardano" (prefix) above "Discard" (substring).
        assert!(
            search_score("card", "Cardano").unwrap() > search_score("card", "Discard").unwrap()
        );
        // Shorter prefix match beats a longer one for the same query.
        assert!(
            search_score("ada", "ADAPOOL").unwrap() > search_score("ada", "ADAPOOLXXXXXX").unwrap()
        );
        // Unrelated → dropped.
        assert!(search_score("zzzz", "Cardano").is_none());
        // A close typo still matches via Jaro-Winkler.
        assert!(search_score("cardona", "Cardano").is_some());
    }
}
