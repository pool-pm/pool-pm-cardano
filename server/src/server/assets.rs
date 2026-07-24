//! Asset REST endpoints (`/api/asset`, `/api/policy`, `/api/assets`). Shared asset-tile
//! helpers (build_owned_tile, row_to_asset, decode_asset_name, POLICY_THUMB_PX, …) and
//! the is_valid_* validators stay in `server` — reached here via `super::*`.
use super::*;

#[derive(serde::Serialize)]
pub(super) struct AssetMedia {
    src: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    media_type: Option<String>,
    name: String,
}

#[derive(serde::Serialize)]
pub(super) struct AssetMediaResponse {
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

/// Resolve an asset's displayable media via NFTCDN. Fetches the (server-signed)
/// `/metadata`, then returns ready-signed URLs: one entry per `metadata.files`
/// entry when present (served from `/files/{i}/`), otherwise a single full-res
/// `/preview`. mediaType is passed through so the frontend media player can pick
/// the right renderer.
pub(super) async fn asset_media(
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

#[derive(serde::Deserialize)]
pub(super) struct PolicyQuery {
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
pub(super) struct AssetsResponse {
    assets: Vec<AssetItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<i64>,
    has_more: bool,
}

/// One policy's tile on the owned-assets grid: its held-asset `count` and up to
/// `GROUP_SAMPLES` sample tiles for the stacked-card thumbnail. A `count` of 1
/// renders as a plain asset tile on the frontend.
#[derive(serde::Serialize)]
pub(super) struct AssetGroup {
    policy: String,
    count: usize,
    samples: Vec<AssetItem>,
}

#[derive(serde::Serialize)]
pub(super) struct GroupsResponse {
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

/// List a policy's assets, most-recently-first-minted first, keyset-paginated on
/// `multi_asset.id` (see `DbSync::assets_by_policy`). Returns ready-signed nftcdn
/// preview URLs — a `src` plus a multi-rung `srcset` so the browser picks the DPR
/// rung — meaning the frontend needs no signing key or subdomain. Stateless
/// db-sync read: no SSE, no in-memory state, no rollback path.
pub(super) async fn policy_assets(
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

/// Assets owned by a payment address (`addr1…`) or stake credential (`stake1…`),
/// **grouped by policy** — one tile per policy with its held `count` and up to
/// `GROUP_SAMPLES` sample tiles (the frontend renders a stacked-card thumbnail and
/// drills into `/{subject}/assets/{policy}`). Served from the in-memory
/// `asset_holdings` map (no db scan); CIP-68 reference NFTs are *not* filtered — owned
/// listings show what the wallet actually holds. `cursor` is an integer offset into the
/// `(policy, name)`-sorted policy list. Only `Address`/`Stake` filters; others 400.
pub(super) async fn owned_assets(
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
pub(super) async fn owned_assets_by_policy(
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
