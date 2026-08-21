use eyre::{bail, Context, Result};
use openchain_core::Config;

/// A resolved token: address (lowercase hex, no 0x), symbol, decimals.
#[derive(Debug, Clone)]
pub struct Token {
    pub address: String,
    pub symbol: String,
    pub decimals: u32,
}

struct Curated {
    chain: u64,
    address: &'static str,
    symbol: &'static str,
    decimals: u32,
}

/// Well-known tokens per chain so `--token usdt` just works out of the box.
const CURATED: &[Curated] = &[
    // Ethereum (1)
    Curated { chain: 1, address: "0xdAC17F958D2ee523a2206206994597C13D831ec7", symbol: "USDT", decimals: 6 },
    Curated { chain: 1, address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", symbol: "USDC", decimals: 6 },
    Curated { chain: 1, address: "0x6B175474E89094C44Da98b954EedeAC495271d0F", symbol: "DAI", decimals: 18 },
    Curated { chain: 1, address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", symbol: "WETH", decimals: 18 },
    Curated { chain: 1, address: "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599", symbol: "WBTC", decimals: 8 },
    Curated { chain: 1, address: "0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84", symbol: "stETH", decimals: 18 },
    Curated { chain: 1, address: "0x514910771AF9Ca656af840dff83E8264EcF986CA", symbol: "LINK", decimals: 18 },
    Curated { chain: 1, address: "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984", symbol: "UNI", decimals: 18 },
    Curated { chain: 1, address: "0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9", symbol: "AAVE", decimals: 18 },
    Curated { chain: 1, address: "0x6982508145454Ce325dDbE47a25d4ec3d2311933", symbol: "PEPE", decimals: 18 },
    Curated { chain: 1, address: "0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE", symbol: "SHIB", decimals: 18 },
    Curated { chain: 1, address: "0xdC035D45d973E3EC169d2276DDab16f1e407384F", symbol: "USDS", decimals: 18 },
    // Base (8453)
    Curated { chain: 8453, address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", symbol: "USDC", decimals: 6 },
    Curated { chain: 8453, address: "0x4200000000000000000000000000000000000006", symbol: "WETH", decimals: 18 },
    Curated { chain: 8453, address: "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf", symbol: "cbBTC", decimals: 8 },
    Curated { chain: 8453, address: "0x940181a94A35A45652Ebd2D01B644A5bC1b39d2f", symbol: "AERO", decimals: 18 },
    Curated { chain: 8453, address: "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb", symbol: "DAI", decimals: 18 },
    // Arbitrum One (42161)
    Curated { chain: 42161, address: "0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9", symbol: "USDT", decimals: 6 },
    Curated { chain: 42161, address: "0xaf88d065e77c8cC2239327C5EDb3A432268e5831", symbol: "USDC", decimals: 6 },
    Curated { chain: 42161, address: "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1", symbol: "WETH", decimals: 18 },
    Curated { chain: 42161, address: "0x912CE59144191C1204E64559FE8253a0e49E6548", symbol: "ARB", decimals: 18 },
    // OP Mainnet (10)
    Curated { chain: 10, address: "0x94b008aA00579c1307B0EF2c499aD98a8ce58e58", symbol: "USDT", decimals: 6 },
    Curated { chain: 10, address: "0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85", symbol: "USDC", decimals: 6 },
    Curated { chain: 10, address: "0x4200000000000000000000000000000000000006", symbol: "WETH", decimals: 18 },
    Curated { chain: 10, address: "0x4200000000000000000000000000000000000042", symbol: "OP", decimals: 18 },
    // Polygon PoS (137)
    Curated { chain: 137, address: "0xc2132D05D31c914a87C6611C10748AEb04B58e8F", symbol: "USDT", decimals: 6 },
    Curated { chain: 137, address: "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359", symbol: "USDC", decimals: 6 },
    Curated { chain: 137, address: "0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619", symbol: "WETH", decimals: 18 },
    Curated { chain: 137, address: "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270", symbol: "WPOL", decimals: 18 },
];

fn curated(chain: u64) -> impl Iterator<Item = Token> + 'static {
    CURATED.iter().filter(move |c| c.chain == chain).map(|c| Token {
        address: c.address.trim_start_matches("0x").to_ascii_lowercase(),
        symbol: c.symbol.to_string(),
        decimals: c.decimals,
    })
}

/// Resolve `--token <symbol|address>` for a chain.
///
/// Symbols match the built-in curated list and any token already cached in the
/// `tokens` table. Raw addresses get their decimals fetched on-chain once and
/// cached, so subsequent queries are pure SQL.
pub async fn resolve(config: &Config, chain: u64, input: &str) -> Result<Token> {
    let input = input.trim();
    if input.starts_with("0x") || input.starts_with("0X") {
        let addr = super::parse_address(input, "--token")?;
        return resolve_address(config, chain, &addr).await;
    }
    let wanted = input.to_ascii_uppercase();

    // Cached tokens first (they may override or extend the curated list).
    let cached = super::ch(
        config,
        &format!(
            "SELECT concat('0x', lower(hex(address))) AS a, symbol, decimals FROM tokens \
             WHERE chain_id = {chain} AND upper(symbol) = '{wanted}' \
             LIMIT 1 FORMAT JSONEachRow"
        ),
        "JSONEachRow",
    )
    .await?;
    if let Some(line) = cached.lines().next() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            return Ok(Token {
                address: v["a"].as_str().unwrap_or_default().trim_start_matches("0x").to_ascii_lowercase(),
                symbol: v["symbol"].as_str().unwrap_or(input).to_string(),
                decimals: v["decimals"].as_u64().unwrap_or(18) as u32,
            });
        }
    }

    if let Some(t) = curated(chain).find(|t| t.symbol.to_ascii_uppercase() == wanted) {
        return Ok(t);
    }

    let mut known: Vec<String> = curated(chain).map(|t| t.symbol).collect();
    known.sort();
    known.dedup();
    bail!(
        "unknown token '{input}' on chain {chain} — pass a contract address (0x…) instead. \
         Known symbols here: {}",
        known.join(", ")
    )
}

async fn resolve_address(config: &Config, chain: u64, addr: &str) -> Result<Token> {
    // Cache lookup first.
    let cached = super::ch(
        config,
        &format!(
            "SELECT symbol, name, decimals FROM tokens \
             WHERE chain_id = {chain} AND address = unhex('{addr}') \
             LIMIT 1 FORMAT JSONEachRow"
        ),
        "JSONEachRow",
    )
    .await?;
    if let Some(line) = cached.lines().next() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            return Ok(Token {
                address: addr.to_string(),
                symbol: v["symbol"].as_str().unwrap_or("?").to_string(),
                decimals: v["decimals"].as_u64().unwrap_or(18) as u32,
            });
        }
    }

    // On-chain fetch: decimals() = 0x313ce567, symbol() = 0x95d89b41.
    use alloy::primitives::{Address, Bytes};
    use alloy::providers::{Provider, RootProvider};
    use alloy::rpc::types::TransactionRequest;

    let chain_cfg = config.chain(chain)?;
    let provider = RootProvider::<alloy::network::Ethereum>::new_http(
        chain_cfg.rpc.parse().wrap_err("invalid rpc url in config")?,
    );
    let to = addr.parse::<Address>().wrap_err("invalid token address")?;
    let eth_call = |selector: &'static str| {
        let req = TransactionRequest::default()
            .to(to)
            .input(Bytes::from(alloy::hex::decode(selector).expect("static hex")).into());
        provider.call(req)
    };

    let decimals = match eth_call("0x313ce567").await {
        Ok(out) if out.len() >= 32 => out[out.len() - 1] as u32,
        _ => bail!(
            "cannot read decimals() from {addr} on chain {chain} — is it an ERC-20 token and is the RPC reachable?"
        ),
    };
    let symbol = match eth_call("0x95d89b41").await {
        Ok(out) => decode_symbol(&out).unwrap_or_else(|| "?".to_string()),
        Err(_) => "?".to_string(),
    };

    // Cache for next time; best-effort only.
    let _ = super::ch(
        config,
        &format!(
            "INSERT INTO tokens (chain_id, address, symbol, name, decimals, source, updated_at) \
             SELECT {chain}, unhex('{addr}'), '{symbol}', '', {decimals}, 'onchain', now()"
        ),
        "JSONEachRow",
    )
    .await;

    Ok(Token { address: addr.to_string(), symbol, decimals })
}

/// Decode an ABI-encoded dynamic string return value into Rust.
fn decode_symbol(out: &[u8]) -> Option<String> {
    // offset(32) len(32) data… — take words after the length word.
    if out.len() < 96 {
        return None;
    }
    let len = u128::from_be_bytes(out[32 + 16..64].try_into().ok()?) as usize;
    if len > 64 || out.len() < 64 + len {
        return None;
    }
    String::from_utf8(out[64..64 + len].to_vec()).ok()
}
