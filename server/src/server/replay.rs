//! Feed replay: fetch blocks over N2N and shape them into SSE `Event`s for a feed
//! connection's history + the `/older` infinite-scroll pages. `SubjectReplay` walks a
//! stake credential backward; `older_pool_drep` merges the keyset-paged pool/DRep sources.
//! Reaches decode / subject builders and shared server state via `super::*`.
use super::*;

/// A block to replay: pool's own block (all txs) or stake-change block (filtered).
pub(super) struct ReplayBlock {
    pub(super) slot: u64,
    pub(super) hash: String,
    pub(super) number: u64,
    /// Block's epoch — only used by the stake/address backward stake walk
    /// (`SubjectReplay`); 0 for pool/DRep replay blocks, which don't walk.
    pub(super) epoch: u64,
    pub(super) pool_id: Option<String>,
    pub(super) pool_ticker: Option<String>,
    /// If true, filter txs to only those involving pool delegators.
    pub(super) filter_by_delegators: bool,
}

/// Backward stake/delegation reconstruction for a single stake credential's feed.
/// Walks the replayed blocks newest→oldest, undoing each block's net stake change
/// (plus epoch-boundary reward accruals and off-window withdrawals that happen
/// between the address's blocks) from the current snapshot stake to recover the
/// exact pre-block `live_stake` at every displayed block. Delegation `from`/`to`
/// come from the full db history (`deleg_by_tx`), so both are correct at any age.
pub(super) struct SubjectReplay {
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
    pub(super) reward_capsules: Vec<(u64, Vec<crate::event::RewardRow>)>,
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
    pub(super) fn cursor(&self) -> (u64, u64, i64) {
        (self.last_slot, self.last_epoch, self.running)
    }
}

/// Fetch replay blocks via N2N and send as SSE events. Newest-first order.
/// Shared SSE transport + config for replay sends, reused across the per-feed
/// branches in `filtered_events`. Built once; the per-call inputs (blocks,
/// delegators, filter, deleg_info, threshold) stay separate arguments.
pub(super) struct ReplaySse<'a> {
    pub(super) sender: &'a Sender<Result<SseEvent, Infallible>>,
    pub(super) nftcdn: &'a NftcdnConfig,
    pub(super) genesis: &'a GenesisConfig,
    pub(super) chain_state: &'a RwLock<State>,
    pub(super) n2n_addr: SocketAddr,
    pub(super) magic: u64,
    pub(super) mainnet: bool,
    pub(super) size: u16,
}

/// Build per-tx delegation info for a single stake credential from the feed
/// index, keyed by tx hash and ready to inject into replayed blocks on
/// stake/address feeds. The pool and DRep delegation events of a tx are merged
/// into one `DelegationInfo` (a tx may change both at once). `from`/`to` labels
/// are resolved against the current snapshot. Returns empty if the credential
/// has no delegation events in the (5-day) feed-index window.
pub(super) fn build_stake_deleg_info(
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
pub(super) async fn build_subject_replay(
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
pub(super) struct ReplayCtx<'a> {
    nftcdn: &'a NftcdnConfig,
    genesis: &'a GenesisConfig,
    chain_state: &'a RwLock<State>,
    mainnet: bool,
}

/// Per-feed replay parameters, constant across every block of one replay: the
/// credential set to filter by, the feed filter, the tx_hash → delegations overlay to
/// inject, and the minimum stake change a pool/DRep feed shows.
pub(super) struct ReplayParams<'a> {
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
pub(super) async fn process_replay_block(
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
                    || feed_filter.matches_vote(tx)
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
pub(super) async fn send_replay_blocks(
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
pub(super) async fn send_filtered_snapshot(
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

/// Categories for feed index replay actions, by priority.
pub(super) enum SlotAction {
    PoolMinted(BlockRef),
    StakeChange(BlockRef),
}

#[derive(serde::Deserialize)]
pub(super) struct OlderQuery {
    /// Stake/address feeds: fetch blocks strictly older than this slot. Absent for
    /// pool/DRep feeds, which page by per-source keyset id.
    before: Option<u64>,
    /// Walk anchor from the previous page (stake feeds): the pre-block stake at
    /// `before`'s block, as a string (can exceed JS MAX_SAFE_INTEGER).
    stake: Option<String>,
    epoch: Option<u64>,
    /// Pool/DRep keyset cursor: the previous page's per-source min row id. Fetch rows
    /// with `src.id < this`; absent on the first older page (queries from the tip).
    block_id: Option<i64>,
    vote_id: Option<i64>,
    deleg_id: Option<i64>,
    dpr: Option<f64>,
}

/// Pagination cursor for the next (older) page. `slot`/`stake`/`epoch` are the
/// stake/address (slot-walk) cursor; `*_id` are the pool/DRep per-source keyset cursor.
#[derive(serde::Serialize)]
pub(super) struct OlderCursor {
    #[serde(skip_serializing_if = "Option::is_none")]
    slot: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stake: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vote_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deleg_id: Option<i64>,
}

#[derive(serde::Serialize)]
pub(super) struct OlderResponse {
    blocks: Vec<crate::event::Event>,
    /// `None` ⇒ reached the address's first transaction (stop paginating).
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<OlderCursor>,
}

/// Infinite-scroll pagination: blocks older than `before` for a stake/address feed,
/// continuing the backward stake walk from the client's cursor. Mirrors the SSE
/// replay (reuses `process_replay_block`) but returns JSON. Stake/Address only.
pub(super) async fn older_blocks(
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

    // Pool/DRep feeds paginate by per-source keyset id (blocks/votes/delegations),
    // merged by slot — a different shape from the stake/address slot-walk below.
    if matches!(
        &filter,
        filter::FeedFilter::Pool(_) | filter::FeedFilter::DRep(_)
    ) {
        return older_pool_drep(&state, &db, &filter, &query, size, limit).await;
    }

    let before = query.before.ok_or(StatusCode::BAD_REQUEST)?;
    let (hash_raw, blocks) = match &filter {
        filter::FeedFilter::Stake(payload) => (
            Some(payload.clone()),
            db.stake_recent_blocks(payload, before as i64, limit).await,
        ),
        filter::FeedFilter::Address(addr) => (
            stake_hash_raw_of(addr, state.mainnet),
            db.address_recent_blocks(addr, before as i64, limit).await,
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
            slot: Some(slot),
            epoch: Some(epoch),
            stake: Some(stake.to_string()),
            block_id: None,
            vote_id: None,
            deleg_id: None,
        })
    } else {
        // Address feeds (no walk): slot-only cursor at the oldest block.
        replay_blocks
            .iter()
            .map(|b| b.slot)
            .min()
            .map(|slot| OlderCursor {
                slot: Some(slot),
                epoch: None,
                stake: None,
                block_id: None,
                vote_id: None,
                deleg_id: None,
            })
    };

    Ok(axum::Json(OlderResponse {
        blocks: events,
        cursor,
    }))
}

/// Infinite-scroll pagination for a pool/DRep feed: merge the subject's minted blocks
/// (pool only), governance votes and gained delegations — each paged by its own keyset
/// id (`src.id < cursor`, an index-only top-K, sub-ms at any depth) — into one
/// slot-ordered page, replay them over N2N, and return the next per-source cursor
/// (`None` once every source is exhausted).
pub(super) async fn older_pool_drep(
    state: &AppState,
    db: &crate::state::DbSync,
    filter: &filter::FeedFilter,
    query: &OlderQuery,
    size: u16,
    limit: i64,
) -> Result<axum::Json<OlderResponse>, StatusCode> {
    use crate::state::{DelegationFill, FillBlock};

    let block_before = query.block_id.unwrap_or(i64::MAX);
    let vote_before = query.vote_id.unwrap_or(i64::MAX);
    let deleg_before = query.deleg_id.unwrap_or(i64::MAX);

    // Query each applicable source (off the lock — db handle already cloned).
    let (blocks, votes, delegs): (Vec<FillBlock>, Vec<FillBlock>, Vec<DelegationFill>) =
        match filter {
            filter::FeedFilter::Pool(hash) => (
                db.pool_recent_blocks(hash, block_before, limit)
                    .await
                    .unwrap_or_default(),
                db.pool_recent_votes(hash, vote_before, limit)
                    .await
                    .unwrap_or_default(),
                db.pool_recent_delegations(hash, deleg_before, limit)
                    .await
                    .unwrap_or_default(),
            ),
            filter::FeedFilter::DRep(bytes) => (
                Vec::new(),
                db.drep_recent_votes(bytes, vote_before, limit)
                    .await
                    .unwrap_or_default(),
                db.drep_recent_delegations(bytes, deleg_before, limit)
                    .await
                    .unwrap_or_default(),
            ),
            _ => return Err(StatusCode::BAD_REQUEST),
        };

    // Merge the sources into one slot-desc page, tagged by source (0=block, 1=vote,
    // 2=deleg) so we can advance a per-source keyset cursor.
    struct Row {
        slot: u64,
        id: i64,
        src: u8,
        hash: String,
        number: u64,
        deleg: Option<DelegationFill>,
    }
    let mut rows: Vec<Row> = Vec::with_capacity(blocks.len() + votes.len() + delegs.len());
    for b in &blocks {
        rows.push(Row {
            slot: b.slot,
            id: b.id,
            src: 0,
            hash: b.block_hash.clone(),
            number: b.block_no,
            deleg: None,
        });
    }
    for b in &votes {
        rows.push(Row {
            slot: b.slot,
            id: b.id,
            src: 1,
            hash: b.block_hash.clone(),
            number: b.block_no,
            deleg: None,
        });
    }
    for d in &delegs {
        rows.push(Row {
            slot: d.slot,
            id: d.id,
            src: 2,
            hash: d.block_hash.clone(),
            number: d.block_no,
            deleg: Some(d.clone()),
        });
    }
    // Newest first; id breaks ties within a slot.
    rows.sort_by(|a, b| b.slot.cmp(&a.slot).then(b.id.cmp(&a.id)));
    let total_returned = rows.len();
    rows.truncate(limit as usize);

    // Per-source cursor = the min displayed id for that source; if a source put nothing
    // on this page (its rows are older, shown next page) keep its incoming cursor.
    let min_id = |src: u8| rows.iter().filter(|r| r.src == src).map(|r| r.id).min();
    let block_cursor = min_id(0).unwrap_or(block_before);
    let vote_cursor = min_id(1).unwrap_or(vote_before);
    let deleg_cursor = min_id(2).unwrap_or(deleg_before);

    // More older pages exist if any source was capped at `limit`, or returned rows we
    // didn't fit on this page.
    let has_more = blocks.len() as i64 == limit
        || votes.len() as i64 == limit
        || delegs.len() as i64 == limit
        || total_returned > rows.len();

    if rows.is_empty() {
        return Ok(axum::Json(OlderResponse {
            blocks: vec![],
            cursor: None,
        }));
    }

    // Resolve delegation labels + build the tx_hash -> DelegationInfo overlay and the
    // slot_map (PoolMinted wins over vote/deleg on a shared slot) under one short lock.
    let (deleg_info, slot_map, to_pool_id, to_ticker) = {
        let guard = state.chain_state.read().await;
        let snap = guard.current();
        let (to_pool_id, to_ticker, to_drep_id, to_drep_name) = match filter {
            filter::FeedFilter::Pool(hash) => {
                let ticker = snap
                    .and_then(|s| s.pools.get(&hex::encode(hash)))
                    .and_then(|p| p.ticker.clone());
                (Some(pool_bech32_id(hash)), ticker, None, None)
            }
            filter::FeedFilter::DRep(bytes) => {
                let name = match bytes.first() {
                    Some(0x02) => Some("Always Abstain".to_string()),
                    Some(0x03) => Some("Always No Confidence".to_string()),
                    _ => snap
                        .and_then(|s| s.dreps.get(bytes.as_slice()))
                        .and_then(|d| d.given_name.clone()),
                };
                (None, None, Some(drep_bech32_id(bytes)), name)
            }
            _ => (None, None, None, None),
        };
        let mut deleg_info: HashMap<String, Vec<DelegationInfo>> = HashMap::new();
        let mut slot_map: HashMap<u64, SlotAction> = HashMap::new();
        for r in &rows {
            let block_ref = BlockRef {
                slot: r.slot,
                hash: r.hash.clone(),
                number: r.number,
            };
            if r.src == 0 {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::PoolMinted(block_ref));
            } else {
                slot_map
                    .entry(r.slot)
                    .or_insert(SlotAction::StakeChange(block_ref));
                if let Some(d) = &r.deleg {
                    if !deleg_info.contains_key(&d.tx_hash) {
                        let live_stake = snap
                            .map(|s| {
                                s.stakes.get(&d.cred).copied().unwrap_or(0)
                                    + s.rewards.get(&d.cred).copied().unwrap_or(0)
                            })
                            .unwrap_or(0);
                        deleg_info
                            .entry(d.tx_hash.clone())
                            .or_default()
                            .push(DelegationInfo {
                                stake_address: crate::pallas::stake_address_from_cred_bytes(
                                    &d.cred,
                                    state.mainnet,
                                ),
                                from_pool_id: None,
                                from_ticker: None,
                                to_pool_id: to_pool_id.clone(),
                                to_ticker: to_ticker.clone(),
                                from_drep_id: None,
                                from_drep_name: None,
                                to_drep_id: to_drep_id.clone(),
                                to_drep_name: to_drep_name.clone(),
                                live_stake,
                            });
                    }
                }
            }
        }
        (deleg_info, slot_map, to_pool_id, to_ticker)
    };

    let mut replay_blocks: Vec<ReplayBlock> = slot_map
        .into_values()
        .map(|action| match action {
            SlotAction::PoolMinted(r) => ReplayBlock {
                slot: r.slot,
                hash: r.hash,
                number: r.number,
                epoch: 0,
                pool_id: to_pool_id.clone(),
                pool_ticker: to_ticker.clone(),
                filter_by_delegators: false,
            },
            SlotAction::StakeChange(r) => ReplayBlock {
                slot: r.slot,
                hash: r.hash,
                number: r.number,
                epoch: 0,
                pool_id: None,
                pool_ticker: None,
                filter_by_delegators: true,
            },
        })
        .collect();
    replay_blocks.sort_by(|a, b| b.slot.cmp(&a.slot));

    // Replay each block over N2N. Vote blocks are kept by `matches_vote`, delegation
    // blocks by the injected overlay — so an empty delegator set / 0 threshold is fine.
    let delegators = imbl::hashset::HashSet::new();
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
        feed_filter: filter,
        deleg_info: &deleg_info,
        stake_threshold: 0,
    };
    let mut events = Vec::new();
    for block in &replay_blocks {
        if let Some(mut ev) = process_replay_block(&mut client, &ctx, block, &params, None).await {
            resolve_event_assets(&mut ev, size);
            events.push(ev);
        }
    }
    let _ = client.abort().await;

    let is_pool = matches!(filter, filter::FeedFilter::Pool(_));
    // A cursor still at i64::MAX means "from the tip" (that source contributed nothing
    // to this page). Omit it rather than serialize a value beyond JS's safe-integer
    // range — the client round-trips the cursor through a JSON number, and MAX would
    // round up past i64::MAX and 400 on the next request. Absent == MAX server-side.
    let keyset = |id: i64| (id != i64::MAX).then_some(id);
    let cursor = has_more.then(|| OlderCursor {
        slot: None,
        epoch: None,
        stake: None,
        block_id: is_pool.then(|| keyset(block_cursor)).flatten(),
        vote_id: keyset(vote_cursor),
        deleg_id: keyset(deleg_cursor),
    });

    Ok(axum::Json(OlderResponse {
        blocks: events,
        cursor,
    }))
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
}
