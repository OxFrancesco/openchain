pub mod decode;

use alloy::consensus::Transaction as _;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::Ethereum;
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::{Block, TransactionReceipt};
use eyre::{eyre, Context, Result};
use openchain_core::{now_millis, BlockBundle, BlockRow, LogRow, TxRow};
use std::collections::HashMap;
use std::time::Duration;
use time::OffsetDateTime;

const RETRIES: u32 = 4;

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
        let mut delay = Duration::from_millis(500);
        let mut last_err = None;
        for attempt in 0..RETRIES {
            match self.try_fetch_bundle(number).await {
                Ok(bundle) => return Ok(bundle),
                Err(err) => {
                    tracing::debug!(number, attempt, %err, "fetch failed, retrying");
                    last_err = Some(err);
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        Err(last_err.unwrap().wrap_err(format!("block {number}: fetch failed after {RETRIES} attempts")))
    }

    async fn try_fetch_bundle(&self, number: u64) -> Result<BlockBundle> {
        let (block, receipts) = tokio::try_join!(
            self.provider.get_block_by_number(BlockNumberOrTag::Number(number)).full(),
            self.provider.get_block_receipts(BlockId::number(number)),
        )?;
        let block = block.ok_or_else(|| eyre!("block {number} not found"))?;
        let receipts = receipts.ok_or_else(|| eyre!("receipts for block {number} not available"))?;
        self.build_bundle(block, receipts)
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
