use eyre::{bail, Result};
use openchain_core::{Config, Dataset};
use openchain_evm::decode::EventDecoder;
use openchain_sink::Sink;
use std::time::Instant;

/// Incrementally decode raw logs into decoded_events for every registered ABI.
/// Resumes from the decoded_events watermark up to the logs watermark.
pub async fn run(config: &Config, chain_id: u64, batch_blocks: u64) -> Result<()> {
    let sink = Sink::new(&config.clickhouse);
    sink.ensure_schema().await?;

    let abis = sink.list_abis(chain_id).await?;
    if abis.is_empty() {
        bail!("no ABIs registered for chain {chain_id}; run `openchain abi add` first");
    }
    let mut decoder = EventDecoder::new();
    for abi in &abis {
        decoder.register(abi.address, &abi.name, &abi.abi)?;
    }
    let addresses: Vec<[u8; 20]> = decoder.addresses().copied().collect();

    let Some(logs_wm) = sink.watermark(chain_id, Dataset::Logs).await? else {
        bail!("no logs synced for chain {chain_id}; run `openchain sync` first");
    };
    let min_block = sink.min_log_block(chain_id).await?.unwrap_or(0);
    let start_block = match sink.watermark(chain_id, Dataset::DecodedEvents).await? {
        Some(wm) if wm >= logs_wm => {
            println!("decoded_events already caught up to logs at block {logs_wm}");
            return Ok(());
        }
        Some(wm) => (wm + 1).max(min_block),
        None => min_block,
    };

    println!(
        "decoding chain {chain_id} blocks {start_block}..={logs_wm} ({} contracts)",
        decoder.contract_count()
    );
    let started = Instant::now();
    let (mut scanned, mut decoded, mut unmatched, mut failed) = (0u64, 0u64, 0u64, 0u64);

    let mut from = start_block;
    while from <= logs_wm {
        let to = (from + batch_blocks - 1).min(logs_wm);
        let logs = sink.logs_for_addresses(chain_id, &addresses, from, to).await?;
        scanned += logs.len() as u64;

        let mut rows = Vec::with_capacity(logs.len());
        for log in &logs {
            match decoder.decode(log) {
                Some(Ok(row)) => rows.push(row),
                Some(Err(err)) => {
                    failed += 1;
                    tracing::debug!(block = log.block_number, log_index = log.log_index, %err, "decode failed");
                }
                None => unmatched += 1,
            }
        }
        decoded += rows.len() as u64;
        sink.insert_decoded(&rows).await?;
        sink.set_watermark(chain_id, &[Dataset::DecodedEvents], to).await?;
        from = to + 1;
    }

    let secs = started.elapsed().as_secs_f64();
    println!(
        "done in {secs:.2}s: {decoded} events decoded from {scanned} logs \
         ({unmatched} unknown event, {failed} failed), {:.0} logs/s",
        scanned as f64 / secs.max(0.001)
    );
    Ok(())
}
