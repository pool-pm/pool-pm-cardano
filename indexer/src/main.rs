use args::Args;
use clap::Parser;
use std::fs;
use std::process;

mod args;
mod chain;
mod daemon;
mod dbsync;
mod model;
mod sink;
mod utxorpc;

fn main() {
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
