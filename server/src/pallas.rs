use pallas::crypto::hash::Hasher;
use pallas::ledger::primitives::{alonzo, conway, Metadatum, StakeCredential};
use pallas::ledger::traverse::{MultiEraCert, MultiEraInput, MultiEraOutput, MultiEraTx};

use crate::event::{CatalystInfo, MetadataEntry};

/// CIP-36/CIP-15 Catalyst voting registration and its separate witness label.
/// Surfaced as a structured `CatalystInfo` (see `extract_catalyst`), not a text line.
const CATALYST_REGISTRATION: u64 = 61284;
const CATALYST_WITNESS: u64 = 61285;

/// A tx's metadata, label by label, as data rather than as rendered text.
///
/// The values travel intact so the client decides what any of it means — a metadata
/// schema then costs a frontend deploy, not a server restart, the same reasoning that
/// sends datums raw. Flattening here used to discard everything but the shape: label 1
/// carries `{"timestamp": …, "absolute_slot": …}` on ~9,800 txs a month and the server
/// sent the two key names, having thrown the values away.
///
/// Catalyst's two labels are omitted: they're surfaced structurally as `CatalystInfo`,
/// and a second raw copy would only be rendered twice.
pub fn extract_tx_metadata(tx: &MultiEraTx<'_>) -> Option<Vec<MetadataEntry>> {
    let metadata = tx.metadata();
    let mut entries: Vec<(u64, &Metadatum)> = metadata.collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort_by_key(|(label, _)| *label);
    let out: Vec<MetadataEntry> = entries
        .into_iter()
        .filter(|(label, _)| !matches!(*label, CATALYST_REGISTRATION | CATALYST_WITNESS))
        .map(|(label, datum)| MetadataEntry {
            label,
            value: metadatum_to_json(datum),
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// A `Metadatum` as JSON, shaped so nothing is lost and nothing is guessed.
///
/// Text and integers map to their JSON counterparts. Bytes become `{"bytes": "…"}` and
/// maps `{"map": [{"k": …, "v": …}]}` — tagged rather than flattened, because metadata
/// keys aren't always text and a byte string isn't distinguishable from a string once
/// it's been rendered as one. Integers keep JSON numbers: metadata integers are bounded
/// by the ledger to 64 bits, and every one seen in practice is a count or a timestamp.
fn metadatum_to_json(datum: &Metadatum) -> serde_json::Value {
    use serde_json::{json, Value};
    match datum {
        Metadatum::Int(i) => json!(i128::from(*i)),
        Metadatum::Bytes(b) => json!({ "bytes": hex::encode(b.as_slice()) }),
        Metadatum::Text(t) => Value::String(t.clone()),
        Metadatum::Array(items) => Value::Array(items.iter().map(metadatum_to_json).collect()),
        Metadatum::Map(entries) => json!({
            "map": entries
                .iter()
                .map(|(k, v)| json!({ "k": metadatum_to_json(k), "v": metadatum_to_json(v) }))
                .collect::<Vec<_>>()
        }),
    }
}

/// The output's datum as hex, for script addresses only.
///
/// Gated on the address being a script because that's where protocol datums live, and
/// sending every datum on the chain would be paying for data no reader can use. Measured
/// over 200 blocks, script datums are ~360 bytes a block raw — around 1% of the feed.
///
/// An output can carry its datum inline or reference it by hash, and which a protocol
/// picks decides whether its orders are readable at all: WingRiders publishes 955 orders
/// by hash for every 1 inline. But the datum is nearly always in the same transaction's
/// witness set regardless — 155 of 156 sampled — so a hash is resolved there rather than
/// given up on.
pub fn datum_hex(
    tx: &MultiEraTx<'_>,
    output: &pallas::ledger::traverse::MultiEraOutput<'_>,
    address: &str,
) -> Option<String> {
    use pallas::ledger::primitives::conway::DatumOption;
    if !address.starts_with("addr1w") && !address.starts_with("addr1z") {
        return None;
    }
    match output.datum()? {
        // `KeepRaw` hands back the bytes as they appeared on chain, so nothing is
        // re-encoded and the client sees exactly what the protocol wrote.
        DatumOption::Data(data) => Some(hex::encode(data.0.raw_cbor())),
        DatumOption::Hash(hash) => tx
            .plutus_data()
            .iter()
            .find(|data| Hasher::<256>::hash(data.raw_cbor()) == hash)
            .map(|data| hex::encode(data.raw_cbor())),
    }
}

/// Extract a CIP-36/CIP-15 Catalyst voting registration (label 61284). The
/// registrant's stake address is `blake2b-224(staking vkey)` (field `2`) built into
/// a reward address. `live_stake` is left `None` (filled by the stake-feed walk).
pub fn extract_catalyst(tx: &MultiEraTx<'_>, mainnet: bool) -> Option<CatalystInfo> {
    let metadata = tx.metadata();
    let Metadatum::Map(entries) = metadata.find(CATALYST_REGISTRATION)? else {
        return None;
    };
    // Field 2 = the staking public key (32-byte ed25519 vkey).
    let vkey = entries.iter().find_map(|(k, v)| match (k, v) {
        (Metadatum::Int(i), Metadatum::Bytes(b)) if i128::from(*i) == 2 => Some(b),
        _ => None,
    })?;
    let cred = Hasher::<224>::hash(vkey);
    Some(CatalystInfo {
        stake_address: stake_address_from_cred_bytes(cred.as_ref(), mainnet),
        live_stake: None,
    })
}

pub type DrepDelegationChange = (Vec<u8>, Option<Vec<u8>>);

/// (operator_hash, pledge, cost, margin_numerator, margin_denominator)
pub type PoolUpdate = (Vec<u8>, u64, u64, u64, u64);

/// (full StakeCredential, Some(pool_hash)) for delegation,
/// (full StakeCredential, None) for deregistration.
pub type PoolDelegationCert = (StakeCredential, Option<Vec<u8>>);

pub fn stake_credential_bytes(cred: &StakeCredential) -> Vec<u8> {
    match cred {
        StakeCredential::AddrKeyhash(h) => h.as_ref().to_vec(),
        StakeCredential::ScriptHash(h) => h.as_ref().to_vec(),
    }
}

/// Extract the 28-byte stake credential from raw address bytes.
/// Works for base addresses (types 0-3) and reward addresses (types 14-15).
pub fn stake_credential_from_address_bytes(addr: &[u8]) -> Option<Vec<u8>> {
    if addr.is_empty() {
        return None;
    }
    let addr_type = addr[0] >> 4;
    match addr_type {
        // Base addresses: 1 header + 28 payment + 28 stake
        0..=3 if addr.len() >= 57 => Some(addr[29..57].to_vec()),
        // Reward addresses: 1 header + 28 stake
        14 | 15 if addr.len() >= 29 => Some(addr[1..29].to_vec()),
        _ => None,
    }
}

/// Extract the 28-byte stake credential of an address given as a bech32 string (base or reward
/// address), or None for enterprise/pointer/byron/unparseable addresses without a stake part.
pub fn stake_credential_from_bech32(address: &str) -> Option<Vec<u8>> {
    let bytes = pallas::ledger::addresses::Address::from_bech32(address)
        .ok()?
        .to_vec();
    stake_credential_from_address_bytes(&bytes)
}

pub fn stake_address_bech32(cred: &StakeCredential, mainnet: bool) -> String {
    use bech32::{Bech32, Hrp};
    let (header, hrp) = match (cred, mainnet) {
        (StakeCredential::AddrKeyhash(_), true) => (0xe1u8, "stake"),
        (StakeCredential::AddrKeyhash(_), false) => (0xe0u8, "stake_test"),
        (StakeCredential::ScriptHash(_), true) => (0xf1u8, "stake"),
        (StakeCredential::ScriptHash(_), false) => (0xf0u8, "stake_test"),
    };
    let hash = stake_credential_bytes(cred);
    let mut payload = Vec::with_capacity(29);
    payload.push(header);
    payload.extend_from_slice(&hash);
    bech32::encode::<Bech32>(Hrp::parse(hrp).unwrap(), &payload).unwrap()
}

/// Build a bech32 stake address from raw 28-byte credential bytes.
/// Assumes key-based credential (0xe1/0xe0 header). Used when we only
/// have the raw bytes (e.g. from drep_delegation_changes) without the
/// StakeCredential enum.
pub fn stake_address_from_cred_bytes(cred: &[u8], mainnet: bool) -> String {
    use bech32::{Bech32, Hrp};
    let (header, hrp) = if mainnet {
        (0xe1u8, "stake")
    } else {
        (0xe0u8, "stake_test")
    };
    let mut payload = Vec::with_capacity(29);
    payload.push(header);
    payload.extend_from_slice(cred);
    bech32::encode::<Bech32>(Hrp::parse(hrp).unwrap(), &payload).unwrap()
}

pub fn drep_to_bytes(drep: &conway::DRep) -> Vec<u8> {
    match drep {
        conway::DRep::Key(h) => [&[0x00], h.as_ref()].concat(),
        conway::DRep::Script(h) => [&[0x01], h.as_ref()].concat(),
        conway::DRep::Abstain => vec![0x02],
        conway::DRep::NoConfidence => vec![0x03],
    }
}

/// Extracted voting procedure: (voter, gov_action_id, vote).
pub type ExtractedVote = (conway::Voter, conway::GovActionId, conway::Vote);

/// The inputs a transaction actually spends and the outputs it actually creates, honouring
/// phase-2 validity. A valid tx (including all Byron/pre-Alonzo txs) uses its regular
/// inputs/outputs. A phase-2-invalid ("script-invalid") tx is recorded on-chain but the ledger
/// applies ONLY its collateral: it spends its collateral inputs and creates its collateral
/// return, placed at the ledger output index = the number of regular outputs (so a later spend
/// resolves against the correct UTXO ref). Its regular inputs/outputs, mints, withdrawals and
/// certificates never take effect. Returned outputs are `(index, output)` pairs.
pub fn effective_io<'a>(
    tx: &'a MultiEraTx<'_>,
) -> (Vec<MultiEraInput<'a>>, Vec<(usize, MultiEraOutput<'a>)>) {
    if tx.is_valid() {
        (tx.inputs(), tx.outputs().into_iter().enumerate().collect())
    } else {
        let idx = tx.outputs().len();
        (
            tx.collateral(),
            tx.collateral_return()
                .into_iter()
                .map(|o| (idx, o))
                .collect(),
        )
    }
}

pub trait MultiEraTxExt {
    /// Pool delegation certificates with full StakeCredential preserved.
    fn pool_delegation_certs(&self) -> Vec<PoolDelegationCert>;

    fn drep_delegation_changes(&self) -> Vec<DrepDelegationChange>;

    /// Pool registration certificates (used for both new pools and parameter updates).
    fn pool_updates(&self) -> Vec<PoolUpdate>;

    /// Pool retirement certificates as `(operator, retiring_epoch)`.
    fn pool_retirements(&self) -> Vec<(Vec<u8>, u64)>;

    /// Governance voting procedures from Conway-era transactions.
    fn voting_procedures(&self) -> Vec<ExtractedVote>;
}

impl MultiEraTxExt for MultiEraTx<'_> {
    fn pool_delegation_certs(&self) -> Vec<PoolDelegationCert> {
        let mut certs = Vec::new();
        for cert in self.certs() {
            match cert {
                MultiEraCert::AlonzoCompatible(c) => match &**c {
                    alonzo::Certificate::StakeDelegation(cred, pool) => {
                        certs.push((cred.clone(), Some(pool.as_ref().to_vec())));
                    }
                    alonzo::Certificate::StakeDeregistration(cred) => {
                        certs.push((cred.clone(), None));
                    }
                    _ => {}
                },
                MultiEraCert::Conway(c) => match &**c {
                    conway::Certificate::StakeDelegation(cred, pool)
                    | conway::Certificate::StakeVoteDeleg(cred, pool, _)
                    | conway::Certificate::StakeRegDeleg(cred, pool, _)
                    | conway::Certificate::StakeVoteRegDeleg(cred, pool, _, _) => {
                        certs.push((cred.clone(), Some(pool.as_ref().to_vec())));
                    }
                    conway::Certificate::StakeDeregistration(cred)
                    | conway::Certificate::UnReg(cred, _) => {
                        certs.push((cred.clone(), None));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        certs
    }

    fn drep_delegation_changes(&self) -> Vec<DrepDelegationChange> {
        let mut changes = Vec::new();
        for cert in self.certs() {
            if let MultiEraCert::Conway(c) = cert {
                match &**c {
                    conway::Certificate::VoteDeleg(cred, drep)
                    | conway::Certificate::StakeVoteDeleg(cred, _, drep)
                    | conway::Certificate::VoteRegDeleg(cred, drep, _)
                    | conway::Certificate::StakeVoteRegDeleg(cred, _, drep, _) => {
                        changes.push((stake_credential_bytes(cred), Some(drep_to_bytes(drep))));
                    }
                    conway::Certificate::StakeDeregistration(cred)
                    | conway::Certificate::UnReg(cred, _) => {
                        changes.push((stake_credential_bytes(cred), None));
                    }
                    _ => {}
                }
            }
        }
        changes
    }

    fn pool_updates(&self) -> Vec<PoolUpdate> {
        self.certs()
            .iter()
            .filter_map(|cert| match cert {
                MultiEraCert::AlonzoCompatible(c) => match &***c {
                    alonzo::Certificate::PoolRegistration {
                        operator,
                        pledge,
                        cost,
                        margin,
                        ..
                    } => Some((operator, pledge, cost, margin)),
                    _ => None,
                },
                MultiEraCert::Conway(c) => match &***c {
                    conway::Certificate::PoolRegistration {
                        operator,
                        pledge,
                        cost,
                        margin,
                        ..
                    } => Some((operator, pledge, cost, margin)),
                    _ => None,
                },
                _ => None,
            })
            .map(|(operator, pledge, cost, margin)| {
                (
                    operator.as_ref().to_vec(),
                    *pledge,
                    *cost,
                    margin.numerator,
                    margin.denominator,
                )
            })
            .collect()
    }

    fn pool_retirements(&self) -> Vec<(Vec<u8>, u64)> {
        self.certs()
            .iter()
            .filter_map(|cert| match cert {
                MultiEraCert::AlonzoCompatible(c) => match &***c {
                    alonzo::Certificate::PoolRetirement(operator, epoch) => {
                        Some((operator.as_ref().to_vec(), *epoch))
                    }
                    _ => None,
                },
                MultiEraCert::Conway(c) => match &***c {
                    conway::Certificate::PoolRetirement(operator, epoch) => {
                        Some((operator.as_ref().to_vec(), *epoch))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn voting_procedures(&self) -> Vec<ExtractedVote> {
        let mut votes = Vec::new();
        if let MultiEraTx::Conway(tx) = self {
            if let Some(ref procedures) = tx.transaction_body.voting_procedures {
                for (voter, actions) in procedures.iter() {
                    for (action_id, procedure) in actions.iter() {
                        votes.push((voter.clone(), action_id.clone(), procedure.vote.clone()));
                    }
                }
            }
        }
        votes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real mainnet phase-2-invalid transaction (956c5eff…): 1 regular input, 2 collateral
    /// inputs. `effective_io` must ignore its regular inputs/outputs and use only collateral.
    #[test]
    fn effective_io_uses_only_collateral_for_invalid_tx() {
        let bytes = hex::decode(include_str!("testdata/invalid_tx_956c5eff.hex").trim()).unwrap();
        let tx = MultiEraTx::decode(&bytes).unwrap();

        assert!(!tx.is_valid(), "fixture must be a phase-2-invalid tx");
        assert!(!tx.collateral().is_empty());
        // The fixture's regular inputs differ from its collateral, so the test distinguishes.
        assert_ne!(tx.inputs().len(), tx.collateral().len());

        let (inputs, outputs) = effective_io(&tx);

        // Spends the collateral inputs, not the (never-applied) regular inputs.
        assert_eq!(inputs.len(), tx.collateral().len());
        assert_eq!(inputs[0].hash(), tx.collateral()[0].hash());
        // Creates only the collateral return (0 or 1), at the ledger index = #regular outputs.
        assert_eq!(outputs.len(), tx.collateral_return().iter().count());
        if let Some((idx, _)) = outputs.first() {
            assert_eq!(*idx, tx.outputs().len());
        }
    }

    fn text(s: &str) -> Metadatum {
        Metadatum::Text(s.to_string())
    }

    /// Every `Metadatum` shape survives the trip to JSON.
    ///
    /// This is the whole of the server's remaining job with metadata: hand the values
    /// over intact so the client can decide what they mean. Anything lost here is lost
    /// for good, because there's nowhere else to recover it from.
    #[test]
    fn metadatum_shapes_survive_as_json() {
        assert_eq!(
            metadatum_to_json(&text("hello")),
            serde_json::json!("hello")
        );
        assert_eq!(
            metadatum_to_json(&Metadatum::Int(42.into())),
            serde_json::json!(42)
        );
        assert_eq!(
            metadatum_to_json(&Metadatum::Int((-7).into())),
            serde_json::json!(-7)
        );
        // Tagged, not rendered as a string: a byte string and a string are different
        // things, and flattening makes them indistinguishable.
        assert_eq!(
            metadatum_to_json(&Metadatum::Bytes(vec![0xde, 0xad].into())),
            serde_json::json!({ "bytes": "dead" })
        );
        assert_eq!(
            metadatum_to_json(&Metadatum::Array(vec![text("a"), Metadatum::Int(1.into())])),
            serde_json::json!(["a", 1])
        );
    }

    /// Metadata keys aren't always text, which is why a map keeps its `k`/`v` pairs
    /// rather than collapsing into a JSON object.
    #[test]
    fn a_map_keeps_keys_that_are_not_text() {
        let datum = Metadatum::Map(
            vec![
                (text("timestamp"), Metadatum::Int(1785570471.into())),
                (Metadatum::Int(0.into()), text("by-number")),
            ]
            .into(),
        );
        assert_eq!(
            metadatum_to_json(&datum),
            serde_json::json!({ "map": [
                { "k": "timestamp", "v": 1785570471 },
                { "k": 0, "v": "by-number" }
            ]})
        );
    }

    #[test]
    fn stake_credential_extraction_by_address_type() {
        let cred = vec![0x42u8; 28];
        // Base address (type 0): 1 header + 28 payment + 28 stake.
        let base = [&[0x01u8][..], &[0x11; 28], &cred].concat();
        assert_eq!(
            stake_credential_from_address_bytes(&base),
            Some(cred.clone())
        );
        // Reward address (type 14, header 0xe1): 1 header + 28 stake.
        let reward = [&[0xe1u8][..], &cred].concat();
        assert_eq!(
            stake_credential_from_address_bytes(&reward),
            Some(cred.clone())
        );
        // Enterprise address (type 6, header 0x61): no stake part.
        let enterprise = [&[0x61u8][..], &[0x11; 28]].concat();
        assert_eq!(stake_credential_from_address_bytes(&enterprise), None);
        // Empty / too short.
        assert_eq!(stake_credential_from_address_bytes(&[]), None);
        assert_eq!(stake_credential_from_address_bytes(&[0x01, 0x02]), None);
    }

    #[test]
    fn stake_address_from_cred_bytes_roundtrips() {
        let cred = vec![0x42u8; 28];

        let mainnet = stake_address_from_cred_bytes(&cred, true);
        assert!(mainnet.starts_with("stake1"));
        let (hrp, data) = bech32::decode(&mainnet).unwrap();
        assert_eq!(hrp.as_str(), "stake");
        assert_eq!(data[0], 0xe1); // key-cred mainnet reward-address header
        assert_eq!(&data[1..], cred.as_slice());

        let testnet = stake_address_from_cred_bytes(&cred, false);
        assert!(testnet.starts_with("stake_test"));
        let (_, data) = bech32::decode(&testnet).unwrap();
        assert_eq!(data[0], 0xe0);
        assert_eq!(&data[1..], cred.as_slice());
    }
}
