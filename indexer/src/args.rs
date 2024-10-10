use clap::Parser;
use oura::framework::ChainConfig;
use serde::Deserialize;
use std::{
    error::Error,
    fmt::{self, Display},
    net::{AddrParseError, SocketAddr},
    ops::Deref,
    str::FromStr,
};

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

    /// Cardano node peers
    #[clap(short, long, value_delimiter = ' ', num_args = 1..)]
    pub peers: Vec<String>,

    /// Enable Prometheus metrics{n} (ADDR:PORT or 'default' for 127.0.0.1:9188)
    #[clap(short, long)]
    pub metrics: Option<Metrics>,

    /// Network ('mainnet', 'preprod', 'preview')
    #[clap(short, long, default_value = "mainnet")]
    pub network: Chain,

    /// Output directory
    #[clap(short, long, default_value = "/tmp/cardano")]
    pub output: String,

    /// Verbose logs
    #[clap(short, long)]
    pub verbose: bool,
}

impl Args {
    pub fn chain(&self) -> oura::framework::ChainConfig {
        let Chain(config) = &self.network;
        config.clone()
    }
}

#[derive(Clone, Deserialize)]
pub struct Metrics(pub SocketAddr);

#[derive(Debug)]
pub struct MetricsParseError;

impl Display for MetricsParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "expecting ADDR:PORT (ex: 0.0.0.0:9188)")
    }
}

impl Error for MetricsParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

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

#[derive(Clone)]
pub struct Chain(ChainConfig);

#[derive(Debug)]
pub struct ChainParseError;

impl Display for ChainParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "expecting mainnet, preprod or preview")
    }
}

impl Error for ChainParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

impl FromStr for Chain {
    type Err = ChainParseError;

    fn from_str(s: &str) -> Result<Chain, ChainParseError> {
        match s {
            "mainnet" => Ok(Chain(ChainConfig::Mainnet)),
            "preprod" => Ok(Chain(ChainConfig::PreProd)),
            "preview" => Ok(Chain(ChainConfig::Preview)),
            _ => Err(ChainParseError),
        }
    }
}

impl ToString for Chain {
    fn to_string(&self) -> String {
        match self {
            Chain(ChainConfig::Mainnet) => "mainnet".to_string(),
            Chain(ChainConfig::PreProd) => "preprod".to_string(),
            Chain(ChainConfig::Preview) => "preview".to_string(),
            _ => "".to_string(),
        }
    }
}

impl Default for Chain {
    fn default() -> Self {
        Chain(ChainConfig::Mainnet)
    }
}

impl Deref for Chain {
    type Target = ChainConfig;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
