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
