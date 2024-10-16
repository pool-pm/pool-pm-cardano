use gasket::daemon::Daemon;
use oura::{cursor, filters, framework::*, sources};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tracing::info;

use crate::args::{Args, Metrics};
use crate::sink;

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
    mut filters: Vec<filters::Bootstrapper>,
    mut sink: sink::Stage,
    mut cursor: cursor::Bootstrapper,
    policy: gasket::runtime::Policy,
) -> Result<Daemon, Error> {
    let mut prev = source.borrow_output();

    for filter in filters.iter_mut() {
        gasket::messaging::tokio::connect_ports(prev, filter.borrow_input(), 100);
        prev = filter.borrow_output();
    }

    gasket::messaging::tokio::connect_ports(prev, &mut sink.input, 100);
    let prev = &mut sink.cursor;

    gasket::messaging::tokio::connect_ports(prev, cursor.borrow_track(), 100);

    let mut tethers = vec![];
    tethers.push(source.spawn(policy.clone()));
    tethers.extend(filters.into_iter().map(|x| x.spawn(policy.clone())));
    tethers.push(gasket::runtime::spawn_stage(sink, policy.clone()));
    tethers.push(cursor.spawn(policy));

    let runtime = Daemon(tethers);

    Ok(runtime)
}

fn setup_tracing(args: &Args) {
    let level = match args.verbose {
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
    setup_tracing(&args);

    let source_config = sources::Config::N2N(sources::n2n::Config {
        peers: args.peers.clone(),
    });
    let filter_configs = vec![filters::Config::ParseCbor(filters::parse_cbor::Config {})];
    let sink_config = sink::Config {
        db_url: args.db.replace("NETWORK", &args.network.to_string()),
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
    let filters = filter_configs
        .into_iter()
        .map(|x| x.bootstrapper(&ctx))
        .collect::<Result<_, _>>()?;
    let sink = sink_config.bootstrapper(&ctx)?;
    let cursor = cursor::Bootstrapper::File(cursor_config.bootstrapper(&ctx)?);
    let retries = define_gasket_policy();
    let daemon = connect_stages(source, filters, sink, cursor, retries)?;

    info!("fetcher is running");

    let daemon = Arc::new(daemon);

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();

    let prometheus = tokio_rt.spawn(serve_prometheus(daemon.clone(), args.metrics));

    daemon.block();

    info!("oura is stopping");

    daemon.teardown();
    prometheus.abort();

    Ok(())
}
