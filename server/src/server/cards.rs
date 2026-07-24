//! Server-rendered Open Graph / Twitter social cards. nginx routes only crawler
//! User-Agents to `og_page`; humans get the SPA. The pure card model + HTML
//! renderer + formatters live in `crate::og`.
use super::*;

/// axum fallback: a social-card HTML document for any page path. The Host header gives the
/// absolute base for `og:url` / `og:image` (works across pool.pm / preprod / preview).
pub(super) async fn og_page(
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
            let assets = snap.map(|s| s.stake_asset_count(cred)).unwrap_or(0);
            let title = match handle {
                Some(h) => format!("${h}'s stake"),
                None => og::short_id(&filter.feed_id()),
            };
            (
                title,
                // Balance + asset count (distinct assets across all the credential's addresses).
                og::join(&[
                    og::fmt_ada(balance + rewards),
                    format!("{} assets", og::commas(assets as i64)),
                ]),
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
            let title = match handle {
                Some(h) => format!("${h}"),
                None => og::short_id(addr),
            };
            (
                title,
                // Balance + asset count.
                og::join(&[
                    og::fmt_ada(balance),
                    format!("{} assets", og::commas(assets as i64)),
                ]),
            )
        }
    };
    if owned {
        title.push_str(" assets");
    }
    og::Card::branded(base_url, title, description)
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
