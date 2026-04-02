use clap::Parser;
use serde::Deserialize;
use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;

use crate::chain::Chain;

/// Cardano tokens fetcher
#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    /// Cardano-db-sync database connect URL
    #[clap(
        short,
        long,
        default_value = "postgresql:///NETWORK?host=/var/run/postgresql"
    )]
    pub db: String,

    /// Cardano node socket path
    #[clap(short, long)]
    pub socket: PathBuf,

    /// Enable Prometheus metrics{n} (ADDR:PORT or 'default' for 127.0.0.1:9188)
    #[clap(short, long)]
    pub metrics: Option<Metrics>,

    /// Network ('mainnet', 'preprod', 'preview')
    #[clap(short, long, default_value = "mainnet")]
    pub network: Chain,

    /// Output directory
    #[clap(short, long, default_value = "/tmp/cardano")]
    pub output: String,

    /// SSE server listen address (e.g., 0.0.0.0:3000)
    #[clap(short, long)]
    pub listen: Option<SocketAddr>,

    /// Node-to-node address for block-fetch
    #[clap(long, default_value = "127.0.0.1:3001")]
    pub n2n: SocketAddr,

    /// Snapshot depth (blocks back from tip for persistence)
    #[clap(long, default_value = "8")]
    pub snapshot_depth: usize,

    /// Clear cached UTXOs from snapshot on startup (forces re-resolution from db-sync)
    #[clap(long)]
    pub clear_utxos: bool,

    /// Verbose logs
    #[clap(short, long)]
    pub verbose: bool,
}

#[derive(Clone, Deserialize)]
pub struct Metrics(pub SocketAddr);

impl FromStr for Metrics {
    type Err = AddrParseError;

    fn from_str(s: &str) -> Result<Metrics, AddrParseError> {
        match s {
            "default" => Ok(Default::default()),
            _ => s.parse().map(Metrics),
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics("127.0.0.1:9188".parse().unwrap())
    }
}
