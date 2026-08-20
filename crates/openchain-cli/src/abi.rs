use eyre::{bail, eyre, Context, Result};
use openchain_core::{now_millis, AbiRow, Config};
use openchain_evm::decode::EventDecoder;
use openchain_sink::Sink;
use std::path::PathBuf;
use std::str::FromStr;

pub async fn add(
    config: &Config,
    chain_id: u64,
    address: &str,
    name: Option<String>,
    file: Option<PathBuf>,
) -> Result<()> {
    let address = alloy::primitives::Address::from_str(address).wrap_err("invalid address")?;
    let sink = Sink::new(&config.clickhouse);
    sink.ensure_schema().await?;

    let (abi_json, contract_name, source) = match file {
        Some(path) => {
            let raw = std::fs::read_to_string(&path)?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            // Accept either a raw ABI array or a compiler artifact with an "abi" field.
            let abi = match &value {
                serde_json::Value::Array(_) => value.clone(),
                serde_json::Value::Object(o) => {
                    o.get("abi").cloned().ok_or_else(|| eyre!("no 'abi' field in {}", path.display()))?
                }
                _ => bail!("unrecognized ABI file format"),
            };
            let fallback = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            (abi.to_string(), name.unwrap_or(fallback), "file".to_string())
        }
        None => {
            let (abi, fetched_name) = fetch_sourcify(chain_id, &address.to_string()).await?;
            (abi, name.unwrap_or(fetched_name), "sourcify".to_string())
        }
    };

    // Validate the ABI and count decodable events before storing.
    let mut decoder = EventDecoder::new();
    let event_count = decoder.register(address.0 .0, &contract_name, &abi_json)?;

    sink.insert_abi(&AbiRow {
        chain_id,
        address: address.0 .0,
        name: contract_name.clone(),
        abi: abi_json,
        source,
        insert_version: now_millis(),
    })
    .await?;
    println!("registered {contract_name} at {address} ({event_count} decodable events)");
    Ok(())
}

pub async fn list(config: &Config, chain_id: u64) -> Result<()> {
    let sink = Sink::new(&config.clickhouse);
    let abis = sink.list_abis(chain_id).await?;
    if abis.is_empty() {
        println!("no ABIs registered for chain {chain_id}");
        return Ok(());
    }
    for abi in abis {
        let address = alloy::primitives::Address::from(abi.address);
        println!("{address}  {}  (source: {})", abi.name, abi.source);
    }
    Ok(())
}

async fn fetch_sourcify(chain_id: u64, address: &str) -> Result<(String, String)> {
    let url =
        format!("https://sourcify.dev/server/v2/contract/{chain_id}/{address}?fields=abi,compilation");
    let response = reqwest::get(&url).await?;
    if !response.status().is_success() {
        bail!(
            "Sourcify has no verified ABI for {address} on chain {chain_id} \
             (HTTP {}); pass --file <abi.json> instead",
            response.status()
        );
    }
    let body: serde_json::Value = response.json().await?;
    let abi = body.get("abi").ok_or_else(|| eyre!("Sourcify response missing 'abi'"))?.to_string();
    let name = body
        .pointer("/compilation/name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();
    Ok((abi, name))
}
