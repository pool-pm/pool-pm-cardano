//! Recognize token creation and destruction.
//!
//! A mint is invisible in a tx's inputs and outputs: the new token simply appears in an
//! output, indistinguishable from one that was transferred. Only the mint field says it
//! came into existence — and a burn leaves no trace in the outputs at all, so without
//! this a burn reads as a plain self-transfer.
//!
//! The assets are carried on the annotation rather than left to be found in the outputs.
//! A burn has no output to find, and "BURNED 1 TOKEN" says nothing a reader can act on:
//! the name and the thumbnail are the point.

use pallas::ledger::traverse::MultiEraTx;

use crate::event::{policy_assets_to_info, AssetInfo, MintInfo, TxAnnotation};
use crate::model::PolicyAssets;
use crate::nftcdn::NftcdnConfig;

/// The client shows a handful of thumbnails and a collection drop mints thousands in one
/// tx, so the asset list is capped while the counts stay exact.
const MAX_ASSETS: usize = 32;

/// The mint/burn summary of `tx`, or `None` if it neither creates nor destroys tokens.
///
/// A single tx can do both — swapping a CIP-68 reference token is a burn plus a mint —
/// so both sides are reported rather than picking a winner.
pub fn extract_mint(
    tx: &MultiEraTx<'_>,
    nftcdn: &NftcdnConfig,
    decimals_of: impl Fn(&str) -> u8,
) -> Option<TxAnnotation> {
    let mut minted = 0u32;
    let mut burned = 0u32;
    let mut policies = Vec::new();
    // Grouped like a UTXO's assets so the display conversion is the shared one.
    let mut created: PolicyAssets = Vec::new();
    let mut destroyed: PolicyAssets = Vec::new();

    for policy_assets in tx.mints() {
        let policy = policy_assets.policy().as_ref().to_vec();
        let mut touched = false;
        for asset in policy_assets.assets() {
            // `mint_coin` is signed: positive creates, negative destroys. A zero delta is
            // legal CBOR and means nothing happened, so it counts as neither.
            let (side, count, quantity) = match asset.mint_coin() {
                Some(q) if q > 0 => (&mut created, &mut minted, q as u64),
                Some(q) if q < 0 => (&mut destroyed, &mut burned, q.unsigned_abs()),
                _ => continue,
            };
            *count += 1;
            touched = true;
            if side.iter().map(|(_, tokens)| tokens.len()).sum::<usize>() < MAX_ASSETS {
                push_asset(side, &policy, asset.name(), quantity);
            }
        }
        if touched {
            policies.push(hex::encode(&policy));
        }
    }

    if minted == 0 && burned == 0 {
        return None;
    }
    let to_info = |assets: &PolicyAssets| -> Vec<AssetInfo> {
        policy_assets_to_info(
            assets,
            |fp| decimals_of(fp),
            |fp| nftcdn.compute_ladder(fp, "preview"),
        )
    };
    Some(TxAnnotation::Mint(MintInfo {
        minted,
        burned,
        created: to_info(&created),
        destroyed: to_info(&destroyed),
        policies,
    }))
}

/// Add one token to a policy-grouped list, reusing the policy's entry when it's already
/// there (the same shape `PolicyAssets` has everywhere else).
fn push_asset(assets: &mut PolicyAssets, policy: &[u8], name: &[u8], quantity: u64) {
    match assets.iter_mut().find(|(p, _)| p == policy) {
        Some((_, tokens)) => tokens.push((name.to_vec(), quantity)),
        None => assets.push((policy.to_vec(), vec![(name.to_vec(), quantity)])),
    }
}
