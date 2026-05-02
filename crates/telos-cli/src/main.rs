use alloy::primitives::Address;
use eyre::{Result, WrapErr};
use telos_listener::{watch_both, watch_fills, watch_headers, watch_intents};
use telos_settler::PriceBook;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cfg = Config::from_env()?;
    let fork_url = cfg.fork_url.clone();
    let hedge_venue = cfg.hedge_venue;
    let prices = PriceBook::new();

    match cfg.mode() {
        Mode::Both { tempo_url, tempo_contract, hl_url, hl_contract } => {
            watch_both(
                &tempo_url,
                tempo_contract,
                &hl_url,
                hl_contract,
                prices,
                hedge_venue,
                fork_url,
            )
            .await
        }
        Mode::Intents { url, contract } => {
            watch_intents(&url, contract, prices, hedge_venue, fork_url).await
        }
        Mode::Fills { url, contract } => watch_fills(&url, contract, prices).await,
        Mode::Headers { url } => watch_headers(&url).await,
    }
}

struct Config {
    fallback_url: String,
    tempo_url: Option<String>,
    tempo_contract: Option<Address>,
    hl_url: Option<String>,
    hl_contract: Option<Address>,
    fork_url: Option<String>,
    hedge_venue: Option<Address>,
}

enum Mode {
    Headers { url: String },
    Intents { url: String, contract: Address },
    Fills { url: String, contract: Address },
    Both {
        tempo_url: String,
        tempo_contract: Address,
        hl_url: String,
        hl_contract: Address,
    },
}

impl Config {
    fn from_env() -> Result<Self> {
        Ok(Self {
            fallback_url: std::env::var("TELOS_WS_URL")
                .unwrap_or_else(|_| "wss://ethereum-rpc.publicnode.com".to_string()),
            tempo_url: std::env::var("TELOS_TEMPO_WS_URL").ok(),
            tempo_contract: parse_addr("TELOS_TEMPO_CONTRACT")?,
            hl_url: std::env::var("TELOS_HL_WS_URL").ok(),
            hl_contract: parse_addr("TELOS_HL_CONTRACT")?,
            fork_url: std::env::var("TELOS_TEMPO_FORK_URL").ok(),
            hedge_venue: parse_addr("TELOS_HL_GATEWAY")?,
        })
    }

    fn mode(self) -> Mode {
        let tempo = self.tempo_contract.map(|c| {
            (
                self.tempo_url.clone().unwrap_or_else(|| self.fallback_url.clone()),
                c,
            )
        });
        let hl = self
            .hl_contract
            .map(|c| (self.hl_url.clone().unwrap_or_else(|| self.fallback_url.clone()), c));

        match (tempo, hl) {
            (Some((tempo_url, tempo_contract)), Some((hl_url, hl_contract))) => Mode::Both {
                tempo_url,
                tempo_contract,
                hl_url,
                hl_contract,
            },
            (Some((url, contract)), None) => Mode::Intents { url, contract },
            (None, Some((url, contract))) => Mode::Fills { url, contract },
            (None, None) => Mode::Headers { url: self.fallback_url },
        }
    }
}

fn parse_addr(var: &str) -> Result<Option<Address>> {
    match std::env::var(var) {
        Ok(s) => Ok(Some(s.parse().wrap_err_with(|| format!("{var} must be a hex address"))?)),
        Err(_) => Ok(None),
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
