use clickhouse::Row;
use eyre::{bail, Result};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Datasets OpenChain can sync. All three are produced from the same
/// block + receipts fetch, so selecting fewer datasets only reduces writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dataset {
    Blocks,
    Transactions,
    Logs,
    /// Derived dataset, produced by `openchain decode` rather than sync.
    DecodedEvents,
}

impl Dataset {
    /// Every block-scoped table, i.e. everything a reorg rewind must touch.
    pub const ALL: [Dataset; 4] =
        [Dataset::Blocks, Dataset::Transactions, Dataset::Logs, Dataset::DecodedEvents];

    pub fn table(&self) -> &'static str {
        match self {
            Dataset::Blocks => "blocks",
            Dataset::Transactions => "transactions",
            Dataset::Logs => "logs",
            Dataset::DecodedEvents => "decoded_events",
        }
    }

    pub fn parse_list(s: &str) -> Result<Vec<Dataset>> {
        let mut out = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            out.push(match part {
                "blocks" => Dataset::Blocks,
                "transactions" | "txs" => Dataset::Transactions,
                "logs" => Dataset::Logs,
                other => bail!("unknown dataset '{other}' (expected blocks, transactions, logs)"),
            });
        }
        if out.is_empty() {
            bail!("no datasets selected");
        }
        Ok(out)
    }
}

impl std::fmt::Display for Dataset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.table())
    }
}

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct BlockRow {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: [u8; 32],
    pub parent_hash: [u8; 32],
    #[serde(with = "clickhouse::serde::time::datetime")]
    pub timestamp: OffsetDateTime,
    pub miner: [u8; 20],
    pub gas_used: u64,
    pub gas_limit: u64,
    pub base_fee_per_gas: Option<u64>,
    pub tx_count: u32,
    pub insert_version: u64,
}

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct TxRow {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: [u8; 32],
    pub tx_index: u32,
    pub tx_hash: [u8; 32],
    pub from_address: [u8; 20],
    pub to_address: Option<[u8; 20]>,
    /// Wei. u128 comfortably covers ETH's total supply.
    pub value: u128,
    pub nonce: u64,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub effective_gas_price: u128,
    pub tx_type: u8,
    pub status: u8,
    #[serde(with = "serde_bytes")]
    pub input: Vec<u8>,
    pub insert_version: u64,
}

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct LogRow {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: [u8; 32],
    pub tx_hash: [u8; 32],
    pub tx_index: u32,
    pub log_index: u32,
    pub address: [u8; 20],
    /// Zero-filled for the rare anonymous log with no topics.
    pub topic0: [u8; 32],
    pub topic1: Option<[u8; 32]>,
    pub topic2: Option<[u8; 32]>,
    pub topic3: Option<[u8; 32]>,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    pub insert_version: u64,
}

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DecodedEventRow {
    pub chain_id: u64,
    pub block_number: u64,
    pub tx_hash: [u8; 32],
    pub tx_index: u32,
    pub log_index: u32,
    pub address: [u8; 20],
    pub contract_name: String,
    pub event_name: String,
    pub full_signature: String,
    /// Decoded parameters as a JSON object keyed by parameter name.
    pub params: String,
    pub insert_version: u64,
}

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct AbiRow {
    pub chain_id: u64,
    pub address: [u8; 20],
    pub name: String,
    pub abi: String,
    pub source: String,
    pub insert_version: u64,
}

/// Everything extracted from one block: header row, transaction rows, log rows.
#[derive(Debug, Clone)]
pub struct BlockBundle {
    pub block: BlockRow,
    pub txs: Vec<TxRow>,
    pub logs: Vec<LogRow>,
}

impl BlockBundle {
    pub fn number(&self) -> u64 {
        self.block.block_number
    }
}
