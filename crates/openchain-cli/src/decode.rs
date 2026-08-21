use eyre::{bail, Result};
use openchain_core::{Config, Dataset};
use openchain_evm::decode::{EventDecoder, FunctionDecoder};
use openchain_sink::Sink;
use rayon::prelude::*;
use std::time::Instant;

/// Below this many items per batch, parallel-decode overhead (thread pool
/// wake-up, fold/reduce) outweighs the CPU saved; stay sequential.
const PARALLEL_THRESHOLD: usize = 50_000;

/// Incrementally decode raw logs into decoded_events and call inputs into
/// decoded_calls for every registered ABI. Each stream resumes from its own
/// watermark up to its source dataset's watermark.
pub async fn run(config: &Config, chain_id: u64, batch_blocks: u64) -> Result<()> {
    let sink = Sink::new(&config.clickhouse);
    sink.ensure_schema().await?;

    let abis = sink.list_abis(chain_id).await?;
    if abis.is_empty() {
        bail!("no ABIs registered for chain {chain_id}; run `openchain abi add` first");
    }
    let mut events = EventDecoder::new();
    let mut calls = FunctionDecoder::new();
    for abi in &abis {
        events.register(abi.address, &abi.name, &abi.abi)?;
        calls.register(abi.address, &abi.name, &abi.abi)?;
    }

    decode_events(&sink, chain_id, &events, batch_blocks).await?;
    decode_calls(&sink, chain_id, &calls, batch_blocks).await?;
    Ok(())
}

async fn decode_events(
    sink: &Sink,
    chain_id: u64,
    decoder: &EventDecoder,
    batch_blocks: u64,
) -> Result<()> {
    let Some(source_wm) = sink.watermark(chain_id, Dataset::Logs).await? else {
        bail!("no logs synced for chain {chain_id}; run `openchain sync` first");
    };
    let min_block = sink.min_log_block(chain_id).await?.unwrap_or(0);
    let start_block = match sink.watermark(chain_id, Dataset::DecodedEvents).await? {
        Some(wm) if wm >= source_wm => {
            println!("decoded_events already caught up to logs at block {source_wm}");
            return Ok(());
        }
        Some(wm) => (wm + 1).max(min_block),
        None => min_block,
    };

    let addresses: Vec<[u8; 20]> = decoder.addresses().copied().collect();
    println!(
        "decoding chain {chain_id} blocks {start_block}..={source_wm} ({} contracts)",
        decoder.contract_count()
    );
    let started = Instant::now();
    let (mut scanned, mut decoded, mut unmatched, mut failed) = (0u64, 0u64, 0u64, 0u64);

    let mut from = start_block;
    while from <= source_wm {
        let to = (from + batch_blocks - 1).min(source_wm);
        let logs = sink.logs_for_addresses(chain_id, &addresses, from, to).await?;
        scanned += logs.len() as u64;

        let (rows, stats) = par_decode(&logs, |log| {
            (
                (log.block_number, log.log_index),
                decoder.decode(log),
            )
        });
        decoded += stats.decoded;
        unmatched += stats.unmatched;
        failed += stats.failed;
        sink.insert_decoded(&rows).await?;
        sink.set_watermark(chain_id, &[Dataset::DecodedEvents], to).await?;
        from = to + 1;
    }

    let secs = started.elapsed().as_secs_f64();
    println!(
        "events: {decoded} decoded from {scanned} logs in {secs:.2}s \
         ({unmatched} unknown event, {failed} failed), {:.0} logs/s",
        scanned as f64 / secs.max(0.001)
    );
    Ok(())
}

async fn decode_calls(
    sink: &Sink,
    chain_id: u64,
    decoder: &FunctionDecoder,
    batch_blocks: u64,
) -> Result<()> {
    let Some(source_wm) = sink.watermark(chain_id, Dataset::Traces).await? else {
        println!("decoded_calls: no traces synced; run `openchain sync --datasets ...,traces`");
        return Ok(());
    };
    let min_block = sink.min_trace_block(chain_id).await?.unwrap_or(0);
    let start_block = match sink.watermark(chain_id, Dataset::DecodedCalls).await? {
        Some(wm) if wm >= source_wm => return Ok(()),
        Some(wm) => (wm + 1).max(min_block),
        None => min_block,
    };

    let addresses: Vec<[u8; 20]> = decoder.addresses().copied().collect();
    println!(
        "decoding calls chain {chain_id} blocks {start_block}..={source_wm} ({} functions)",
        decoder.function_count()
    );
    let started = Instant::now();
    let (mut scanned, mut decoded, mut unmatched, mut failed) = (0u64, 0u64, 0u64, 0u64);

    let mut from = start_block;
    while from <= source_wm {
        let to = (from + batch_blocks - 1).min(source_wm);
        let traces = sink.calls_for_addresses(chain_id, &addresses, from, to).await?;
        scanned += traces.len() as u64;

        let (rows, stats) = par_decode(&traces, |trace| {
            ((trace.block_number, trace.tx_index), decoder.decode_call(trace))
        });
        decoded += stats.decoded;
        unmatched += stats.unmatched;
        failed += stats.failed;
        sink.insert_decoded_calls(&rows).await?;
        sink.set_watermark(chain_id, &[Dataset::DecodedCalls], to).await?;
        from = to + 1;
    }

    let secs = started.elapsed().as_secs_f64();
    println!(
        "calls: {decoded} decoded from {scanned} traces in {secs:.2}s \
         ({unmatched} unknown function, {failed} failed), {:.0} traces/s",
        scanned as f64 / secs.max(0.001)
    );
    Ok(())
}

/// Decode one batch, fanning out across cores when the batch is big enough
/// for the thread-pool overhead to pay off. The closure returns per-item
/// position metadata (for debug logging) alongside the decode result.
fn par_decode<T, R>(
    items: &[T],
    f: impl Fn(&T) -> ((u64, u32), Option<Result<R>>) + Sync + Send,
) -> (Vec<R>, BatchStats)
where
    T: Sync,
    R: Send,
{
    if items.len() < PARALLEL_THRESHOLD {
        let mut rows = Vec::with_capacity(items.len());
        let mut stats = BatchStats::default();
        for item in items {
            match f(item) {
                (_, Some(Ok(row))) => {
                    rows.push(row);
                    stats.decoded += 1;
                }
                ((block, index), Some(Err(err))) => {
                    stats.failed += 1;
                    tracing::debug!(block, log_index = index, %err, "decode failed");
                }
                (_, None) => stats.unmatched += 1,
            }
        }
        return (rows, stats);
    }
    items
        .par_iter()
        .map(f)
        .fold(
            || (Vec::new(), BatchStats::default()),
            |mut acc, ((block, index), result)| match result {
                Some(Ok(row)) => {
                    acc.0.push(row);
                    acc.1.decoded += 1;
                    acc
                }
                Some(Err(err)) => {
                    acc.1.failed += 1;
                    tracing::debug!(block, log_index = index, %err, "decode failed");
                    acc
                }
                None => {
                    acc.1.unmatched += 1;
                    acc
                }
            },
        )
        .reduce(
            || (Vec::new(), BatchStats::default()),
            |mut a, mut b| {
                a.0.append(&mut b.0);
                a.1.decoded += b.1.decoded;
                a.1.unmatched += b.1.unmatched;
                a.1.failed += b.1.failed;
                a
            },
        )
}

#[derive(Default)]
struct BatchStats {
    decoded: u64,
    unmatched: u64,
    failed: u64,
}
