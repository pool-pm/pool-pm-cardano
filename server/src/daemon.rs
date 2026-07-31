use gasket::daemon::Daemon;
use oura::framework::*;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use tracing::{info, warn};
use url::Url;

use crate::args::{Args, Metrics};
use crate::cip26;
use crate::event_bus::EventBus;
use crate::mempool;
use crate::nftcdn::NftcdnConfig;
use crate::server;
use crate::sink;
use crate::state::State;

fn define_gasket_policy() -> gasket::runtime::Policy {
    let policy = gasket::retries::Policy {
        max_retries: 20,
        backoff_unit: Duration::from_secs(1),
        backoff_factor: 2,
        max_backoff: Duration::from_secs(60),
        dismissible: false,
    };

    gasket::runtime::Policy {
        tick_timeout: None,
        bootstrap_retry: policy.clone(),
        work_retry: policy.clone(),
        teardown_retry: policy.clone(),
    }
}

fn connect_stages(
    mut source: crate::source::Stage,
    mut sink: sink::Stage,
    mempool: mempool::Stage,
    policy: gasket::runtime::Policy,
) -> Result<Daemon, Error> {
    gasket::messaging::tokio::connect_ports(&mut source.output, &mut sink.input, 100);

    let tethers = vec![
        gasket::runtime::spawn_stage(source, policy.clone()),
        gasket::runtime::spawn_stage(sink, policy.clone()),
        gasket::runtime::spawn_stage(mempool, policy),
    ];

    let runtime = Daemon::new(tethers);

    Ok(runtime)
}

fn setup_tracing(verbose: bool) {
    let level = match verbose {
        true => tracing::Level::DEBUG,
        false => tracing::Level::INFO,
    };
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(level)
            .finish(),
    )
    .unwrap();
}

async fn serve_prometheus(daemon: Arc<Daemon>, metrics: Option<Metrics>) -> Result<(), Error> {
    if let Some(Metrics(sockaddr)) = metrics {
        info!("starting metrics exporter");
        let runtime = daemon.clone();
        gasket_prometheus::serve(sockaddr, runtime).await;
    }

    Ok(())
}

fn start_from_boundary(db_url: &Url, tip_slot: u64) -> (IntersectConfig, Option<u64>) {
    const FEED_INDEX_WINDOW: u64 = 5 * 86400;
    let boundary_slot = tip_slot.saturating_sub(FEED_INDEX_WINDOW);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    match rt.block_on(State::boundary_block(db_url, boundary_slot)) {
        Some((b_slot, b_hash)) => {
            let blocks_estimate = (tip_slot - b_slot) / 20;
            info!(
                slot = b_slot,
                hash = b_hash.as_str(),
                blocks_estimate,
                "starting from boundary block"
            );
            (IntersectConfig::Point(b_slot, b_hash), Some(tip_slot))
        }
        None => {
            warn!("boundary block not found, starting from tip");
            (IntersectConfig::Tip, None)
        }
    }
}

/// Only log a startup populate step that took at least this long — the rest are no-ops on
/// a warm resume and would just be noise.
const STARTUP_STEP_LOG_MS: u64 = 100;

const WATCHDOG_TICK: Duration = Duration::from_secs(30);

/// Capture the process's state once a stall looks real but before acting on it. Mainnet mints a
/// block every ~20s, so three minutes of silence is a ~1-in-10⁴ chain event and a dump here
/// costs nothing in normal operation. This is the window the 2026-07-31 outage spent unobserved:
/// the process was stuck for seven hours and the restart that fixed it erased the cause.
const STALL_DUMP_AFTER_SECS: u64 = 180;

/// Give up and let the supervisor restart us. A warm resume costs ~16s, against hours of silence.
const STALL_RESTART_AFTER_SECS: u64 = 300;

/// 30s × 10 = the 5-minute heartbeat.
const HEARTBEAT_EVERY_TICKS: u64 = 10;

/// Watchdog on a plain OS thread — no async runtime, no lock, nothing it could block on. That
/// independence is the point: it has to outlive the failure it detects, and a check that had to
/// acquire something would hang along with everything else.
///
/// It watches the one fact that proves the whole pipeline moved: a block reaching the state.
/// A stall can park any stage — a wedged db query, a held lock, a full gasket port — and from
/// outside all of them look identical: the process is up, HTTP still accepts, nothing is logged.
/// So rather than guess, dump the evidence at [`STALL_DUMP_AFTER_SECS`] and exit at
/// [`STALL_RESTART_AFTER_SECS`].
///
/// `secs_since` is `None` until the first block, so a cold reset — many minutes with no block, by
/// design — cannot trip either threshold.
fn spawn_watchdog(db_url: String) {
    use crate::state::progress::{secs_since, summary, Link};

    std::thread::spawn(move || {
        let mut ticks: u64 = 0;
        // One dump per stall, not one per tick: the second dump of the same stall says nothing
        // the first didn't, and the exit path dumps again anyway.
        let mut dumped = false;
        loop {
            std::thread::sleep(WATCHDOG_TICK);
            ticks += 1;
            match secs_since(Link::BlockApplied) {
                Some(age) if age >= STALL_RESTART_AFTER_SECS => {
                    tracing::error!(
                        secs_since_last_block = age,
                        progress = %summary(),
                        "pipeline stalled — no block applied; exiting for the supervisor to restart"
                    );
                    crate::diagnostics::dump("pipeline stalled, exiting", &db_url);
                    // Flush the log lines before the process goes away.
                    std::thread::sleep(Duration::from_millis(200));
                    std::process::exit(1);
                }
                Some(age) if age >= STALL_DUMP_AFTER_SECS && !dumped => {
                    dumped = true;
                    warn!(
                        secs_since_last_block = age,
                        progress = %summary(),
                        "no block applied recently — capturing state before it is lost"
                    );
                    crate::diagnostics::dump("pipeline stalling", &db_url);
                }
                Some(age) if age >= STALL_DUMP_AFTER_SECS => {}
                // Recovered (or never stalled): re-arm, so a later stall dumps again.
                _ => dumped = false,
            }
            if ticks.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
                info!(
                    rss_mb = crate::state::rss_mb(),
                    fd_count = crate::state::fd_count(),
                    progress = %summary(),
                    "mem watchdog"
                );
            }
        }
    });
}

pub fn run(args: Args) -> Result<(), Error> {
    setup_tracing(args.verbose);

    let nftcdn = NftcdnConfig::new(&args.network);
    let event_bus = Arc::new(EventBus::new(4096));
    let db_url = Url::parse(&args.db.replace("NETWORK", &args.network.to_string()))
        .expect("invalid database URL");
    spawn_watchdog(db_url.to_string());
    let mut state = State::new(db_url.clone());

    let listen = args.listen;

    let mainnet = args.network.magic() == 764824073;
    let genesis = GenesisValues::from(args.network.config().clone());
    let genesis_config = server::GenesisConfig {
        shelley_known_slot: genesis.shelley_known_slot,
        shelley_known_time: genesis.shelley_known_time,
        shelley_slot_length: genesis.shelley_slot_length,
        byron_epoch_length: genesis.byron_epoch_length,
        byron_slot_length: genesis.byron_slot_length,
        shelley_epoch_length: genesis.shelley_epoch_length,
    };
    let mempool_config = mempool::Config {
        socket_path: args.socket.clone(),
        magic: args.network.magic(),
        mainnet,
        genesis,
    };

    let snapshot_path: PathBuf = [&args.output, &"snapshot.bin".to_string()].iter().collect();
    let snapshot_depth = args.snapshot_depth;

    let (intersect, catchup_target) = if let Some((snapshot, fi, interner)) =
        State::load_snapshot(&snapshot_path, args.network.magic())
    {
        let snap_slot = snapshot.slot;
        let snap_hash = snapshot.block_hash.clone().unwrap_or_default();
        state.restore_from_snapshot(snapshot, interner);
        state.feed_index = fi;
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // Time the step and log it when it costs something, so a slow startup can be
            // attributed instead of guessed at.
            let timed = |label: &str, f: &mut dyn FnMut()| {
                let started = std::time::Instant::now();
                f();
                let elapsed_ms = started.elapsed().as_millis() as u64;
                if elapsed_ms >= STARTUP_STEP_LOG_MS {
                    info!(step = label, elapsed_ms, "startup step");
                }
            };
            // Active stake lives in `State`, not the snapshot, so it is fetched on every
            // start; everything else the feeds need came out of the snapshot.
            let epoch = State::epoch_for_slot(
                snap_slot,
                &GenesisValues::from(args.network.config().clone()),
            );
            timed("active_stakes", &mut || {
                rt.block_on(state.populate_active_stakes(epoch))
            });
        }

        if let Some(snap) = state.current() {
            info!(
                slot = snap_slot,
                hash = snap_hash.as_str(),
                pools = snap.pools.len(),
                delegators = snap
                    .pool_delegators
                    .values()
                    .map(|d| d.len())
                    .sum::<usize>(),
                dreps = snap.dreps.len(),
                drep_delegators = snap
                    .drep_delegators
                    .values()
                    .map(|d| d.len())
                    .sum::<usize>(),
                utxos = snap.utxos.len(),
                decimals = snap.decimals.len(),
                handles = snap.address_by_handle.len(),
                gov_actions = snap.gov_action_titles.len(),
                address_balances = snap.address_balances.len(),
                "loaded snapshot, resuming"
            );
        }
        state.log_memory("loaded snapshot");

        // Estimate current tip from wall clock. If snapshot is >60s behind,
        // set a catchup target so SSE waits before accepting connections.
        let now_slot = genesis_config.shelley_known_slot
            + (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(genesis_config.shelley_known_time))
                / genesis_config.shelley_slot_length as u64;
        let catchup_target = if now_slot > snap_slot + 60 {
            Some(now_slot)
        } else {
            None
        };
        (IntersectConfig::Point(snap_slot, snap_hash), catchup_target)
    } else {
        // No snapshot — query tip from db-sync and start from 5 days ago
        info!("no snapshot, starting from 5 days ago");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let tip_slot = rt
            .block_on(State::boundary_block(&db_url, i64::MAX as u64))
            .map(|(s, _)| s)
            .unwrap_or(0);
        if tip_slot == 0 {
            warn!("no blocks in db-sync, starting from tip");
            (IntersectConfig::Tip, None)
        } else {
            start_from_boundary(&db_url, tip_slot)
        }
    };

    let state = Arc::new(RwLock::new(state));

    let ctx = Context {
        chain: args.network.config().clone(),
        intersect,
        finalize: None,
        current_dir: PathBuf::from(args.output),
        breadcrumbs: Breadcrumbs::new(0),
    };

    let catching_up = Arc::new(std::sync::atomic::AtomicBool::new(catchup_target.is_some()));
    // Node tip, published by the source and read by the sink to end catch-up at the real tip.
    let node_tip = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Source → sink: "the node handed over everything it has", and the last slot it sent.
    let at_tip = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sent_slot = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let source = crate::source::bootstrapper(
        &ctx,
        args.socket.clone(),
        node_tip.clone(),
        at_tip.clone(),
        sent_slot.clone(),
    );
    let sink = sink::bootstrapper(
        &ctx,
        sink::SinkConfig {
            event_bus: event_bus.clone(),
            state: state.clone(),
            nftcdn: nftcdn.clone(),
            snapshot_path,
            snapshot_depth,
            catchup_target,
            node_tip,
            at_tip,
            sent_slot,
            catching_up: catching_up.clone(),
        },
    )?;
    let mempool = mempool::bootstrapper(
        mempool_config,
        event_bus.clone(),
        state.clone(),
        nftcdn.clone(),
    );
    let retries = define_gasket_policy();
    let daemon = connect_stages(source, sink, mempool, retries)?;

    info!("daemon is running");

    let daemon = Arc::new(daemon);

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();

    let prometheus = tokio_rt.spawn(serve_prometheus(daemon.clone(), args.metrics));

    tokio_rt.spawn(cip26_refresh_task(state.clone(), mainnet));

    if let Some(addr) = listen {
        tokio_rt.spawn(server::serve(server::ServeConfig {
            addr,
            bus: event_bus,
            chain_state: state,
            nftcdn,
            genesis: genesis_config,
            n2n_addr: args.n2n,
            magic: args.network.magic(),
            mainnet,
            catching_up,
        }));
    }

    // gasket 0.11's Daemon::block/teardown consume `self`, but the daemon is shared (Arc) with
    // the metrics task. So poll stop_reason() here (exactly what block() loops on), then reclaim
    // sole ownership for the graceful teardown once the metrics task has released its clone.
    while daemon.stop_reason().is_none() {
        std::thread::sleep(Duration::from_millis(1500));
    }

    info!("daemon is stopping");

    prometheus.abort();
    let _ = tokio_rt.block_on(prometheus); // let the metrics task drop its Arc<Daemon> clone

    match Arc::try_unwrap(daemon) {
        Ok(daemon) => daemon.teardown(),
        Err(_) => warn!("daemon still shared, skipping graceful teardown"),
    }

    Ok(())
}

/// Background task: periodically check GitHub for CIP-26 token registry updates.
async fn cip26_refresh_task(state: Arc<RwLock<State>>, mainnet: bool) {
    let config = if mainnet {
        cip26::RegistryConfig::mainnet()
    } else {
        cip26::RegistryConfig::testnet()
    };
    let client = reqwest::Client::new();
    let mut last_sha: Option<String> = None;

    loop {
        tokio::time::sleep(Duration::from_secs(30 * 60)).await;

        // Check if the registry has new commits
        let sha = match cip26::fetch_commit_sha(&client, &config).await {
            Some(s) => s,
            None => continue,
        };
        if last_sha.as_ref() == Some(&sha) {
            continue;
        }

        info!(
            sha = sha.as_str(),
            "CIP-26 registry updated, refreshing decimals"
        );
        let entries = cip26::fetch_decimals(&client, &config).await;
        if entries.is_empty() {
            continue;
        }

        let mut state = state.write().await;
        if let Some(snap) = state.current_mut() {
            let before = snap.decimals.len();
            for (fp, d) in entries {
                snap.decimals.entry(fp).or_insert(d);
            }
            let added = snap.decimals.len() - before;
            if added > 0 {
                info!(
                    added,
                    total = snap.decimals.len(),
                    "CIP-26 decimals updated"
                );
            }
        }
        last_sha = Some(sha);
    }
}
