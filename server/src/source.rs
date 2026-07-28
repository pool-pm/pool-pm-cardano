//! N2C chain-sync source — replaces oura's `sources::n2c`.
//!
//! Behaviourally identical to oura's N2C source (drive pallas-network chain-sync, emit
//! `ChainEvent::Apply`/`Reset` to the sink) with ONE critical addition: a **read timeout**.
//!
//! Oura's source blocks forever in `recv_while_must_reply().await` when the node's N2C connection
//! goes half-alive — socket open, no data, no error. `.or_restart()` never fires (a hang isn't an
//! `Err`), so the whole pipeline freezes silently. That is exactly the production freeze on
//! 2026-07-28: the source logged "awaiting next block (blocking)" for ~36 min while the chain kept
//! advancing, then went silent; the sink and mempool stalled behind it.
//!
//! Here the blocking read is wrapped in `tokio::time::timeout`. On timeout we return
//! `WorkerError::Restart`; gasket tears down the worker (dropping the `NodeClient`) and re-runs
//! `bootstrap` — reconnecting and re-intersecting at our tracked breadcrumbs. All in-process: the
//! sink and its ~10 GB state, and every SSE client, are untouched. Recovery is one reconnect
//! (sub-second on the local socket) instead of a multi-hour outage or a snapshot-reloading restart.

use std::path::PathBuf;
use std::time::Duration;

use gasket::framework::*;
use tracing::{debug, error, info, warn};

use pallas::ledger::traverse::MultiEraBlock;
use pallas::network::facades::NodeClient;
use pallas::network::miniprotocols::chainsync::{BlockContent, NextResponse};
use pallas::network::miniprotocols::Point;

use oura::framework::{
    Breadcrumbs, ChainEvent, Context, GenesisValues, IntersectConfig, Record, SourceOutputPort,
};

use tokio::time::timeout;

/// If no chain-sync message (block or rollback) arrives within this window, treat the N2C
/// connection as stalled and reconnect. Sized from real db-sync data over the last 180 days
/// (764k blocks): everyday gaps are tiny — p999 = 137 s, p9999 = 184 s, p99999 = 238 s — while
/// the absolute max (575 s) was a one-off at the recent hard-fork transition. So 300 s clears
/// normal operation comfortably; the only thing that could trip it is a hard-fork-scale gap, and
/// a reconnect there is harmless (sub-second re-handshake + re-intersect, no state loss). Erring
/// on the smaller side means a genuine stall is caught in ~5 min instead of the multi-hour freeze.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Recent chain points kept for re-intersecting after a reconnect. `find_intersect` picks the
/// newest point still on-chain, so this window also tolerates a rollback that happened while we
/// were disconnected (as long as its target is within the window). ~100 blocks ≈ 30+ minutes.
const BREADCRUMB_CAP: usize = 100;

#[derive(Stage)]
#[stage(
    name = "source",
    unit = "NextResponse<BlockContent>",
    worker = "Worker"
)]
pub struct Stage {
    socket_path: PathBuf,
    chain: GenesisValues,
    /// The intersection used on the *first* connect (snapshot point / db-sync boundary / tip).
    /// After that, `breadcrumbs` (our live position) drives re-intersection on reconnect.
    intersect: IntersectConfig,
    breadcrumbs: Breadcrumbs,

    /// Last block slot we applied, and the node's tip slot as of the last message we received.
    /// Logged on a stall so we can tell whether we were *behind* the node (the connection stopped
    /// delivering blocks that existed) or genuinely *at* the tip — the key question for diagnosing
    /// whether a freeze is the N2C connection vs. runtime starvation.
    last_slot: u64,
    last_tip_slot: u64,

    pub output: SourceOutputPort,

    #[metric]
    ops_count: gasket::metrics::Counter,
    #[metric]
    chain_tip: gasket::metrics::Gauge,
    #[metric]
    current_slot: gasket::metrics::Gauge,
    #[metric]
    rollback_count: gasket::metrics::Counter,
    #[metric]
    reconnect_count: gasket::metrics::Counter,
}

/// Establish the chain-sync intersection. On a reconnect the breadcrumbs hold our recent points,
/// so we resume exactly where we were (the node replies with a no-op rollback to that point, then
/// rolls forward). On the very first connect the breadcrumbs are empty and we use the configured
/// intersect. If, on a reconnect, *none* of our breadcrumbs is still on-chain (a rollback deeper
/// than our window happened while disconnected), we exit so systemd cold-restarts us — which
/// resumes from db-sync, the same path used when a snapshot is too old.
async fn intersect(peer: &mut NodeClient, stage: &Stage) -> Result<(), WorkerError> {
    let chainsync = peer.chainsync();

    if !stage.breadcrumbs.is_empty() {
        let (found, _tip) = chainsync
            .find_intersect(stage.breadcrumbs.points())
            .await
            .or_restart()?;
        match found {
            Some(point) => {
                info!(?point, "reconnected, re-intersected at breadcrumb");
                return Ok(());
            }
            None => {
                error!("reconnect: no breadcrumb still on-chain (deep rollback); exiting for cold db-sync resume");
                std::process::exit(1);
            }
        }
    }

    match &stage.intersect {
        IntersectConfig::Origin => {
            info!("intersecting origin");
            chainsync.intersect_origin().await.or_restart()?;
        }
        IntersectConfig::Tip => {
            info!("intersecting tip");
            chainsync.intersect_tip().await.or_restart()?;
        }
        IntersectConfig::Point(..) | IntersectConfig::Breadcrumbs(..) => {
            let points = stage.intersect.points().unwrap_or_default();
            let (point, _tip) = chainsync.find_intersect(points).await.or_restart()?;
            info!(?point, "intersected at configured point");
        }
    }

    Ok(())
}

pub struct Worker {
    peer: NodeClient,
}

impl Worker {
    async fn process_next(
        &mut self,
        stage: &mut Stage,
        next: &NextResponse<BlockContent>,
    ) -> Result<(), WorkerError> {
        match next {
            NextResponse::RollForward(cbor, tip) => {
                let block = MultiEraBlock::decode(cbor).or_panic()?;
                let slot = block.slot();
                let hash = block.hash();
                let point = Point::Specific(slot, hash.to_vec());

                debug!(slot, %hash, "roll forward");

                stage
                    .output
                    .send(ChainEvent::apply(
                        point.clone(),
                        Record::CborBlock(cbor.to_vec()),
                    ))
                    .await
                    .or_panic()?;

                stage.breadcrumbs.track(point);
                stage.last_slot = slot;
                stage.last_tip_slot = tip.0.slot_or_default();
                stage.chain_tip.set(stage.last_tip_slot as i64);
                stage.current_slot.set(slot as i64);
                stage.ops_count.inc(1);
                Ok(())
            }
            NextResponse::RollBackward(point, tip) => {
                match point {
                    Point::Origin => debug!("rollback to origin"),
                    Point::Specific(slot, _) => debug!(slot, "rollback"),
                }

                stage
                    .output
                    .send(ChainEvent::reset(point.clone()))
                    .await
                    .or_panic()?;

                stage.breadcrumbs.track(point.clone());
                stage.last_slot = point.slot_or_default();
                stage.last_tip_slot = tip.0.slot_or_default();
                stage.chain_tip.set(stage.last_tip_slot as i64);
                stage.current_slot.set(point.slot_or_default() as i64);
                stage.rollback_count.inc(1);
                stage.ops_count.inc(1);
                Ok(())
            }
            // At the tip: `recv_while_must_reply` blocks for the next block, so we normally never
            // see `Await` here; if we do, it's a no-op and the next schedule reads again.
            NextResponse::Await => {
                debug!("reached tip, awaiting next block");
                Ok(())
            }
        }
    }
}

#[async_trait::async_trait(?Send)]
impl gasket::framework::Worker<Stage> for Worker {
    async fn bootstrap(stage: &Stage) -> Result<Self, WorkerError> {
        debug!("connecting to node");
        let mut peer = NodeClient::connect(&stage.socket_path, stage.chain.magic)
            .await
            .or_retry()?;
        intersect(&mut peer, stage).await?;
        Ok(Self { peer })
    }

    async fn schedule(
        &mut self,
        stage: &mut Stage,
    ) -> Result<WorkSchedule<NextResponse<BlockContent>>, WorkerError> {
        let client = self.peer.chainsync();

        // Catching up (we have agency): request the next block, which arrives immediately.
        // At the tip (must-reply): block until the node pushes the next block. THIS is the read
        // that hung forever in oura; we bound it so a half-alive connection can't wedge us.
        let read = async {
            if client.has_agency() {
                client.request_next().await
            } else {
                client.recv_while_must_reply().await
            }
        };

        match timeout(READ_TIMEOUT, read).await {
            Ok(res) => Ok(WorkSchedule::Unit(res.or_restart()?)),
            Err(_elapsed) => {
                warn!(
                    timeout_s = READ_TIMEOUT.as_secs(),
                    our_slot = stage.last_slot,
                    node_tip_slot = stage.last_tip_slot,
                    behind_slots = stage.last_tip_slot.saturating_sub(stage.last_slot),
                    "n2c chain-sync stalled (no message in window); reconnecting"
                );
                stage.reconnect_count.inc(1);
                // Restart => gasket drops the worker (and its connection) and re-bootstraps,
                // reconnecting + re-intersecting at our breadcrumbs. State/SSE clients untouched.
                Err(WorkerError::Restart)
            }
        }
    }

    async fn execute(
        &mut self,
        unit: &NextResponse<BlockContent>,
        stage: &mut Stage,
    ) -> Result<(), WorkerError> {
        self.process_next(stage, unit).await
    }
}

/// Build the source stage. `ctx.intersect` sets the first-connect intersection (snapshot point,
/// db-sync boundary, or tip — chosen in `daemon::run`); breadcrumbs then track our live position.
pub fn bootstrapper(ctx: &Context, socket_path: PathBuf) -> Stage {
    Stage {
        socket_path,
        chain: ctx.chain.clone().into(),
        intersect: ctx.intersect.clone(),
        // NOTE: NOT `ctx.breadcrumbs` — the daemon builds that with capacity 0, which retains
        // nothing, so a reconnect would re-intersect at the boot point and replay. We keep a real
        // window so reconnects resume at the current tip.
        breadcrumbs: Breadcrumbs::new(BREADCRUMB_CAP),
        last_slot: 0,
        last_tip_slot: 0,
        output: Default::default(),
        ops_count: Default::default(),
        chain_tip: Default::default(),
        current_slot: Default::default(),
        rollback_count: Default::default(),
        reconnect_count: Default::default(),
    }
}
