use eyre::Result;
use openchain_core::{Config, Dataset};
use openchain_evm::EvmSource;
use openchain_sink::Sink;
use std::collections::BTreeMap;
use std::time::Duration;

/// Tail the chain head. Keeps a hot window of recent block hashes; when a new
/// block's parent hash does not match, walks back to the common ancestor,
/// rewinds ClickHouse, and re-syncs the canonical chain.
pub async fn run(
    config: &Config,
    chain_id: u64,
    datasets: &[Dataset],
    poll_interval: u64,
    hot_window: u64,
) -> Result<()> {
    let chain = config.chain(chain_id)?;
    let source = EvmSource::connect(&chain.rpc, chain_id).await?;
    let sink = Sink::new(&config.clickhouse);
    sink.ensure_schema().await?;

    let mut window: BTreeMap<u64, [u8; 32]> = sink
        .recent_block_hashes(chain_id, hot_window)
        .await?
        .into_iter()
        .map(|r| (r.block_number, r.block_hash))
        .collect();

    let mut next = match window.last_key_value() {
        Some((n, _)) => n + 1,
        None => {
            let latest = source.latest_block().await?;
            tracing::info!(latest, "no local history, starting from chain head");
            latest
        }
    };
    tracing::info!(chain_id, next, "following chain head");

    loop {
        let latest = source.latest_block().await?;
        while next <= latest {
            let bundle = source.fetch_bundle(next).await?;

            if let Some(parent) = window.get(&(next - 1)) {
                if *parent != bundle.block.parent_hash {
                    let ancestor = find_common_ancestor(&source, &window, next - 1).await?;
                    tracing::warn!(chain_id, ancestor, "reorg detected, rewinding");
                    sink.rewind(chain_id, ancestor + 1).await?;
                    window.split_off(&(ancestor + 1));
                    next = ancestor + 1;
                    continue;
                }
            }

            tracing::info!(
                block = next,
                txs = bundle.txs.len(),
                logs = bundle.logs.len(),
                "synced"
            );
            sink.insert_bundles(std::slice::from_ref(&bundle), datasets).await?;
            sink.set_watermark(chain_id, datasets, next).await?;
            window.insert(next, bundle.block.block_hash);
            while window.len() as u64 > hot_window {
                window.pop_first();
            }
            next += 1;
        }
        tokio::time::sleep(Duration::from_secs(poll_interval)).await;
    }
}

async fn find_common_ancestor(
    source: &EvmSource,
    window: &BTreeMap<u64, [u8; 32]>,
    from: u64,
) -> Result<u64> {
    let mut n = from;
    loop {
        match window.get(&n) {
            Some(local) => {
                let (remote, _) = source.header_hashes(n).await?;
                if remote == *local {
                    return Ok(n);
                }
            }
            None => {
                tracing::warn!(n, "reorg deeper than hot window; rewinding to window edge");
                return Ok(n);
            }
        }
        if n == 0 {
            return Ok(0);
        }
        n -= 1;
    }
}
