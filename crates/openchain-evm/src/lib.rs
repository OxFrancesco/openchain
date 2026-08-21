pub mod decode;
pub mod ratelimit;

use alloy::consensus::Transaction as _;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::Ethereum;
use alloy::providers::ext::TraceApi;
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::{Block, TransactionReceipt};
use alloy::rpc::types::trace::parity::{
    Action, LocalizedTransactionTrace, TraceOutput,
};
use eyre::{eyre, Context, Result};
use openchain_core::{now_millis, BlockBundle, BlockRow, LogRow, TraceRow, TxRow};
use std::collections::HashMap;
use std::time::Duration;
use time::OffsetDateTime;

const RETRIES: u32 = 4;

/// A fetched bundle plus whether the endpoint showed throttle signs along the
/// way (retries recovered it, but the caller's rate governor must know).
#[derive(Debug, Clone)]
pub struct BundleOutcome {
    pub bundle: BlockBundle,
    pub traces: Option<Vec<TraceRow>>,
    pub throttled: bool,
}

/// An EVM chain data source backed by any JSON-RPC endpoint.
/// Extracts blocks, transactions, and logs with two calls per block:
/// eth_getBlockByNumber(full) + eth_getBlockReceipts.
#[derive(Clone)]
pub struct EvmSource {
    provider: RootProvider<Ethereum>,
    chain_id: u64,
}

impl EvmSource {
    pub async fn connect(rpc_url: &str, chain_id: u64) -> Result<Self> {
        let url = rpc_url.parse().wrap_err("invalid RPC url")?;
        let provider = RootProvider::<Ethereum>::new_http(url);
        let remote = provider.get_chain_id().await.wrap_err("cannot reach RPC endpoint")?;
        if remote != chain_id {
            return Err(eyre!("RPC endpoint reports chain id {remote}, expected {chain_id}"));
        }
        Ok(EvmSource { provider, chain_id })
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub async fn latest_block(&self) -> Result<u64> {
        Ok(self.provider.get_block_number().await?)
    }

    /// Block hash + parent hash without transaction bodies (cheap, for reorg walks).
    pub async fn header_hashes(&self, number: u64) -> Result<([u8; 32], [u8; 32])> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_else(|| eyre!("block {number} not found"))?;
        Ok((block.header.hash.0, block.header.parent_hash.0))
    }

    /// Fetch one block's full bundle with retries and exponential backoff.
    pub async fn fetch_bundle(&self, number: u64) -> Result<BlockBundle> {
        self.fetch_outcome(number, false).await.map(|o| o.bundle)
    }

    /// Fetch a block's Parity-style traces (internal txs, creates,
    /// selfdestructs) with retries. Requires an endpoint with `trace_block`.
    pub async fn fetch_traces(&self, number: u64) -> Result<Vec<TraceRow>> {
        self.fetch_outcome(number, true).await.map(|o| o.traces.unwrap_or_default())
    }

    /// Bundle + optional traces in one call, reporting whether any attempt hit
    /// a throttle-class error so the caller can back its concurrency off.
    pub async fn fetch_outcome(&self, number: u64, want_traces: bool) -> Result<BundleOutcome> {
        let mut delay = Duration::from_millis(500);
        let mut throttled = false;
        let mut last_err = None;
        for attempt in 0..RETRIES {
            match self.try_fetch_outcome(number, want_traces).await {
                Ok((bundle, traces)) => {
                    return Ok(BundleOutcome { bundle, traces, throttled })
                }
                Err(err) => {
                    if ratelimit::is_throttle_error(&err) {
                        throttled = true;
                    }
                    tracing::debug!(number, attempt, %err, "fetch failed, retrying");
                    last_err = Some(err);
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        Err(last_err
            .unwrap()
            .wrap_err(format!("block {number}: fetch failed after {RETRIES} attempts")))
    }

    async fn try_fetch_outcome(
        &self,
        number: u64,
        want_traces: bool,
    ) -> Result<(BlockBundle, Option<Vec<TraceRow>>)> {
        let (block, receipts) = tokio::try_join!(
            self.provider.get_block_by_number(BlockNumberOrTag::Number(number)).full(),
            self.provider.get_block_receipts(BlockId::number(number)),
        )?;
        let block = block.ok_or_else(|| eyre!("block {number} not found"))?;
        let receipts = receipts.ok_or_else(|| eyre!("receipts for block {number} not available"))?;
        let bundle = self.build_bundle(block, receipts)?;
        let traces = if want_traces { Some(self.try_fetch_traces(number).await?) } else { None };
        Ok((bundle, traces))
    }

    async fn try_fetch_traces(&self, number: u64) -> Result<Vec<TraceRow>> {
        let traces = self
            .provider
            .trace_block(BlockId::number(number))
            .await
            .map_err(|err| eyre!(err))?;
        let version = now_millis();
        Ok(traces.iter().filter_map(|t| build_trace_row(t, self.chain_id, number, version)).collect())
    }

    fn build_bundle(&self, block: Block, receipts: Vec<TransactionReceipt>) -> Result<BlockBundle> {
        let version = now_millis();
        let header = &block.header;
        let number = header.number;
        let block_hash = header.hash.0;

        let txs_src = block
            .transactions
            .as_transactions()
            .ok_or_else(|| eyre!("block {number} returned without full transactions"))?;

        let block_row = BlockRow {
            chain_id: self.chain_id,
            block_number: number,
            block_hash,
            parent_hash: header.parent_hash.0,
            timestamp: OffsetDateTime::from_unix_timestamp(header.timestamp as i64)?,
            miner: header.beneficiary.0 .0,
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
            base_fee_per_gas: header.base_fee_per_gas,
            tx_count: txs_src.len() as u32,
            insert_version: version,
        };

        let by_hash: HashMap<_, _> =
            receipts.iter().map(|r| (r.transaction_hash, r)).collect();

        let mut txs = Vec::with_capacity(txs_src.len());
        let mut logs = Vec::new();
        for (idx, tx) in txs_src.iter().enumerate() {
            let hash = *tx.inner.tx_hash();
            let receipt = by_hash
                .get(&hash)
                .ok_or_else(|| eyre!("block {number}: missing receipt for tx {hash}"))?;
            let tx_index = idx as u32;

            txs.push(TxRow {
                chain_id: self.chain_id,
                block_number: number,
                block_hash,
                tx_index,
                tx_hash: hash.0,
                from_address: tx.inner.signer().0 .0,
                to_address: tx.inner.to().map(|a| a.0 .0),
                value: saturating_u128(tx.inner.value()),
                nonce: tx.inner.nonce(),
                gas_limit: tx.inner.gas_limit(),
                gas_used: receipt.gas_used,
                effective_gas_price: receipt.effective_gas_price,
                tx_type: tx.inner.tx_type() as u8,
                status: receipt.status() as u8,
                input: tx.inner.input().to_vec(),
                insert_version: version,
            });

            for log in receipt.inner.logs() {
                let topics = log.topics();
                logs.push(LogRow {
                    chain_id: self.chain_id,
                    block_number: number,
                    block_hash,
                    tx_hash: hash.0,
                    tx_index,
                    log_index: log.log_index.ok_or_else(|| eyre!("log without index"))? as u32,
                    address: log.address().0 .0,
                    topic0: topics.first().map(|t| t.0).unwrap_or([0u8; 32]),
                    topic1: topics.get(1).map(|t| t.0),
                    topic2: topics.get(2).map(|t| t.0),
                    topic3: topics.get(3).map(|t| t.0),
                    data: log.data().data.to_vec(),
                    insert_version: version,
                });
            }
        }

        Ok(BlockBundle { block: block_row, txs, logs })
    }
}

fn saturating_u128(v: alloy::primitives::U256) -> u128 {
    v.try_into().unwrap_or(u128::MAX)
}

fn build_trace_row(
    t: &LocalizedTransactionTrace,
    chain_id: u64,
    block_number: u64,
    version: u64,
) -> Option<TraceRow> {
    let tx_hash = t.transaction_hash.map(|h| h.0).unwrap_or([0; 32]);
    let tx_index = t
        .transaction_position
        .map(|p| p as u32)
        .unwrap_or(u32::MAX);
    let trace_address = t
        .trace
        .trace_address
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("_");
    let subtraces = t.trace.subtraces as u32;
    let error = t.trace.error.clone();

    let (kind, call_type, from, to, created_contract, value, gas, gas_used, input, output, reward_type, refund_address) =
        match &t.trace.action {
            Action::Call(call) => {
                let (gas_used, output) = match &t.trace.result {
                    Some(TraceOutput::Call(out)) => (out.gas_used, out.output.to_vec()),
                    _ => (0, Vec::new()),
                };
                (
                    "call",
                    call.call_type.to_string(),
                    call.from.0 .0,
                    Some(call.to.0 .0),
                    None,
                    saturating_u128(call.value),
                    call.gas,
                    gas_used,
                    call.input.to_vec(),
                    output,
                    String::new(),
                    None,
                )
            }
            Action::Create(create) => {
                let (created, gas_used, output) = match &t.trace.result {
                    Some(TraceOutput::Create(out)) => {
                        (Some(out.address.0 .0), out.gas_used, out.code.to_vec())
                    }
                    _ => (None, 0, Vec::new()),
                };
                (
                    "create",
                    String::new(),
                    create.from.0 .0,
                    None,
                    created,
                    saturating_u128(create.value),
                    create.gas,
                    gas_used,
                    create.init.to_vec(),
                    output,
                    String::new(),
                    None,
                )
            }
            Action::Selfdestruct(sd) => (
                "selfdestruct",
                String::new(),
                sd.address.0 .0,
                None,
                None,
                saturating_u128(sd.balance),
                0,
                0,
                Vec::new(),
                Vec::new(),
                String::new(),
                Some(sd.refund_address.0 .0),
            ),
            Action::Reward(reward) => (
                "reward",
                String::new(),
                [0; 20],
                Some(reward.author.0 .0),
                None,
                saturating_u128(reward.value),
                0,
                0,
                Vec::new(),
                Vec::new(),
                format!("{:?}", reward.reward_type).to_lowercase(),
                None,
            ),
        };

    Some(TraceRow {
        chain_id,
        block_number,
        tx_hash,
        tx_index,
        trace_address,
        kind: kind.to_string(),
        call_type,
        from_address: from,
        to_address: to,
        created_contract,
        value,
        gas,
        gas_used,
        error,
        subtraces,
        input,
        output,
        reward_type,
        refund_address,
        insert_version: version,
    })
}
