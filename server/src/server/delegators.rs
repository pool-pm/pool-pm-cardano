//! Delegators REST endpoint (`/api/delegators/{feed_id}`) — the pool/DRep delegators grid.
//!
//! Everything a tile needs is in the snapshot: the delegator set (`pool_delegators` /
//! `drep_delegators`), each credential's live stake (`stakes + rewards`), its ADA Handle
//! (`handle_by_stake`) and the slot its current run with this subject began
//! (`Delegation::since_slot`). No db query — the whole page is one short read-guard scope
//! plus pure work off the lock, like `assets::owned_assets`.
use super::*;

#[derive(serde::Deserialize)]
pub(super) struct DelegatorsQuery {
    /// Integer offset into the sorted list (same scheme as the owned-assets grid).
    cursor: Option<i64>,
    /// `time` sorts by when the delegation started; anything else (default) by live stake.
    sort: Option<String>,
    /// `asc`, or `desc` by default — reuses the assets grid's convention.
    order: Option<String>,
    /// Handle or stake-address filter; absent/empty = no filter.
    q: Option<String>,
}

#[derive(serde::Serialize)]
pub(super) struct DelegatorItem {
    /// bech32 reward address — the tile's identity and its link target.
    stake_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    handle: Option<String>,
    /// `stakes + rewards`, lovelace as a string (can exceed JS `Number.MAX_SAFE_INTEGER`).
    live_stake: String,
    /// Epoch the current run with this subject began, derived from the stored slot.
    /// Absent when the slot is unknown (a snapshot that predates the field, pre-backfill).
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<u64>,
    /// Unix seconds of that same moment, for the tile's tooltip.
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<u64>,
}

#[derive(serde::Serialize)]
pub(super) struct DelegatorsResponse {
    delegators: Vec<DelegatorItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<i64>,
    has_more: bool,
    /// Delegators matching the current filter — the whole set, not just this page.
    total: usize,
}

/// Delegators returned per page. Tiles are text-only on a ~200 px pitch, so an 8K display
/// shows ~38×36 ≈ 1400 at once; 2048 covers that with headroom while keeping the JSON
/// under a few hundred KB. The grid windows and prefetches beyond it.
const DELEGATORS_PAGE_SIZE: usize = 2048;

/// One delegator, in the form the sort and the filter work on. Bech32 encoding and handle
/// resolution happen for the returned page only, never for the whole set.
struct Row {
    cred: Vec<u8>,
    live_stake: i64,
    /// 0 = unknown (pre-backfill snapshot); sorts as the oldest.
    since_slot: u64,
}

/// Sort axis: `?sort=time` orders by when the delegation started, otherwise by live stake.
fn by_time(sort: &Option<String>) -> bool {
    sort.as_deref() == Some("time")
}

/// Total order over delegators, so offset pagination is stable across pages: the primary
/// axis, then the other one, then the credential as the final tiebreak. `descending`
/// reverses the whole tuple (so the secondary key flips with it, as in `owned_assets`).
fn sort_rows(rows: &mut [Row], by_time: bool, descending: bool) {
    rows.sort_unstable_by(|a, b| {
        let ord = if by_time {
            (a.since_slot, a.live_stake, &a.cred).cmp(&(b.since_slot, b.live_stake, &b.cred))
        } else {
            (a.live_stake, a.since_slot, &a.cred).cmp(&(b.live_stake, b.since_slot, &b.cred))
        };
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });
}

/// What a `?q=` filter matches against.
enum DelegatorFilter {
    /// Case-insensitive substring over the credential's ADA Handles. A leading `$` forces
    /// this mode, so `$foo` never falls through to an address match.
    Handle(String),
    /// A complete `stake1…` — decoded once to its 28-byte credential, then an O(1) compare.
    Cred(Vec<u8>),
    /// A partial `stake1…`: the bech32 data chars decoded to a *bit* prefix of the reward
    /// address payload (header byte + credential). Comparing bits avoids bech32-encoding
    /// every delegator on every keystroke — an address prefix costs the same as a handle.
    AddressPrefix { bytes: Vec<u8>, bits: usize },
}

/// bech32 charset, index = the 5-bit value of each character.
const BECH32_CHARSET: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// Decode a bech32 *data part* (the characters after the `1` separator) into the leading
/// bits of the value it encodes: `(whole bytes, total bits)`. `None` on a non-charset
/// character. Trailing bits beyond `bits` in the last byte are zero.
fn bech32_bit_prefix(data: &str) -> Option<(Vec<u8>, usize)> {
    let mut acc: u32 = 0;
    let mut acc_bits = 0usize;
    let mut out = Vec::new();
    let mut bits = 0usize;
    for c in data.bytes() {
        let v = BECH32_CHARSET.iter().position(|&x| x == c)? as u32;
        acc = (acc << 5) | v;
        acc_bits += 5;
        bits += 5;
        while acc_bits >= 8 {
            acc_bits -= 8;
            out.push(((acc >> acc_bits) & 0xff) as u8);
        }
    }
    if acc_bits > 0 {
        out.push(((acc << (8 - acc_bits)) & 0xff) as u8);
    }
    Some((out, bits))
}

/// Does `payload` (a 29-byte reward address: header + credential) start with the given
/// bit prefix?
fn has_bit_prefix(payload: &[u8], prefix: &[u8], bits: usize) -> bool {
    let full = bits / 8;
    let rest = bits % 8;
    if payload.len() < full + usize::from(rest > 0) {
        return false;
    }
    if payload[..full] != prefix[..full] {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    payload[full] & mask == prefix[full] & mask
}

/// Parse `?q=` into a filter. `None` when absent/empty (the unfiltered path).
fn delegator_filter(q: &Option<String>, mainnet: bool) -> Option<DelegatorFilter> {
    let raw = q.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    if let Some(name) = raw.strip_prefix('$') {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        return Some(DelegatorFilter::Handle(name.to_lowercase()));
    }
    let lower = raw.to_lowercase();
    let hrp = if mainnet { "stake1" } else { "stake_test1" };
    if let Some(data) = lower.strip_prefix(hrp) {
        // A complete address decodes (checksum included) — match its credential exactly.
        if let Ok((_, payload)) = bech32::decode(&lower) {
            if payload.len() == 29 {
                return Some(DelegatorFilter::Cred(payload[1..].to_vec()));
            }
        }
        if let Some((bytes, bits)) = bech32_bit_prefix(data) {
            return Some(DelegatorFilter::AddressPrefix { bytes, bits });
        }
    }
    Some(DelegatorFilter::Handle(lower))
}

/// Reward-address payload for a credential: the network+key header byte, then the
/// credential — the bytes `stake_address_from_cred_bytes` bech32-encodes.
fn reward_payload(cred: &[u8], mainnet: bool) -> Vec<u8> {
    let header = if mainnet { 0xe1u8 } else { 0xe0 };
    let mut payload = Vec::with_capacity(1 + cred.len());
    payload.push(header);
    payload.extend_from_slice(cred);
    payload
}

fn matches_filter(
    f: &DelegatorFilter,
    cred: &[u8],
    handles: Option<&Vec<String>>,
    mainnet: bool,
) -> bool {
    match f {
        DelegatorFilter::Handle(needle) => handles.is_some_and(|hs| {
            hs.iter()
                .any(|h| h.to_lowercase().contains(needle.as_str()))
        }),
        DelegatorFilter::Cred(want) => cred == want.as_slice(),
        DelegatorFilter::AddressPrefix { bytes, bits } => {
            has_bit_prefix(&reward_payload(cred, mainnet), bytes, *bits)
        }
    }
}

/// A pool's or DRep's delegators, one tile each: live stake, ADA Handle, and the epoch the
/// delegation started (the start of the credential's current uninterrupted run with this
/// subject — re-delegating to the same subject doesn't restart it).
///
/// Sorted server-side by `?sort=`/`?order=` and filtered by `?q=` over the *whole* set, so
/// the grid's infinite scroll pages a complete, stable ordering. Only `Pool`/`DRep` feeds;
/// others 400.
pub(super) async fn subject_delegators(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(feed_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<DelegatorsQuery>,
) -> Result<axum::Json<DelegatorsResponse>, StatusCode> {
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;
    if !matches!(
        filter,
        filter::FeedFilter::Pool(_) | filter::FeedFilter::DRep(_)
    ) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // One short read guard, nothing awaited inside: pull the rows and the (O(1) to clone)
    // handle map, then do all the sorting/filtering/encoding off the lock.
    let (mut rows, handles) = {
        let guard = state.chain_state.read().await;
        let snap = guard.current().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let delegations = match &filter {
            filter::FeedFilter::DRep(_) => &snap.drep_delegations,
            _ => &snap.pool_delegations,
        };
        let creds = filter.current_delegators(snap);
        let mut rows: Vec<Row> = Vec::with_capacity(creds.len());
        for cred in creds.iter() {
            rows.push(Row {
                live_stake: snap.stakes.get(cred).copied().unwrap_or(0)
                    + snap.rewards.get(cred).copied().unwrap_or(0),
                since_slot: delegations.get(cred).map(|d| d.since_slot).unwrap_or(0),
                cred: cred.clone(),
            });
        }
        (rows, snap.handle_by_stake.clone())
    };

    let mainnet = state.mainnet;
    if let Some(f) = delegator_filter(&query.q, mainnet) {
        rows.retain(|r| matches_filter(&f, &r.cred, handles.get(&r.cred), mainnet));
    }
    sort_rows(
        &mut rows,
        by_time(&query.sort),
        assets::is_descending(&query.order),
    );

    let total = rows.len();
    let offset = query.cursor.unwrap_or(0).max(0) as usize;
    let genesis = state.genesis;
    let delegators: Vec<DelegatorItem> = rows
        .into_iter()
        .skip(offset)
        .take(DELEGATORS_PAGE_SIZE)
        .map(|r| DelegatorItem {
            stake_address: crate::pallas::stake_address_from_cred_bytes(&r.cred, mainnet),
            handle: handles
                .get(&r.cred)
                .and_then(|hs| hs.iter().min_by_key(|h| h.len()).cloned()),
            live_stake: r.live_stake.to_string(),
            epoch: (r.since_slot > 0).then(|| epoch_for_slot(r.since_slot, &genesis)),
            since: (r.since_slot > 0).then(|| slot_to_timestamp(r.since_slot, &genesis)),
        })
        .collect();
    let next = offset + delegators.len();
    let has_more = next < total;

    Ok(axum::Json(DelegatorsResponse {
        delegators,
        cursor: has_more.then_some(next as i64),
        has_more,
        total,
    }))
}

/// Per-connection state the live delegators grid diffs against: the subject's delegator set
/// as of the previous block (an O(1) `imbl` clone) and the epoch it was taken in.
pub(super) struct LiveDelegators {
    creds: imbl::HashSet<Vec<u8>>,
    epoch: u64,
}

#[derive(serde::Serialize)]
struct DelegatorStake {
    stake_address: String,
    live_stake: String,
}

#[derive(serde::Serialize)]
struct DelegatorDeltaWire<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    slot: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    added: Vec<DelegatorItem>,
    /// Stake addresses that left the subject (delegated elsewhere or deregistered).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    removed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    updated: Vec<DelegatorStake>,
    /// The page can't be patched incrementally — reload it (rollback / epoch boundary).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    resync: bool,
}

fn resync_event(slot: u64) -> Option<Result<SseEvent, Infallible>> {
    let json = serde_json::to_string(&DelegatorDeltaWire {
        kind: "DelegatorDelta",
        slot,
        added: Vec::new(),
        removed: Vec::new(),
        updated: Vec::new(),
        resync: true,
    })
    .ok()?;
    Some(Ok(SseEvent::default().data(json)))
}

/// One block's changes to an open delegators grid, as an SSE `DelegatorDelta`.
///
/// Cheap by construction: the candidates are the credentials the *block itself* touched
/// (delegation certs, moved UTXOs, withdrawals) — a few hundred at most — and each is a pair
/// of O(1) set lookups against the previous and current delegator sets. Nothing here is
/// proportional to the subject's delegator count, which can be 280k.
///
/// Two cases can't be patched tile by tile and ask the grid to reload instead:
/// * a **rollback** carries no tx list, so the touched credentials are unknown;
/// * an **epoch boundary** credits rewards to every credential at once, so every tile's
///   stake is stale rather than just the block's.
///
/// Returns `None` when nothing changed (no wasted SSE frame), mirroring `asset_delta_event`.
pub(super) async fn delegator_delta_event(
    event: &crate::event::Event,
    filter: &filter::FeedFilter,
    chain_state: &RwLock<State>,
    prev: &mut Option<LiveDelegators>,
    mainnet: bool,
    genesis: &GenesisConfig,
) -> Option<Result<SseEvent, Infallible>> {
    let (slot, txs) = match event {
        crate::event::Event::Block { slot, txs, .. } => (*slot, Some(txs)),
        crate::event::Event::Rollback { slot } => (*slot, None),
        _ => return None,
    };

    // Credentials this block could have changed. A delegation cert's credential need not
    // appear in the tx's inputs/outputs (fees can be paid from another account), so the
    // certs are collected in their own right.
    let mut candidates: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    if let Some(txs) = txs {
        for tx in txs.iter() {
            for d in &tx.delegations {
                if let Some(cred) = crate::pallas::stake_credential_from_bech32(&d.stake_address) {
                    candidates.insert(cred);
                }
            }
            for cred in &tx.stake_credentials {
                candidates.insert(cred.clone());
            }
        }
    }

    // One short guard, no await inside.
    let guard = chain_state.read().await;
    let snap = guard.current()?;
    let curr = filter.current_delegators(snap);
    let epoch = snap.last_epoch.unwrap_or(0);
    let Some(before) = prev.replace(LiveDelegators {
        creds: curr.clone(),
        epoch,
    }) else {
        return None; // first block on this connection — the grid just loaded its own page
    };
    if txs.is_none() || before.epoch != epoch {
        return resync_event(slot);
    }

    let delegations = match filter {
        filter::FeedFilter::DRep(_) => &snap.drep_delegations,
        _ => &snap.pool_delegations,
    };
    let (mut added, mut removed, mut updated) = (Vec::new(), Vec::new(), Vec::new());
    for cred in candidates {
        let address = crate::pallas::stake_address_from_cred_bytes(&cred, mainnet);
        match (before.creds.contains(&cred), curr.contains(&cred)) {
            (false, true) => {
                let since_slot = delegations.get(&cred).map(|d| d.since_slot).unwrap_or(slot);
                added.push(DelegatorItem {
                    stake_address: address,
                    handle: snap.handle_for_stake(&cred),
                    live_stake: (snap.stakes.get(&cred).copied().unwrap_or(0)
                        + snap.rewards.get(&cred).copied().unwrap_or(0))
                    .to_string(),
                    epoch: Some(epoch_for_slot(since_slot, genesis)),
                    since: Some(slot_to_timestamp(since_slot, genesis)),
                });
            }
            (true, false) => removed.push(address),
            (true, true) => updated.push(DelegatorStake {
                stake_address: address,
                live_stake: (snap.stakes.get(&cred).copied().unwrap_or(0)
                    + snap.rewards.get(&cred).copied().unwrap_or(0))
                .to_string(),
            }),
            (false, false) => {}
        }
    }
    drop(guard);

    if added.is_empty() && removed.is_empty() && updated.is_empty() {
        return None;
    }
    let json = serde_json::to_string(&DelegatorDeltaWire {
        kind: "DelegatorDelta",
        slot,
        added,
        removed,
        updated,
        resync: false,
    })
    .ok()?;
    Some(Ok(SseEvent::default().data(json)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(stake: i64, slot: u64, cred: u8) -> Row {
        Row {
            cred: vec![cred; 28],
            live_stake: stake,
            since_slot: slot,
        }
    }
    fn creds(rows: &[Row]) -> Vec<u8> {
        rows.iter().map(|r| r.cred[0]).collect()
    }

    /// The four sort states the grid cycles through, including the tiebreaks that keep
    /// offset pagination stable.
    #[test]
    fn sort_rows_covers_the_four_states() {
        // Two rows share a stake (tiebreak on slot), two share a slot (tiebreak on stake).
        let mk = || {
            vec![
                row(100, 50, 1),
                row(300, 10, 2),
                row(100, 20, 3),
                row(300, 10, 4),
            ]
        };

        let mut r = mk(); // stake desc: 300s first (older slot first within the tie is
        sort_rows(&mut r, false, true); // reversed too, so cred 4 precedes cred 2)
        assert_eq!(creds(&r), vec![4, 2, 1, 3]);

        let mut r = mk();
        sort_rows(&mut r, false, false); // stake asc
        assert_eq!(creds(&r), vec![3, 1, 2, 4]);

        let mut r = mk();
        sort_rows(&mut r, true, true); // newest delegation first
        assert_eq!(creds(&r), vec![1, 3, 4, 2]);

        let mut r = mk();
        sort_rows(&mut r, true, false); // longest-standing first
        assert_eq!(creds(&r), vec![2, 4, 3, 1]);
    }

    #[test]
    fn by_time_and_defaults() {
        assert!(by_time(&Some("time".into())));
        assert!(!by_time(&Some("stake".into())));
        assert!(!by_time(&None));
    }

    /// A bech32 data prefix must match exactly the addresses that start with it — bit for
    /// bit, including a partial trailing character.
    #[test]
    fn address_prefix_matches_by_bits() {
        let cred = vec![0x42u8; 28];
        let addr = crate::pallas::stake_address_from_cred_bytes(&cred, true);
        let payload = reward_payload(&cred, true);
        let data = addr.strip_prefix("stake1").unwrap();

        for take in [1usize, 2, 3, 7, 12, 20] {
            let (bytes, bits) = bech32_bit_prefix(&data[..take]).unwrap();
            assert!(
                has_bit_prefix(&payload, &bytes, bits),
                "prefix of {take} chars should match its own address"
            );
        }

        // A different credential doesn't match a 12-char prefix of this one.
        let other = reward_payload(&vec![0x43u8; 28], true);
        let (bytes, bits) = bech32_bit_prefix(&data[..12]).unwrap();
        assert!(!has_bit_prefix(&other, &bytes, bits));

        // Non-charset characters (b, i, o, 1) are rejected rather than silently matching.
        assert!(bech32_bit_prefix("ab").is_none());
    }

    #[test]
    fn filter_parsing_and_matching() {
        let cred = vec![0x42u8; 28];
        let addr = crate::pallas::stake_address_from_cred_bytes(&cred, true);
        let handles = vec!["Alice".to_string(), "alice-long".to_string()];

        // Empty / whitespace → no filter at all.
        assert!(delegator_filter(&None, true).is_none());
        assert!(delegator_filter(&Some("   ".into()), true).is_none());

        // A bare word is a handle substring, case-insensitively.
        let f = delegator_filter(&Some("LIC".into()), true).unwrap();
        assert!(matches!(f, DelegatorFilter::Handle(_)));
        assert!(matches_filter(&f, &cred, Some(&handles), true));
        assert!(!matches_filter(&f, &cred, None, true));

        // `$name` forces the handle mode.
        let f = delegator_filter(&Some("$alice".into()), true).unwrap();
        assert!(matches_filter(&f, &cred, Some(&handles), true));
        // …and a `$` alone is not a filter.
        assert!(delegator_filter(&Some("$".into()), true).is_none());

        // A complete address matches by credential bytes, not by string.
        let f = delegator_filter(&Some(addr.clone()), true).unwrap();
        assert!(matches!(f, DelegatorFilter::Cred(_)));
        assert!(matches_filter(&f, &cred, None, true));
        assert!(!matches_filter(&f, &vec![0x43u8; 28], None, true));

        // A partial address is a prefix match.
        let f = delegator_filter(&Some(addr[..20].to_string()), true).unwrap();
        assert!(matches!(f, DelegatorFilter::AddressPrefix { .. }));
        assert!(matches_filter(&f, &cred, None, true));
        assert!(!matches_filter(&f, &vec![0x43u8; 28], None, true));
    }
}
