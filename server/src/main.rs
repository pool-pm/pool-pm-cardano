use args::Args;
use clap::Parser;
use std::fs;
use std::process;

// Use jemalloc as the global allocator. The indexer allocates large, short-lived buffers
// (snapshot (de)serialize, db-sync result Vecs) on top of millions of tiny `imbl` chunks;
// glibc malloc is slow to return those freed pages to the OS, so resident set ratchets up.
// jemalloc's background purge threads release freed pages on a decay timer, keeping RSS
// near the live working set. Not built on MSVC (jemalloc is unsupported there).
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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

    // Enable jemalloc background purge threads (off by default even when compiled in) so
    // freed pages are returned to the OS proactively rather than only on the next alloc in
    // that arena. Tune the decay timers without rebuilding via `_RJEM_MALLOC_CONF`
    // (e.g. `dirty_decay_ms:2000,muzzy_decay_ms:2000`).
    #[cfg(not(target_env = "msvc"))]
    if let Err(e) = tikv_jemalloc_ctl::background_thread::write(true) {
        eprintln!("WARNING: failed to enable jemalloc background_thread: {e}");
    }

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
