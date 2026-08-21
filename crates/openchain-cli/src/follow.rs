use alloy::network::Ethereum;
use alloy::providers::{Provider, RootProvider};
use alloy::pubsub::SubscriptionStream;
use alloy::rpc::types::Header;
use eyre::{Context, Result};
use futures::StreamExt;
use openchain_core::{ChainConfig, Config, Dataset};
use openchain_evm::EvmSource;
use openchain_sink::Sink;
use std::collections::BTreeMap;
use std::time::Duration;

const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Tail the chain head. Keeps a hot window of recent block hashes; when a new
/// block's parent hash does not match, walks back to the common ancestor,
/// rewinds ClickHouse, and re-syncs the canonical chain.
///
/// Wakes on WS `newHeads` when the endpoint supports it (freshness ~0s), with
/// HTTP polling as fallback and safety net.
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

    let mut heads: Option<HeadStream> = connect_heads(chain).await;
    match &heads {
        Some(_) => tracing::info!("newHeads subscription active"),
        None => tracing::info!(poll_interval, "WS unavailable, polling every {poll_interval}s"),
    }

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
        // A head from the subscription may race the RPC's read model; fetch_bundle
        // retries absorb that lag, so trust it as the sync target directly.
        let latest = match heads.as_mut() {
            Some(hs) => {
                tokio::select! {
                    head = hs.stream.next() => match head {
                        Some(head) => head.inner.number,
                        None => {
                            tracing::warn!("head subscription closed, falling back to polling");
                            heads = None;
                            source.latest_block().await?
                        }
                    },
                    // No head yet within poll_interval: normal between blocks
                    // (12s on mainnet). Keep the subscription, re-check latest.
                    _ = tokio::time::sleep(Duration::from_secs(poll_interval)) => {
                        source.latest_block().await?
                    }
                }
            }
            None => {
                tokio::time::sleep(Duration::from_secs(poll_interval)).await;
                if heads.is_none() {
                    if let Some(hs) = connect_heads(chain).await {
                        tracing::info!("newHeads subscription restored");
                        heads = Some(hs);
                    }
                }
                source.latest_block().await?
            }
        };

        while next <= latest {
            let bundle = source.fetch_bundle(next).await?;
            let traces = if datasets.contains(&Dataset::Traces) {
                Some(source.fetch_traces(next).await?)
            } else {
                None
            };

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
            if let Some(traces) = traces {
                sink.insert_traces(&traces).await?;
            }
            sink.set_watermark(chain_id, datasets, next).await?;
            window.insert(next, bundle.block.block_hash);
            while window.len() as u64 > hot_window {
                window.pop_first();
            }
            next += 1;
        }
    }
}

/// WS client + head stream. The provider is kept in the struct so the
/// underlying connection lives as long as the subscription does.
struct HeadStream {
    _provider: RootProvider<Ethereum>,
    stream: SubscriptionStream<Header>,
}

async fn connect_heads(chain: &ChainConfig) -> Option<HeadStream> {
    let url = chain.ws_url();
    tokio::time::timeout(WS_CONNECT_TIMEOUT, async {
        let provider = RootProvider::<Ethereum>::connect(&url)
            .await
            .wrap_err("cannot open WS connection")?;
        let sub = provider.subscribe_blocks().await.wrap_err("cannot subscribe to newHeads")?;
        Ok::<_, eyre::Report>(HeadStream { _provider: provider, stream: sub.into_stream() })
    })
    .await
    .ok()
    .and_then(|res| match res {
        Ok(pair) => Some(pair),
        Err(err) => {
            tracing::debug!(%err, "newHeads subscription failed");
            None
        }
    })
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
