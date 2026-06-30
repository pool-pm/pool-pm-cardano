use args::Args;
use clap::Parser;
use std::fs;
use std::process;

mod args;
mod chain;
mod cip26;
mod cip68;
mod daemon;
mod event;
mod event_bus;
mod filter;
mod mempool;
mod model;
mod nftcdn;
mod oracle;
mod pallas;
mod server;
mod sink;
mod state;

fn main() {
    dotenvy::dotenv().ok();
    let args = Args::parse();

    if let Err(err) = fs::create_dir_all(&args.output) {
        eprintln!("ERROR: {err:#?}");
        process::exit(1);
    }

    let result = daemon::run(args);

    if let Err(err) = &result {
        eprintln!("ERROR: {err:#?}");
        process::exit(1);
    }

    process::exit(0);
}
