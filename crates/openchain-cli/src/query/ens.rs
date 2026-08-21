use eyre::{bail, Result};
use openchain_core::Config;

const ENS_REGISTRY: &str = "0x00000000000C2E074eC69A0dFb2997BA6C7d2e1e";
/// resolver(bytes32)
const SEL_RESOLVER: &str = "0x0178b8bf";
/// addr(bytes32)
const SEL_ADDR: &str = "0x3b3b57de";

/// ENS namehash (ENSIP-1 / EIP-137): keccak over label hashes walking right to left.
fn namehash(name: &str) -> [u8; 32] {
    use alloy::primitives::keccak256;
    let mut node = [0u8; 32];
    if name.is_empty() {
        return node;
    }
    for label in name.rsplit('.') {
        let label_hash = keccak256(label.as_bytes());
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&node);
        buf[32..].copy_from_slice(label_hash.as_slice());
        node = keccak256(buf).into();
    }
    node
}

async fn eth_call_bytes(rpc: &str, to: &str, data: &str) -> Result<Vec<u8>> {
    use alloy::primitives::{Address, Bytes};
    use alloy::providers::{Provider, RootProvider};
    use alloy::rpc::types::TransactionRequest;

    let provider = RootProvider::<alloy::network::Ethereum>::new_http(rpc.parse()?);
    let req = TransactionRequest::default()
        .to(to.parse::<Address>()?)
        .input(alloy::rpc::types::TransactionInput::new(Bytes::from(
            alloy::hex::decode(data.strip_prefix("0x").unwrap_or(data))?,
        )));
    let out = provider.call(req).await?;
    Ok(out.to_vec())
}

/// Resolve an ENS name to its address using the ENS contracts on Ethereum
/// mainnet. Returns None when the name does not resolve.
pub async fn resolve(config: &Config, name: &str) -> Result<Option<String>> {
    // ENS lives on Ethereum mainnet; resolve through chain 1's RPC.
    let rpc = match config.chains.get("1") {
        Some(c) => c.rpc.clone(),
        None => bail!(
            "cannot resolve '{name}': ENS needs a [chains.1] ethereum RPC in the config"
        ),
    };

    let node = namehash(name);
    let node_hex = alloy::hex::encode(node);

    // registry.resolver(node) → resolver address (last 20 bytes of the word).
    let out = eth_call_bytes(&rpc, ENS_REGISTRY, &format!("{SEL_RESOLVER}{node_hex}")).await?;
    if out.len() < 32 {
        return Ok(None);
    }
    let resolver = &out[out.len() - 20..];
    if resolver.iter().all(|&b| b == 0) {
        return Ok(None); // no resolver set
    }
    let resolver_hex = alloy::hex::encode(resolver);

    // resolver.addr(node) → address.
    let out =
        eth_call_bytes(&rpc, &format!("0x{resolver_hex}"), &format!("{SEL_ADDR}{node_hex}")).await?;
    if out.len() < 32 {
        return Ok(None);
    }
    let addr = &out[out.len() - 20..];
    if addr.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    Ok(Some(format!("0x{}", alloy::hex::encode(addr))))
}

/// Parse an address-or-ENS-name flag value. Names are resolved on demand;
/// plain 0x addresses pass through unchanged (lowercase hex, no prefix).
pub async fn parse_address_or_name(
    config: &Config,
    input: &str,
    flag: &str,
) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.contains('.') {
        match resolve(config, trimmed).await? {
            Some(addr) => {
                eprintln!("resolved {trimmed} → {addr}");
                return super::parse_address(&addr, flag);
            }
            None => bail!("ENS name '{trimmed}' does not resolve to an address"),
        }
    }
    super::parse_address(trimmed, flag)
}
