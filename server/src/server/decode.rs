//! Block decode + event output: turn fetched block CBOR into resolved `BlockTx`es
//! (`decode_block_txs` + `resolve_block_inputs`) and serialize/resolve outgoing SSE
//! events. Used by the replay path and the live stream; shared helpers stay in `server`.
use super::*;

/// Collapse every `AssetInfo`'s precomputed token ladder down to the single
/// `tk` + `size` matching this client's negotiated rung, dropping the rest so
/// it never hits the wire. Idempotent and cheap (a slice scan, no crypto).
pub(super) fn resolve_event_assets(event: &mut crate::event::Event, size: u16) {
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

pub(super) fn serialize_event(
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
pub(super) fn decode_block_txs(
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
pub(super) async fn resolve_block_inputs(
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
