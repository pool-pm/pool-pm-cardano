use gasket::daemon::Daemon;
use oura::{cursor, framework::*, sources};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{broadcast, RwLock};
use tracing::info;

use crate::args::{Args, Metrics};
use crate::event::Event;
use crate::mempool;
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

pub fn run(args: Args) -> Result<(), Error> {
    setup_tracing(args.verbose);

    let (event_tx, _) = broadcast::channel::<Event>(4096);
    let state = Arc::new(RwLock::new(State::new()));

    let listen = args.listen;

    let source_config = sources::Config::N2C(sources::n2c::Config {
        socket_path: args.socket.clone(),
    });
    let sink_config = sink::Config {
        db_url: args.db.replace("NETWORK", &args.network.to_string()),
    };
    let mempool_config = mempool::Config {
        socket_path: args.socket.clone(),
        magic: args.network.magic(),
    };

    let cursor_config = cursor::file::Config {
        path: Some([&args.output, &"cursor.json".to_string()].iter().collect()),
        ..Default::default()
    };

    let ctx = Context {
        chain: args.network.config().clone(),
        intersect: IntersectConfig::Tip,
        finalize: None,
        current_dir: PathBuf::from(args.output),
        breadcrumbs: cursor_config.initial_load()?,
    };

    let source = source_config.bootstrapper(&ctx)?;
    let sink = sink::bootstrapper(sink_config, &ctx, event_tx.clone(), state.clone())?;
    let cursor = cursor::Bootstrapper::File(cursor_config.bootstrapper(&ctx)?);
    let mempool = mempool::bootstrapper(mempool_config, event_tx.clone(), state.clone());
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

    if let Some(addr) = listen {
        tokio_rt.spawn(server::serve(addr, event_tx));
    }

    daemon.block();

    info!("daemon is stopping");

    daemon.teardown();
    prometheus.abort();

    Ok(())
}
