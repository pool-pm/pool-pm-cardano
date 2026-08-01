//! Recognize token creation and destruction.
//!
//! A mint is invisible in a tx's inputs and outputs: the new token simply appears in an
//! output, indistinguishable from one that was transferred. Only the mint field says it
//! came into existence — and a burn leaves no trace in the outputs at all, so without
//! this a burn reads as a plain self-transfer.
//!
//! The assets themselves aren't repeated here. A minted token is already in an output
//! (with its thumbnail and quantity), so the annotation carries only what the outputs
//! can't say: that this was a mint, how much came or went, and under which policies —
//! which is what lets the client name the app behind a known policy.

use pallas::ledger::traverse::MultiEraTx;

use crate::event::{MintInfo, TxAnnotation};

/// The mint/burn summary of `tx`, or `None` if it neither creates nor destroys tokens.
///
/// A single tx can do both — swapping a CIP-68 reference token is a burn plus a mint —
/// so both counts are reported rather than picking a winner.
pub fn extract_mint(tx: &MultiEraTx<'_>) -> Option<TxAnnotation> {
    let mut minted = 0u32;
    let mut burned = 0u32;
    let mut fingerprints = Vec::new();
    let mut policies = Vec::new();

    for policy_assets in tx.mints() {
        let policy = policy_assets.policy();
        let mut touched = false;
        for asset in policy_assets.assets() {
            // `mint_coin` is signed: positive creates, negative destroys. A zero delta is
            // legal CBOR and means nothing happened, so it counts as neither.
            match asset.mint_coin() {
                Some(q) if q > 0 => {
                    minted += 1;
                    if fingerprints.len() < MAX_FINGERPRINTS {
                        fingerprints.push(crate::model::asset_fingerprint(
                            policy.as_ref(),
                            asset.name(),
                        ));
                    }
                }
                Some(q) if q < 0 => burned += 1,
                _ => continue,
            }
            touched = true;
        }
        if touched {
            policies.push(hex::encode(policy.as_ref()));
        }
    }

    if minted == 0 && burned == 0 {
        return None;
    }
    Some(TxAnnotation::Mint(MintInfo {
        minted,
        burned,
        fingerprints,
        policies,
    }))
}

/// The client only shows a handful of thumbnails, and a collection drop can mint
/// thousands in one tx — so the fingerprint list is capped while the counts stay exact.
const MAX_FINGERPRINTS: usize = 32;
