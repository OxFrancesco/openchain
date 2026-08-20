use eyre::{bail, Result};
use futures::{stream, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use openchain_core::{BlockBundle, Config, Dataset};
use openchain_evm::EvmSource;
use openchain_sink::Sink;

pub async fn run(
    config: &Config,
    chain_id: u64,
    from: Option<u64>,
    to: Option<String>,
    datasets: &[Dataset],
    chunk_size: u64,
    concurrency: usize,
) -> Result<()> {
    let chain = config.chain(chain_id)?;
    let source = EvmSource::connect(&chain.rpc, chain_id).await?;
    let sink = Sink::new(&config.clickhouse);
    sink.ensure_schema().await?;

    let to = match to.as_deref() {
        None | Some("latest") => source.latest_block().await?,
        Some(s) => s.parse()?,
    };
    let from = match from {
        Some(f) => f,
        None => {
            let mut min: Option<u64> = None;
            for dataset in datasets {
                if let Some(w) = sink.watermark(chain_id, *dataset).await? {
                    min = Some(min.map_or(w, |m| m.min(w)));
                }
            }
            match min {
                Some(w) => w + 1,
                None => bail!("no previous sync found for chain {chain_id}; pass --from"),
            }
        }
    };
    if from > to {
        println!("chain {chain_id}: already synced to {to}, nothing to do");
        return Ok(());
    }

    println!(
        "syncing chain {chain_id} ({}) blocks {from}..={to} -> [{}]",
        chain.name,
        datasets.iter().map(|d| d.table()).collect::<Vec<_>>().join(", ")
    );
    let progress = ProgressBar::new(to - from + 1).with_style(ProgressStyle::with_template(
        "{bar:40.cyan/blue} {pos}/{len} blocks ({per_sec}, eta {eta})",
    )?);

    let chunks: Vec<(u64, u64)> = (from..=to)
        .step_by(chunk_size as usize)
        .map(|start| (start, (start + chunk_size - 1).min(to)))
        .collect();

    // Chunks resolve in order (buffered preserves ordering), so the watermark
    // always marks a contiguous prefix and interrupted syncs resume cleanly.
    let mut stream = stream::iter(chunks.into_iter().map(|(start, end)| {
        let source = source.clone();
        async move {
            let futs = (start..=end).map(|n| source.fetch_bundle(n));
            futures::future::try_join_all(futs).await
        }
    }))
    .buffered(concurrency);

    let (mut total_txs, mut total_logs) = (0u64, 0u64);
    while let Some(bundles) = stream.next().await {
        let bundles: Vec<BlockBundle> = bundles?;
        sink.insert_bundles(&bundles, datasets).await?;
        let last = bundles.last().expect("non-empty chunk").number();
        sink.set_watermark(chain_id, datasets, last).await?;
        total_txs += bundles.iter().map(|b| b.txs.len() as u64).sum::<u64>();
        total_logs += bundles.iter().map(|b| b.logs.len() as u64).sum::<u64>();
        progress.inc(bundles.len() as u64);
    }
    progress.finish();
    println!(
        "done: {} blocks, {total_txs} transactions, {total_logs} logs",
        to - from + 1
    );
    Ok(())
}
