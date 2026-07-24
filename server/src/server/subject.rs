//! Per-subject SSE header/info builders (pool / DRep / stake / address): the `*_sse_event`
//! shapers and `send_*_info` senders, plus small address/stake helpers. Shared bits
//! (cardano_stats, pool_drep_info, address_bytes) stay in `server`, reached via `super::*`.
use super::*;

/// Exact count of blocks this pool minted in the current epoch. Read from the feed index,
/// which holds the pool's full minted-block list for the whole 5-day window (≥ one epoch, and
/// uncapped — the 30-block cap is only on what's *sent* to a client), filtered to slots at or
/// after the current epoch's start. Rollback-safe: the feed index reverts `pool_minted` on a
/// rollback (see `FeedIndex::rollback`), so this recomputes correctly.
pub(super) fn pool_epoch_blocks(
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

pub(super) fn pool_sse_event(
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

pub(super) fn drep_sse_event(
    drep_bytes: &[u8],
    snap: Option<&BlockSnapshot>,
) -> Result<SseEvent, Infallible> {
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

#[derive(serde::Serialize)]
pub(super) struct StakeEvent<'a> {
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

pub(super) fn stake_sse_event(
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
pub(super) async fn send_stake_info(
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
pub(super) fn stake_hash_raw_of(address: &str, mainnet: bool) -> Option<Vec<u8>> {
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

pub(super) fn stake_address_of(address: &str, mainnet: bool) -> Option<String> {
    let payload = stake_hash_raw_of(address, mainnet)?;
    let hrp = if mainnet { "stake" } else { "stake_test" };
    bech32::encode::<bech32::Bech32>(bech32::Hrp::parse(hrp).unwrap(), &payload).ok()
}

#[derive(serde::Serialize)]
pub(super) struct AddressEvent<'a> {
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

/// Build an `Address` info event — balance from snapshot `address_balances`
/// (kept live by the sink); `assets_count` is the caller's connect-time value.
pub(super) fn address_sse_event(
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
pub(super) async fn send_address_info(
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
pub(super) async fn subject_assets_count(
    filter: &filter::FeedFilter,
    chain_state: &RwLock<State>,
) -> u32 {
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

/// Extract pool id, ticker, and hash from current state. Fast (in-memory).
pub(super) fn extract_pool_meta(
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
pub(super) async fn send_pool_info(
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
pub(super) async fn send_drep_info(
    sender: &Sender<Result<SseEvent, Infallible>>,
    chain_state: &RwLock<State>,
    drep_bytes: &[u8],
) {
    let guard = chain_state.read().await;
    let snap = guard.current();
    let _ = sender.send(drep_sse_event(drep_bytes, snap)).await;
}
