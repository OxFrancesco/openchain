use eyre::{bail, Result};
use futures::{stream, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use openchain_core::{BlockBundle, Config, Dataset, TraceRow};
use openchain_evm::ratelimit::{is_throttle_error, Governor};
use openchain_evm::EvmSource;
use openchain_sink::Sink;
use std::sync::Arc;

pub struct SyncOptions {
    /// Blocks per ClickHouse insert batch.
    pub chunk_size: u64,
    /// Initial concurrent block fetches; the governor adapts from here.
    pub concurrency: usize,
    /// Upper bound for adaptive concurrency.
    pub max_concurrency: usize,
}

pub async fn run(
    config: &Config,
    chain_id: u64,
    from: Option<u64>,
    to: Option<String>,
    datasets: &[Dataset],
    opts: &SyncOptions,
) -> Result<()> {
    let chain = config.chain(chain_id)?;
    let source = EvmSource::connect(&chain.rpc, chain_id).await?;
    let sink = Sink::new(&config.clickhouse);
    sink.ensure_schema().await?;

    let to = match to.as_deref() {
        None | Some("latest") => source.latest_block().await?,
        Some(s) => s.parse()?,
    };
    // Frozen starting minimum across selected datasets: flushing above it
    // advances watermarks, backfilling below it must never regress them.
    let mut start_watermark: Option<u64> = None;
    for dataset in datasets {
        if let Some(w) = sink.watermark(chain_id, *dataset).await? {
            start_watermark = Some(start_watermark.map_or(w, |m| m.min(w)));
        }
    }
    let from = match from {
        Some(f) => f,
        None => match start_watermark {
            Some(w) => w + 1,
            None => bail!("no previous sync found for chain {chain_id}; pass --from"),
        },
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

    let want_traces = datasets.contains(&Dataset::Traces);
    let governor = Arc::new(Governor::new(opts.concurrency, opts.max_concurrency));

    // Blocks flow through in order; the governor decides how many hit the RPC
    // at once, so the watermark always marks a contiguous prefix.
    let mut stream = stream::iter((from..=to).map(|n| {
        let source = source.clone();
        let governor = governor.clone();
        async move {
            let permit = governor.acquire().await;
            let res = source.fetch_outcome(n, want_traces).await;
            drop(permit);
            match &res {
                Ok(outcome) => {
                    if outcome.throttled {
                        governor.record_throttled().await;
                    } else {
                        governor.record_success();
                    }
                }
                Err(err) if is_throttle_error(err) => governor.record_throttled().await,
                Err(_) => {}
            }
            res
        }
    }))
    .buffered(opts.max_concurrency);

    let mut pending: Vec<BlockBundle> = Vec::with_capacity(opts.chunk_size as usize);
    let mut pending_traces: Vec<TraceRow> = Vec::new();
    let (mut total_txs, mut total_logs, mut total_traces) = (0u64, 0u64, 0u64);

    while let Some(item) = stream.next().await {
        let outcome = item?;
        let (bundle, traces) = (outcome.bundle, outcome.traces);
        total_txs += bundle.txs.len() as u64;
        total_logs += bundle.logs.len() as u64;
        progress.inc(1);
        pending.push(bundle);
        if let Some(traces) = traces {
            total_traces += traces.len() as u64;
            pending_traces.extend(traces);
        }
        if pending.len() == opts.chunk_size as usize {
            flush(&sink, chain_id, datasets, &mut pending, &mut pending_traces, start_watermark)
                .await?;
        }
    }
    if !pending.is_empty() {
        flush(&sink, chain_id, datasets, &mut pending, &mut pending_traces, start_watermark).await?;
    }
    progress.finish();
    println!(
        "done: {} blocks, {total_txs} transactions, {total_logs} logs{traces_note}",
        to - from + 1,
        traces_note =
            if want_traces { format!(", {total_traces} traces") } else { String::new() },
    );
    Ok(())
}

async fn flush(
    sink: &Sink,
    chain_id: u64,
    datasets: &[Dataset],
    bundles: &mut Vec<BlockBundle>,
    traces: &mut Vec<TraceRow>,
    start_watermark: Option<u64>,
) -> Result<()> {
    sink.insert_bundles(bundles, datasets).await?;
    if datasets.contains(&Dataset::Traces) {
        sink.insert_traces(traces).await?;
        traces.clear();
    }
    let last = bundles.last().expect("non-empty flush").number();
    // Backfilling a range below the starting watermark must not regress it.
    if start_watermark.is_none_or(|w| last > w) {
        sink.set_watermark(chain_id, datasets, last).await?;
    }
    bundles.clear();
    Ok(())
}
