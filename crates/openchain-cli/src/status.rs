use eyre::Result;
use openchain_core::{Config, Dataset};
use openchain_evm::EvmSource;
use openchain_sink::Sink;

/// Per-dataset row counts, block ranges, and watermarks for one chain, plus
/// head lag against the RPC's latest block.
pub async fn run(config: &Config, chain_id: u64) -> Result<()> {
    let chain = config.chain(chain_id)?;
    let sink = Sink::new(&config.clickhouse);
    sink.ensure_schema().await?;
    let stats = sink.dataset_stats(chain_id).await?;

    let head = match EvmSource::connect(&chain.rpc, chain_id).await {
        Ok(source) => Some(source.latest_block().await?),
        Err(err) => {
            tracing::warn!(%err, "cannot reach RPC for chain head");
            None
        }
    };

    println!("chain {chain_id} ({})", chain.name);
    println!(
        "{:<16} {:>12} {:>12} {:>12} {:>12}",
        "dataset", "rows", "min_block", "max_block", "watermark"
    );
    let mut blocks_max: Option<u64> = None;
    for s in &stats {
        if s.dataset == Dataset::Blocks {
            blocks_max = (s.rows > 0).then_some(s.max_block);
        }
        if s.rows == 0 && s.watermark.is_none() {
            continue;
        }
        println!(
            "{:<16} {:>12} {:>12} {:>12} {:>12}",
            s.dataset,
            s.rows,
            format_opt(s.rows > 0, s.min_block),
            format_opt(s.rows > 0, s.max_block),
            format_opt(s.watermark.is_some(), s.watermark.unwrap_or(0)),
        );
    }
    match (head, blocks_max) {
        (Some(head), Some(local)) => println!("head {head}, local lag {} blocks", head - local),
        (Some(head), None) => println!("head {head}, nothing synced yet"),
        (None, _) => {}
    }
    Ok(())
}

fn format_opt(present: bool, v: u64) -> String {
    if present { v.to_string() } else { "-".to_string() }
}
