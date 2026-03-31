use gasket::daemon::Daemon;
use oura::{cursor, framework::*, sources};
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
    mut source: sources::Bootstrapper,
    mut sink: sink::Stage,
    mut cursor: cursor::Bootstrapper,
    mempool: mempool::Stage,
    policy: gasket::runtime::Policy,
) -> Result<Daemon, Error> {
    let prev = source.borrow_output();

    gasket::messaging::tokio::connect_ports(prev, &mut sink.input, 100);
    let prev = &mut sink.cursor;

    gasket::messaging::tokio::connect_ports(prev, cursor.borrow_track(), 100);

    let mut tethers = vec![];
    tethers.push(source.spawn(policy.clone()));
    tethers.push(gasket::runtime::spawn_stage(sink, policy.clone()));
    tethers.push(cursor.spawn(policy.clone()));
    tethers.push(gasket::runtime::spawn_stage(mempool, policy));

    let runtime = Daemon(tethers);

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

fn start_from_boundary(
    db_url: &Url,
    tip_slot: u64,
    epoch_length: u64,
) -> (IntersectConfig, Option<u64>) {
    let boundary_slot = tip_slot.saturating_sub(epoch_length);
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

pub fn run(args: Args) -> Result<(), Error> {
    setup_tracing(args.verbose);

    let nftcdn = NftcdnConfig::new(&args.network);
    let event_bus = Arc::new(EventBus::new(4096));
    let db_url = Url::parse(&args.db.replace("NETWORK", &args.network.to_string()))
        .expect("invalid database URL");
    let mut state = State::new(db_url.clone());

    let listen = args.listen;

    let source_config = sources::Config::N2C(sources::n2c::Config {
        socket_path: args.socket.clone(),
    });
    let mainnet = args.network.magic() == 764824073;
    let genesis = GenesisValues::from(args.network.config().clone());
    let epoch_length = genesis.shelley_epoch_length as u64;
    let genesis_config = server::GenesisConfig {
        shelley_known_slot: genesis.shelley_known_slot,
        shelley_known_time: genesis.shelley_known_time,
        shelley_slot_length: genesis.shelley_slot_length as u32,
        byron_epoch_length: genesis.byron_epoch_length as u32,
        byron_slot_length: genesis.byron_slot_length as u32,
        shelley_epoch_length: genesis.shelley_epoch_length as u32,
    };
    let mempool_config = mempool::Config {
        socket_path: args.socket.clone(),
        magic: args.network.magic(),
        mainnet,
        genesis,
    };

    let snapshot_path: PathBuf = [&args.output, &"snapshot.bin".to_string()].iter().collect();
    let snapshot_depth = args.snapshot_depth;

    let (intersect, catchup_target) =
        if let Some((snapshot, fi)) = State::load_snapshot(&snapshot_path, args.network.magic()) {
            let snap_slot = snapshot.slot;
            let snap_hash = snapshot.block_hash.clone().unwrap_or_default();
            state.restore_from_snapshot(snapshot);
            state.feed_index = fi;

            if let Some(snap) = state.current() {
                info!(
                    slot = snap_slot,
                    hash = snap_hash.as_str(),
                    pools = snap.pools.len(),
                    delegators = snap.pool_delegators.values().map(|d| d.len()).sum::<usize>(),
                    dreps = snap.drep_delegators.len(),
                    drep_delegators = snap.drep_delegators.values().map(|d| d.len()).sum::<usize>(),
                    utxos = snap.utxos.len(),
                    decimals = snap.decimals.len(),
                    "loaded snapshot, resuming"
                );
            }

            (IntersectConfig::Point(snap_slot, snap_hash), None)
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
                start_from_boundary(&db_url, tip_slot, epoch_length)
            }
        };

    let state = Arc::new(RwLock::new(state));

    let cursor_config = cursor::file::Config {
        path: Some([&args.output, &"cursor.json".to_string()].iter().collect()),
        ..Default::default()
    };

    let ctx = Context {
        chain: args.network.config().clone(),
        intersect,
        finalize: None,
        current_dir: PathBuf::from(args.output),
        breadcrumbs: Breadcrumbs::new(0),
    };

    let source = source_config.bootstrapper(&ctx)?;
    let sink = sink::bootstrapper(
        &ctx,
        event_bus.clone(),
        state.clone(),
        nftcdn.clone(),
        snapshot_path,
        snapshot_depth,
        catchup_target,
    )?;
    let cursor = cursor::Bootstrapper::File(cursor_config.bootstrapper(&ctx)?);
    let mempool = mempool::bootstrapper(
        mempool_config,
        event_bus.clone(),
        state.clone(),
        nftcdn.clone(),
    );
    let retries = define_gasket_policy();
    let daemon = connect_stages(source, sink, cursor, mempool, retries)?;

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
        tokio_rt.spawn(server::serve(
            addr,
            event_bus,
            state,
            nftcdn,
            genesis_config,
            args.n2n,
            args.network.magic(),
            mainnet,
        ));
    }

    daemon.block();

    info!("daemon is stopping");

    daemon.teardown();
    prometheus.abort();

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
