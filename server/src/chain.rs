use oura::framework::{ChainConfig, GenesisValues};
use std::{
    error::Error,
    fmt::{self, Display},
    ops::Deref,
    str::FromStr,
};

#[derive(Clone)]
pub struct Chain(ChainConfig);

impl Chain {
    pub fn config(&self) -> &ChainConfig {
        &self.0
    }

    pub fn magic(&self) -> u64 {
        GenesisValues::from(self.0.clone()).magic
    }
}

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

impl Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match self {
            Chain(ChainConfig::Mainnet) => "mainnet",
            Chain(ChainConfig::PreProd) => "preprod",
            Chain(ChainConfig::Preview) => "preview",
            _ => "",
        };
        write!(f, "{s}")
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
