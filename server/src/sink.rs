use gasket::framework::*;
use oura::framework::*;
use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::miniprotocols::Point;
use sqlx::types::Decimal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use pallas::crypto::hash::Hasher;

use crate::cip68;

use crate::event::Event;
use crate::event_bus::EventBus;
use crate::mempool::extract_tx;
use crate::model::{is_handle_policy, parse_handle_name, pool_bech32_id, TxOutput};
use crate::nftcdn::NftcdnConfig;
use crate::pallas::{
    stake_credential_bytes, stake_credential_from_address_bytes, MultiEraTxExt, PoolUpdate,
};
use crate::state::feed_index::{BlockRef, DelegationEntry};
use crate::state::{BlockUpdate, State};

/// During normal operation, persist a snapshot every this many blocks. Skipped
/// while catching up (initial sync), where periodic saves only slow it down.
const SNAPSHOT_INTERVAL: u64 = 50;

pub struct Worker;

impl Worker {
    async fn handle_reset(&self, point: &Point, stage: &Stage) -> Result<(), WorkerError> {
        let slot = point.slot_or_default();

        {
            let mut state = stage.state.write().await;
            if state.rollback(slot) {
                // Snapshot covered this slot, just truncate history. The off-chain
                // ticker/name cache isn't block-derived, so a truncated refresh would be
                // lost while its in-memory cursor stayed advanced; reset the cursors so
                // the next block re-reads from 0 and re-applies current values.
                state.pool_meta_cursor = 0;
                state.drep_meta_cursor = 0;
            } else {
                state
                    .reset(slot, &stage.genesis, stage.mainnet)
                    .await
                    .or_panic()?;
                match state.save_snapshot(
                    &stage.snapshot_path,
                    stage.snapshot_depth,
                    stage.genesis.magic,
                ) {
                    Ok(saved_slot) => info!(saved_slot, "snapshot saved after reset"),
                    Err(e) => warn!("failed to save snapshot after reset: {}", e),
                }
            }
        }

        stage.event_bus.send(Event::Rollback { slot }).await;

        Ok(())
    }

    async fn handle_apply(&self, cbor: &[u8], stage: &Stage) -> Result<(), WorkerError> {
        let apply_started = std::time::Instant::now();
        let decode_started = apply_started;
        let block = MultiEraBlock::decode(cbor).or_panic()?;
        let slot = block.slot();
        let height = block.number();
        let block_hash = block.hash().to_string();
        let (
            txs,
            produced,
            consumed,
            pool_deleg,
            drep_deleg,
            pool_updates,
            pool_retirements,
            stake_changes,
            withdrawal_changes,
            pool_id,
            pool_ticker,
            issuer_pool_hash,
            stake_change_pools,
            stake_change_dreps,
            vote_pools,
            vote_dreps,
            feed_delegations,
            drep_feed_delegations,
            new_decimals,
            handle_changes,
        ) = {
            let state = stage.state.read().await;
            let snap = state.current();
            let mut txs = Vec::new();
            // Each consumed input carries its resolved TxOutput so `apply_block`
            // can update per-address aggregates without re-looking up inputs
            // that predate the snapshot (db-sync fallback path).
            let mut consumed: Vec<((Vec<u8>, i16), TxOutput)> = Vec::new();
            let mut produced: std::collections::HashMap<(Vec<u8>, i16), TxOutput> =
                std::collections::HashMap::new();
            let mut pool_deleg: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
            let mut drep_deleg: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
            let mut pool_updates: Vec<PoolUpdate> = Vec::new();
            let mut pool_retirements: Vec<(Vec<u8>, u64)> = Vec::new();
            let mut stake_changes: Vec<(Vec<u8>, i64)> = Vec::new();
            let mut withdrawal_changes: Vec<(Vec<u8>, i64)> = Vec::new();

            // Feed index: pools (SPO) / DReps that cast a governance vote in this block.
            let mut vote_pools: std::collections::HashSet<Vec<u8>> =
                std::collections::HashSet::new();
            // DReps that voted, with how many votes each cast in this block (a tx voting on
            // several actions counts once per action) — the index only needs the key set, the
            // header's vote counters need the count.
            let mut vote_dreps: std::collections::HashMap<Vec<u8>, u32> =
                std::collections::HashMap::new();

            // CIP-68: collect decimals from reference token datums in this block
            let mut new_decimals: Vec<(String, u8)> = Vec::new();

            // ADA Handle: collect (handle_name, owner_address) for this block
            let mut handle_changes: Vec<(String, String)> = Vec::new();

            // Every previous output this block spends, fetched in ONE db round trip.
            //
            // The loop below needs each input's previous output (address, ADA, tokens) to move
            // stake and holdings. Inputs created in this block, or still in `snap.utxos`, are
            // free; the rest used to be fetched one at a time — two queries per input, awaited
            // in sequence — which is what made a resume's catch-up slow (~800 inputs across the
            // blocks it replays, ~7 ms each). `resolve_utxos_batch` matches them all with one
            // UNNEST'd statement, the same call the feed-replay path makes (`decode.rs`).
            //
            // Anything the batch doesn't return (no db handle yet, a query error) still falls
            // back to the per-input lookup below, so behaviour is unchanged — only the number
            // of round trips is.
            //
            // The block's own outputs are added as they're decoded, so this doubles as the
            // `block_utxos` map `extract_tx` resolves against — that's the second consumer of
            // these lookups (it renders each input's address/value into the SSE tx) and it has
            // the same one-at-a-time fallback. Kept separate from `produced`, which must stay
            // exactly the outputs this block *creates* (`apply_block` inserts those as UTXOs).
            let mut known_utxos: std::collections::HashMap<(Vec<u8>, i16), TxOutput> = {
                let mut in_block: std::collections::HashSet<(Vec<u8>, i16)> =
                    std::collections::HashSet::new();
                let mut spent: Vec<(Vec<u8>, i16)> = Vec::new();
                for tx in block.txs() {
                    let (inputs, outputs) = crate::pallas::effective_io(&tx);
                    let hash = tx.hash().as_ref().to_vec();
                    for i in 0..outputs.len() {
                        in_block.insert((hash.clone(), i as i16));
                    }
                    for input in &inputs {
                        spent.push((input.hash().as_ref().to_vec(), input.index() as i16));
                    }
                }
                // A tx can only spend an output made earlier in the same block, but filtering
                // after the full pass is order-independent — and dedup keeps a doubly-spent
                // key (impossible on-chain, cheap to guard) out of the array parameter.
                spent.retain(|key| {
                    !in_block.contains(key) && !snap.is_some_and(|s| s.utxos.contains_key(key))
                });
                spent.sort_unstable();
                spent.dedup();
                let db_started = std::time::Instant::now();
                let resolved = match (spent.is_empty(), state.db_handle()) {
                    (false, Some(db)) => db
                        .resolve_utxos_batch(&spent)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|(key, (address, lovelace, assets, _unspent))| {
                            let address = pallas::ledger::addresses::Address::from_bech32(&address)
                                .ok()
                                .map(|a| a.to_vec())?;
                            Some((
                                key,
                                TxOutput {
                                    lovelaces: Decimal::from(lovelace),
                                    address,
                                    assets,
                                },
                            ))
                        })
                        .collect(),
                    _ => std::collections::HashMap::new(),
                };
                stage
                    .catchup_db_us
                    .fetch_add(db_started.elapsed().as_micros() as u64, Ordering::Relaxed);
                resolved
            };

            // Feed index: collect raw delegation certs for building DelegationEntry
            struct RawDelegCert {
                tx_hash: String,
                cred_bytes: Vec<u8>,
                target_pool: Option<Vec<u8>>,
            }
            let mut raw_deleg_certs: Vec<RawDelegCert> = Vec::new();

            struct RawDrepDelegCert {
                tx_hash: String,
                cred_bytes: Vec<u8>,
                target_drep: Option<Vec<u8>>,
            }
            let mut raw_drep_deleg_certs: Vec<RawDrepDelegCert> = Vec::new();

            for tx in block.txs() {
                let hash = tx.hash();
                // A phase-2-invalid tx is recorded on-chain, but the ledger applies ONLY its
                // collateral: its regular inputs/outputs, mints, withdrawals and certificates
                // never take effect. So spend its collateral inputs and produce its collateral
                // return, and skip everything else (gated on `valid` below). Byron/pre-Alonzo
                // txs are always valid.
                let valid = tx.is_valid();
                // The inputs this tx actually spends and outputs it actually creates —
                // collateral only for a phase-2-invalid tx (see `effective_io`).
                let (inputs, outputs) = crate::pallas::effective_io(&tx);

                // Feed index: governance voters (SPO + DRep) in this tx. A phase-2-invalid
                // tx applies only its collateral, so its votes never take effect — skip them.
                if valid {
                    let (vps, vds) = crate::mempool::extract_vote_subjects(&tx);
                    vote_pools.extend(vps);
                    for drep in vds {
                        *vote_dreps.entry(drep).or_insert(0) += 1;
                    }
                }

                // Track consumed UTXOs: subtract lovelaces from stake credentials.
                for input in &inputs {
                    let key = (input.hash().as_ref().to_vec(), input.index() as i16);
                    // Check block-local UTXOs first, then in-memory state,
                    // then fall back to db-sync for pre-reset UTXOs.
                    let resolved: Option<TxOutput> = if let Some(utxo) = produced.get(&key) {
                        Some(utxo.clone())
                    } else if let Some(utxo) = snap.and_then(|s| s.utxos.get(&key)) {
                        Some(utxo.clone())
                    } else if let Some(utxo) = known_utxos.get(&key) {
                        Some(utxo.clone())
                    } else {
                        let (addr_str, lovelace, assets) = state
                            .resolve_input(input.hash().as_ref(), input.index() as i16)
                            .await;
                        addr_str
                            .and_then(|a| {
                                pallas::ledger::addresses::Address::from_bech32(&a)
                                    .ok()
                                    .map(|a| a.to_vec())
                            })
                            .map(|addr| TxOutput {
                                lovelaces: Decimal::from(lovelace),
                                address: addr,
                                assets,
                            })
                    };
                    if let Some(utxo) = resolved {
                        if let Some(cred) = stake_credential_from_address_bytes(&utxo.address) {
                            let amount: i64 = utxo
                                .lovelaces
                                .try_into()
                                .expect("lovelace value must fit i64");
                            stake_changes.push((cred.clone(), -amount));
                        }
                        consumed.push((key, utxo));
                    } else {
                        // Unresolved input (very old / byron-only): record the
                        // utxo removal key with a zero-lovelace, no-asset
                        // sentinel so the utxo map drop still happens.
                        consumed.push((
                            key,
                            TxOutput {
                                lovelaces: Decimal::ZERO,
                                address: Vec::new(),
                                assets: Vec::new(),
                            },
                        ));
                    }
                }

                txs.push(
                    extract_tx(
                        &tx,
                        &state,
                        &stage.nftcdn,
                        &known_utxos,
                        stage.mainnet,
                        &stage.genesis,
                    )
                    .await,
                );

                // CIP-68: extract decimals from reference token datums (valid txs only;
                // an invalid tx's outputs never reach the ledger).
                if valid {
                    new_decimals.extend(cip68::extract_from_tx(&tx));
                }

                // Track produced UTXOs: add lovelaces to stake credentials.
                for (idx, output) in &outputs {
                    let addr = output
                        .address()
                        .ok()
                        .map(|a| a.to_vec())
                        .unwrap_or_default();
                    let coin = output.value().coin();
                    let lovelaces = Decimal::from(coin);
                    if let Some(cred) = stake_credential_from_address_bytes(&addr) {
                        let amount: i64 =
                            lovelaces.try_into().expect("lovelace value must fit i64");
                        stake_changes.push((cred.clone(), amount));
                    }
                    let mut assets: crate::model::PolicyAssets = Vec::new();
                    for pa in output.value().assets().iter() {
                        let policy_id = pa.policy().as_ref();
                        let mut tokens: Vec<(Vec<u8>, u64)> = Vec::new();
                        for a in pa.assets().iter() {
                            if let Some(raw) = a.output_coin() {
                                tokens.push((a.name().to_vec(), raw));
                            }
                            // Detect ADA Handle tokens (classic, CIP-68, virtual)
                            if is_handle_policy(policy_id) {
                                if let Some((handle, is_virtual)) = parse_handle_name(a.name()) {
                                    let owner = if is_virtual {
                                        // Virtual: resolve from inline datum
                                        output.datum().and_then(|d| {
                                            use pallas::ledger::primitives::conway::DatumOption;
                                            match d {
                                                DatumOption::Data(data) => {
                                                    crate::model::parse_virtual_handle_address_from_datum(
                                                        &data.0,
                                                    )
                                                }
                                                _ => None,
                                            }
                                        })
                                    } else {
                                        pallas::ledger::addresses::Address::from_bytes(&addr)
                                            .ok()
                                            .map(|a| a.to_string())
                                    };
                                    if let Some(owner) = owner {
                                        handle_changes.push((handle, owner));
                                    }
                                }
                            }
                        }
                        if !tokens.is_empty() {
                            assets.push((policy_id.to_vec(), tokens));
                        }
                    }
                    let key = (hash.as_ref().to_vec(), *idx as i16);
                    let utxo = TxOutput {
                        lovelaces,
                        address: addr,
                        assets,
                    };
                    known_utxos.insert(key.clone(), utxo.clone());
                    produced.insert(key, utxo);
                }

                // An invalid tx's withdrawals and certificates never take effect — only its
                // collateral (handled above) moved. Skip the rest of the per-tx processing.
                if !valid {
                    continue;
                }

                // Track withdrawals (reduce reward balance)
                for (reward_addr, amount) in tx.withdrawals_sorted_set() {
                    if reward_addr.len() >= 29 {
                        let cred = reward_addr[1..29].to_vec();
                        withdrawal_changes.push((cred, amount as i64));
                    }
                }

                // Collect delegation certs (both raw bytes for state and full for feed index)
                let certs = tx.pool_delegation_certs();
                for (cred, target_pool) in &certs {
                    let cred_bytes = stake_credential_bytes(cred);
                    pool_deleg.push((cred_bytes.clone(), target_pool.clone()));
                    raw_deleg_certs.push(RawDelegCert {
                        tx_hash: hash.to_string(),
                        cred_bytes,
                        target_pool: target_pool.clone(),
                    });
                }

                let drep_changes = tx.drep_delegation_changes();
                for (cred_bytes, target_drep) in &drep_changes {
                    raw_drep_deleg_certs.push(RawDrepDelegCert {
                        tx_hash: hash.to_string(),
                        cred_bytes: cred_bytes.clone(),
                        target_drep: target_drep.clone(),
                    });
                }
                drep_deleg.extend(drep_changes);
                pool_updates.extend(tx.pool_updates());
                pool_retirements.extend(tx.pool_retirements());
            }

            let produced: Vec<_> = produced.into_iter().collect();

            // Extract block issuer pool hash
            let issuer_pool_hash: Option<Vec<u8>> = block
                .header()
                .issuer_vkey()
                .map(|vkey| Hasher::<224>::hash(vkey).as_ref().to_vec());

            let (pool_id, pool_ticker) = issuer_pool_hash
                .as_ref()
                .and_then(|hash| snap?.pools.get(&hex::encode(hash)))
                .map(|pool| (Some(pool_bech32_id(&pool.hash_raw)), pool.ticker.clone()))
                .unwrap_or((None, None));

            // Build feed delegation entries from pre-block state
            let mut feed_delegations: Vec<DelegationEntry> = Vec::new();
            if let Some(s) = snap {
                for cert in &raw_deleg_certs {
                    let from_pool = s.pool_delegations.get(&cert.cred_bytes);

                    // Skip same-pool re-delegation
                    if let (Some(from), Some(to)) = (from_pool, &cert.target_pool) {
                        if from.target.as_ref() == to.as_slice() {
                            continue;
                        }
                    }

                    let live_stake = s.stakes.get(&cert.cred_bytes).copied().unwrap_or(0)
                        + s.rewards.get(&cert.cred_bytes).copied().unwrap_or(0);

                    feed_delegations.push(DelegationEntry {
                        slot,
                        block_hash: block_hash.clone(),
                        block_no: height,
                        tx_hash: cert.tx_hash.clone(),
                        cred: cert.cred_bytes.clone(),
                        live_stake,
                        from: from_pool.map(|d| d.target.to_vec()),
                        to: cert.target_pool.clone(),
                    });
                }
            }

            // Build DRep feed delegation entries from pre-block state
            let mut drep_feed_delegations: Vec<DelegationEntry> = Vec::new();
            if let Some(s) = snap {
                for cert in &raw_drep_deleg_certs {
                    let from_drep = s.drep_delegations.get(&cert.cred_bytes);

                    // Skip same-drep re-delegation
                    if let (Some(from), Some(to)) = (from_drep, &cert.target_drep) {
                        if from.target.as_ref() == to.as_slice() {
                            continue;
                        }
                    }

                    let live_stake = s.stakes.get(&cert.cred_bytes).copied().unwrap_or(0)
                        + s.rewards.get(&cert.cred_bytes).copied().unwrap_or(0);

                    drep_feed_delegations.push(DelegationEntry {
                        slot,
                        block_hash: block_hash.clone(),
                        block_no: height,
                        tx_hash: cert.tx_hash.clone(),
                        cred: cert.cred_bytes.clone(),
                        live_stake,
                        from: from_drep.map(|d| d.target.to_vec()),
                        to: cert.target_drep.clone(),
                    });
                }
            }

            // Feed index: which pools / DReps had a *significant* stake change this block — a
            // single tx moving more than `active_stake / STAKE_CHANGE_DIVISOR` of the subject's
            // stake. Filtering here (rather than flagging every touched subject) keeps the index
            // to blocks that actually render, so a feed's replay doesn't fetch thousands of
            // sub-threshold blocks only to discard them. The per-tx magnitude mirrors the
            // query-time render filter (`replay.rs`), and the threshold uses the epoch-stable
            // active stake (populated in `State`), not the O(delegators) live stake.
            let (pool_mag, drep_mag) = match snap {
                Some(s) => crate::filter::block_stake_change_magnitudes(&txs, s),
                None => (
                    std::collections::HashMap::new(),
                    std::collections::HashMap::new(),
                ),
            };
            let stake_change_pools: std::collections::HashSet<Vec<u8>> = pool_mag
                .into_iter()
                .filter(|(p, m)| *m as u64 > state.pool_stake_threshold(p))
                .map(|(p, _)| p)
                .collect();
            let stake_change_dreps: std::collections::HashSet<Vec<u8>> = drep_mag
                .into_iter()
                .filter(|(d, m)| *m as u64 > state.drep_stake_threshold(d))
                .map(|(d, _)| d)
                .collect();

            (
                txs,
                produced,
                consumed,
                pool_deleg,
                drep_deleg,
                pool_updates,
                pool_retirements,
                stake_changes,
                withdrawal_changes,
                pool_id,
                pool_ticker,
                issuer_pool_hash,
                stake_change_pools,
                stake_change_dreps,
                vote_pools,
                vote_dreps,
                feed_delegations,
                drep_feed_delegations,
                new_decimals,
                handle_changes,
            )
        };
        stage.catchup_decode_us.fetch_add(
            decode_started.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );

        let timestamp = crate::mempool::slot_to_timestamp(slot, &stage.genesis);
        let epoch = State::epoch_for_slot(slot, &stage.genesis);

        // Check for epoch boundary and fetch reward deltas + DRep activity from db-sync
        #[allow(clippy::type_complexity)]
        let (reward_deltas, drep_active_until, new_active_stakes): (
            _,
            _,
            Option<(
                std::collections::HashMap<Vec<u8>, u64>,
                std::collections::HashMap<Vec<u8>, u64>,
            )>,
        ) = {
            let state = stage.state.read().await;
            let last_epoch = state.current().and_then(|s| s.last_epoch);
            if last_epoch.is_some() && last_epoch != Some(epoch) {
                info!(
                    epoch,
                    "epoch boundary detected, fetching reward deltas + drep activity + active stake"
                );
                // Refresh per-subject active stake for the new epoch (the threshold denominator).
                // Use `db()` (initializes the connection if needed) — `db_handle()` would return
                // None if this is the first db use. Never wipe the map on an empty result/error:
                // that would drop the threshold to 0 and leak every tiny change.
                let new_active = if let Some(db) = state.db().await {
                    let pools = match db.pool_active_stakes(epoch).await {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(epoch, error = %e, "epoch boundary: pool active-stake query failed");
                            Vec::new()
                        }
                    };
                    let dreps = db.drep_active_stakes(epoch).await.unwrap_or_default();
                    let p_sum: u128 = pools.iter().map(|(_, a)| (*a).max(0) as u128).sum();
                    info!(
                        epoch,
                        pools = pools.len(),
                        dreps = dreps.len(),
                        total_pool_active_stake_ada = (p_sum / 1_000_000) as u64,
                        "epoch boundary: refreshed active stakes"
                    );
                    if pools.is_empty() {
                        warn!(
                            epoch,
                            "epoch boundary: pool active-stake query empty; kept prior map"
                        );
                        None
                    } else {
                        Some((
                            pools
                                .into_iter()
                                .map(|(h, a)| (h, a.max(0) as u64))
                                .collect::<std::collections::HashMap<_, _>>(),
                            dreps
                                .into_iter()
                                .map(|(h, a)| (h, a.max(0) as u64))
                                .collect::<std::collections::HashMap<_, _>>(),
                        ))
                    }
                } else {
                    warn!(epoch, "epoch boundary: no db for active-stake refresh");
                    None
                };
                (
                    state.epoch_reward_delta(epoch).await,
                    state.drep_active_until().await,
                    new_active,
                )
            } else {
                (None, None, None)
            }
        };

        let state_started = std::time::Instant::now();
        {
            let mut state = stage.state.write().await;
            if let Some((pools, dreps)) = new_active_stakes {
                state.pool_active_stake = pools;
                state.drep_active_stake = dreps;
            }
            let block_ref = BlockRef {
                slot,
                hash: block_hash.clone(),
                number: height,
            };

            if let Some(pool_hash) = &issuer_pool_hash {
                state
                    .feed_index
                    .add_pool_minted(pool_hash.clone(), block_ref.clone());
            }

            if !stake_change_pools.is_empty() {
                state
                    .feed_index
                    .add_pool_stake_changes(stake_change_pools, block_ref.clone());
            }

            for entry in feed_delegations {
                state.feed_index.add_delegation_event(entry);
            }

            if !stake_change_dreps.is_empty() {
                state
                    .feed_index
                    .add_drep_stake_changes(stake_change_dreps, block_ref.clone());
            }

            if !vote_pools.is_empty() {
                state
                    .feed_index
                    .add_pool_votes(vote_pools, block_ref.clone());
            }

            if !vote_dreps.is_empty() {
                state
                    .feed_index
                    .add_drep_votes(vote_dreps.keys().cloned().collect(), block_ref.clone());
            }

            for entry in drep_feed_delegations {
                state.feed_index.add_drep_delegation_event(entry);
            }

            // Prune feed index entries older than 5 days
            const FEED_INDEX_WINDOW: u64 = 5 * 86400;
            let prune_boundary = slot.saturating_sub(FEED_INDEX_WINDOW);
            state.feed_index.prune(prune_boundary);

            state.apply_block(BlockUpdate {
                slot,
                block_hash: block_hash.clone(),
                epoch,
                produced,
                consumed: &consumed,
                pool_delegation_changes: &pool_deleg,
                drep_delegation_changes: &drep_deleg,
                pool_updates: &pool_updates,
                pool_retirements: &pool_retirements,
                issuer_pool_hash: issuer_pool_hash.as_deref(),
                stake_changes: &stake_changes,
                withdrawal_changes: &withdrawal_changes,
                reward_deltas: reward_deltas.as_ref(),
                drep_active_until: drep_active_until.as_ref(),
                drep_votes: &vote_dreps,
            });

            // ADA Handle: keep the resolution live. `handle_changes` are the handles produced
            // (moved/minted) this block; also gather the handle names *spent* this block so a
            // burn/revoke (spent, not re-produced) is removed rather than left stale.
            let mut consumed_handles: Vec<String> = Vec::new();
            for (_, out) in &consumed {
                for (policy, tokens) in &out.assets {
                    if is_handle_policy(policy) {
                        for (name, _qty) in tokens {
                            if let Some((handle, _is_virtual)) = parse_handle_name(name) {
                                consumed_handles.push(handle);
                            }
                        }
                    }
                }
            }
            if !handle_changes.is_empty() || !consumed_handles.is_empty() {
                if let Some(snap) = state.current_mut() {
                    snap.apply_handle_updates(&handle_changes, &consumed_handles);
                }
            }

            // CIP-68: update decimals in the latest snapshot
            if !new_decimals.is_empty() {
                if let Some(snap) = state.current_mut() {
                    for (fp, d) in new_decimals {
                        if d > 0 {
                            snap.decimals.insert(fp, d);
                        } else {
                            snap.decimals.remove(&fp);
                        }
                    }
                }
            }
        }
        stage.catchup_state_us.fetch_add(
            state_started.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );

        let tx_count = txs.len();

        stage
            .event_bus
            .send(Event::Block {
                slot,
                hash: block_hash,
                number: height,
                timestamp,
                size: cbor.len(),
                pool_id,
                pool_ticker,
                txs,
            })
            .await;

        let catchup = stage.catchup_target.load(Ordering::Relaxed);
        let mut catchup_complete = false;
        if catchup > 0 {
            // Account for this block before deciding whether we're done.
            let now_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);
            stage
                .catchup_first_us
                .compare_exchange(
                    0,
                    now_us.saturating_sub(apply_started.elapsed().as_micros() as u64),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .ok();
            stage.catchup_blocks.fetch_add(1, Ordering::Relaxed);
            stage.catchup_apply_us.fetch_add(
                apply_started.elapsed().as_micros() as u64,
                Ordering::Relaxed,
            );
            // Complete the moment we drain to the node's real tip (published by the source), not
            // when we cross the wall-clock estimate `catchup` — that estimate is normally a bit
            // ahead of the tip, so crossing it would force an extra wait for the next minted block
            // before SSE opens. `node_tip` is 0 only until the first message arrives; treat that
            // as not-yet-known and keep catching up.
            let tip = stage.node_tip.load(Ordering::Relaxed);
            // Two ways to be done, whichever comes first: the applied block reached the tip the
            // node reported, or the node has run out of blocks (`at_tip`) and we've applied the
            // last one it sent. The second is what stops us idling for the next minted block.
            let drained = stage.at_tip.load(Ordering::Relaxed)
                && slot >= stage.sent_slot.load(Ordering::Relaxed);
            if (tip > 0 && slot >= tip) || drained {
                stage.catchup_target.store(0, Ordering::Relaxed);
                stage.catching_up.store(false, Ordering::Relaxed);
                catchup_complete = true;
                // `apply_ms` is time spent applying, `wall_ms` the span from the first block
                // to this one: the difference is time the sink spent waiting for the node to
                // hand it the next block, which on mainnet means waiting for one to be minted.
                let blocks = stage.catchup_blocks.load(Ordering::Relaxed);
                let apply_ms = stage.catchup_apply_us.load(Ordering::Relaxed) / 1000;
                let decode_ms = stage.catchup_decode_us.load(Ordering::Relaxed) / 1000;
                let db_ms = stage.catchup_db_us.load(Ordering::Relaxed) / 1000;
                let state_ms = stage.catchup_state_us.load(Ordering::Relaxed) / 1000;
                let first_us = stage.catchup_first_us.load(Ordering::Relaxed);
                let wall_ms = now_us.saturating_sub(first_us) / 1000;
                info!(
                    slot,
                    height,
                    tip,
                    drained,
                    blocks,
                    apply_ms,
                    // decode = block decode + building the SSE txs (db_ms is the query inside it)
                    decode_ms,
                    db_ms,
                    state_ms,
                    wall_ms,
                    idle_ms = wall_ms.saturating_sub(apply_ms),
                    "catch-up complete"
                );
            } else if height % 1000 == 0 {
                let target = if tip > 0 { tip } else { catchup };
                let remaining = target.saturating_sub(slot) / 20;
                info!(slot, height, remaining, "catching up");
            }
        } else {
            info!(slot, height, tx_count, "apply block");
        }

        // Skip periodic snapshots while catching up — they only slow the sync.
        // Save once when catch-up completes (so a restart doesn't repeat it),
        // then every SNAPSHOT_INTERVAL blocks during normal operation.
        if catchup_complete || (catchup == 0 && height % SNAPSHOT_INTERVAL == 0) {
            // Trace steady-state map sizes + RSS (O(1) per map; brief read lock).
            if let Some(cur) = stage.state.read().await.current() {
                cur.log_sizes("periodic");
            }
            // Offload the 1.6 GB serialize + write off the sink/feeds: clone the
            // point-in-time data under a brief read lock, then serialize + write on a
            // blocking thread. The `swap` guard skips the interval if a prior save is
            // still running (so two saves never stack).
            if !stage.snapshot_saving.swap(true, Ordering::AcqRel) {
                let (snap, fi, slot) = {
                    let state = stage.state.read().await;
                    state.clone_for_save(stage.snapshot_depth)
                };
                let path = stage.snapshot_path.clone();
                let magic = stage.genesis.magic;
                let saving = stage.snapshot_saving.clone();
                tokio::task::spawn_blocking(move || {
                    match crate::state::write_snapshot(&path, &snap, &fi, magic) {
                        Ok(_) => info!(saved_slot = slot, "snapshot saved"),
                        Err(e) => warn!("failed to save snapshot: {}", e),
                    }
                    saving.store(false, Ordering::Release);
                });
            }
        }

        // Idle off-chain metadata refresh (pool tickers / DRep names) during normal
        // operation: a sub-ms MAX(id) gate, then read only rows newer than the cursor and
        // apply to the latest snapshot. Off-chain data db-sync fetches asynchronously isn't
        // in any block, so unlike decimals/handles it can't be derived per block — we poll.
        // Runs off the chain_state lock (db queries between a brief read and write guard).
        if catchup == 0 {
            let (pc, dc, db) = {
                let g = stage.state.read().await;
                (g.pool_meta_cursor, g.drep_meta_cursor, g.db_handle())
            };
            if let Some(db) = db {
                let pool = match db.max_pool_meta_id().await {
                    Ok(max) if max > pc => db.pool_ticker_updates(pc).await.ok().map(|r| (r, max)),
                    _ => None,
                };
                let drep = match db.max_drep_meta_id().await {
                    Ok(max) if max > dc => {
                        db.drep_metadata(i64::MAX, dc).await.ok().map(|d| (d, max))
                    }
                    _ => None,
                };
                if pool.is_some() || drep.is_some() {
                    let mut g = stage.state.write().await;
                    if let Some((rows, max)) = pool {
                        if let Some(snap) = g.current_mut() {
                            for (hash_raw, ticker, _) in rows {
                                if let Some(p) = snap.pools.get_mut(&hex::encode(&hash_raw)) {
                                    p.ticker = Some(ticker);
                                }
                            }
                        }
                        g.pool_meta_cursor = max;
                    }
                    if let Some((dreps, max)) = drep {
                        if let Some(snap) = g.current_mut() {
                            for (key, entry) in dreps {
                                if let Some(name) = entry.given_name {
                                    if let Some(target) = snap.dreps.get_mut(&key) {
                                        target.given_name = Some(name);
                                    }
                                }
                            }
                        }
                        g.drep_meta_cursor = max;
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(_stage: &Stage) -> Result<Self, WorkerError> {
        Ok(Self)
    }

    async fn schedule(
        &mut self,
        stage: &mut Stage,
    ) -> Result<WorkSchedule<ChainEvent>, WorkerError> {
        let msg = stage.input.recv().await.or_panic()?;
        Ok(WorkSchedule::Unit(msg.payload))
    }

    async fn execute(&mut self, unit: &ChainEvent, stage: &mut Stage) -> Result<(), WorkerError> {
        let point = unit.point();

        match unit {
            ChainEvent::Reset(point) => {
                info!("Reset to {:?}", point);
                self.handle_reset(point, stage).await?;
            }
            ChainEvent::Apply(_, Record::CborBlock(cbor)) => {
                self.handle_apply(cbor, stage).await?;
            }
            event => {
                warn!("Unexpected chain event {:?}", event);
            }
        }

        stage.ops_count.inc(1);
        stage.latest_block.set(point.slot_or_default() as i64);

        Ok(())
    }
}

#[derive(Stage)]
#[stage(name = "sink-fetcher", unit = "ChainEvent", worker = "Worker")]
pub struct Stage {
    genesis: GenesisValues,
    mainnet: bool,
    event_bus: Arc<EventBus>,
    state: Arc<RwLock<State>>,
    nftcdn: NftcdnConfig,
    snapshot_path: PathBuf,
    snapshot_depth: usize,
    catchup_target: AtomicU64,
    /// The node's tip slot, published by the source stage. Catch-up ends when an applied block
    /// reaches this, so SSE opens the instant we drain to the real tip (see the completion logic).
    node_tip: Arc<AtomicU64>,
    /// Set by the source when the node answers `Await` (nothing left to hand over), with the
    /// last slot it forwarded. Applying that slot while the flag holds means we are *at* the
    /// node's tip — the correct end of catch-up.
    at_tip: Arc<std::sync::atomic::AtomicBool>,
    sent_slot: Arc<AtomicU64>,
    /// Catch-up accounting, to tell *work* from *waiting*: blocks applied, the time spent
    /// applying them, and when the first one landed. Without this the only visible figure is
    /// the wall clock, which can't say whether the drain is slow or the chain simply hasn't
    /// produced the next block yet.
    catchup_blocks: AtomicU64,
    catchup_apply_us: AtomicU64,
    catchup_first_us: AtomicU64,
    /// Where a block's apply time actually goes: decoding + building the SSE txs (`decode`),
    /// the one batched UTXO query inside it (`db`), and mutating the state maps under the
    /// write lock (`state`). Summed over the catch-up and reported when it ends.
    catchup_decode_us: AtomicU64,
    catchup_db_us: AtomicU64,
    catchup_state_us: AtomicU64,
    /// Shared flag: set to false once catch-up is complete. SSE server waits on this.
    pub catching_up: Arc<std::sync::atomic::AtomicBool>,
    /// True while an offloaded periodic snapshot save is in flight, so a slow save can't
    /// stack a second 1.6 GB serialize+write on top of itself.
    snapshot_saving: Arc<AtomicBool>,

    pub input: MapperInputPort,

    #[metric]
    ops_count: gasket::metrics::Counter,

    #[metric]
    latest_block: gasket::metrics::Gauge,
}

/// Runtime handles + persistence config for the sink stage. Bundled so
/// `bootstrapper` takes the gasket context plus one config arg.
pub struct SinkConfig {
    pub event_bus: Arc<EventBus>,
    pub state: Arc<RwLock<State>>,
    pub nftcdn: NftcdnConfig,
    pub snapshot_path: PathBuf,
    pub snapshot_depth: usize,
    pub catchup_target: Option<u64>,
    pub node_tip: Arc<AtomicU64>,
    pub at_tip: Arc<std::sync::atomic::AtomicBool>,
    pub sent_slot: Arc<AtomicU64>,
    pub catching_up: Arc<std::sync::atomic::AtomicBool>,
}

pub fn bootstrapper(context: &Context, config: SinkConfig) -> Result<Stage, Error> {
    let SinkConfig {
        event_bus,
        state,
        nftcdn,
        snapshot_path,
        snapshot_depth,
        catchup_target,
        node_tip,
        at_tip,
        sent_slot,
        catching_up,
    } = config;
    let genesis = GenesisValues::from(context.chain.clone());
    let mainnet = genesis.magic == 764824073;
    Ok(Stage {
        genesis,
        mainnet,
        event_bus,
        state,
        nftcdn,
        snapshot_path,
        snapshot_depth,
        catchup_target: AtomicU64::new(catchup_target.unwrap_or(0)),
        node_tip,
        at_tip,
        sent_slot,
        catching_up,
        catchup_blocks: AtomicU64::new(0),
        catchup_apply_us: AtomicU64::new(0),
        catchup_first_us: AtomicU64::new(0),
        catchup_decode_us: AtomicU64::new(0),
        catchup_db_us: AtomicU64::new(0),
        catchup_state_us: AtomicU64::new(0),
        snapshot_saving: Arc::new(AtomicBool::new(false)),
        ops_count: Default::default(),
        latest_block: Default::default(),
        input: Default::default(),
    })
}
