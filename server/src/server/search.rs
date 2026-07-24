//! Pool-ticker / DRep-name search and ADA Handle resolution
//! (`/api/search`, `/api/handle`).
use super::*;

#[derive(serde::Deserialize)]
pub(super) struct SearchQuery {
    q: String,
}

#[derive(serde::Serialize)]
pub(super) struct SearchResult {
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
pub(super) async fn search(
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
    // Rank: score, then delegator count, then live stake — all descending. Duplicate
    // tickers / DRep names (not unique on-chain) tie on score, so the bigger one leads.
    // Delegator count is an O(1) set length → cheap for every match. Live stake is
    // O(delegators), so it's computed only within a run that already ties on both score
    // and delegator count (rare, tiny) — never for every match.
    let deleg_count = |hit: &Hit| -> usize {
        match hit {
            Hit::Pool(p) => snap.pool_delegators.get(&p.hash_raw).map_or(0, |d| d.len()),
            Hit::DRep(d) => snap
                .drep_delegators
                .get(&d.hash_bytes)
                .map_or(0, |d| d.len()),
        }
    };
    let live_stake = |hit: &Hit| -> i64 {
        match hit {
            Hit::Pool(p) => State::pool_live_stake(&snap, &p.hash_raw).unwrap_or(0),
            Hit::DRep(d) => State::drep_live_stake(&snap, &d.hash_bytes).unwrap_or(0),
        }
    };
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| deleg_count(&b.1).cmp(&deleg_count(&a.1)))
    });
    // Break the (rare) remaining score+delegators ties by live stake, descending.
    let mut i = 0;
    while i < scored.len() {
        let mut j = i + 1;
        while j < scored.len()
            && scored[j].0.total_cmp(&scored[i].0) == std::cmp::Ordering::Equal
            && deleg_count(&scored[j].1) == deleg_count(&scored[i].1)
        {
            j += 1;
        }
        if j - i > 1 {
            scored[i..j].sort_by(|a, b| live_stake(&b.1).cmp(&live_stake(&a.1)));
        }
        i = j;
    }
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
pub(super) struct HandleAddress {
    address: String,
}

/// Resolve an exact ADA Handle name to its holder's payment address — the deterministic
/// lookup behind the `pool.pm/$handle` URL redirect (the fuzzy `$`-prefixed `/api/search`
/// stays the search-dropdown path). The stored handle name carries no `$` (it's just the
/// display sigil), so a single leading `$` is stripped if the caller included it; matching is
/// case-insensitive against `address_by_handle` (handle name → holder address, kept live by
/// the sink). `404` if no such handle — the frontend renders its Not Found page.
pub(super) async fn resolve_handle(
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

#[cfg(test)]
mod tests {
    use super::*;

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
