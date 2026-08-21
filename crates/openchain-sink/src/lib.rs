use clickhouse::{Client, Row};
use eyre::{Context, Result};
use openchain_core::{
    now_millis, AbiRow, BlockBundle, ClickHouseConfig, Dataset, DecodedEventRow, LogRow, TraceRow,
};
use serde::{Deserialize, Serialize};

const SCHEMAS: &[&str] = &[
    include_str!("../../../schemas/blocks.sql"),
    include_str!("../../../schemas/transactions.sql"),
    include_str!("../../../schemas/logs.sql"),
    include_str!("../../../schemas/traces.sql"),
    include_str!("../../../schemas/decoded_events.sql"),
    include_str!("../../../schemas/abis.sql"),
    include_str!("../../../schemas/sync_status.sql"),
];

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Row, Serialize)]
struct WatermarkRow {
    chain_id: u64,
    dataset: String,
    last_synced_block: u64,
    version: u64,
}

#[derive(Row, Deserialize)]
pub struct BlockHashRow {
    pub block_number: u64,
    pub block_hash: [u8; 32],
}

#[derive(Debug)]
pub struct DatasetStats {
    pub dataset: Dataset,
    pub watermark: Option<u64>,
    pub rows: u64,
    pub min_block: u64,
    pub max_block: u64,
}

pub struct Sink {
    client: Client,
    config: ClickHouseConfig,
}

impl Sink {
    pub fn new(config: &ClickHouseConfig) -> Self {
        let client = Client::default()
            .with_url(&config.url)
            .with_database(&config.database)
            .with_user(&config.user)
            .with_password(&config.password);
        Sink { client, config: config.clone() }
    }

    /// Create the database (if missing) and all tables.
    pub async fn ensure_schema(&self) -> Result<()> {
        let admin = Client::default()
            .with_url(&self.config.url)
            .with_user(&self.config.user)
            .with_password(&self.config.password);
        admin
            .query(&format!("CREATE DATABASE IF NOT EXISTS {}", self.config.database))
            .execute()
            .await
            .wrap_err("cannot create database (is ClickHouse running?)")?;
        for ddl in SCHEMAS {
            self.client.query(ddl).execute().await.wrap_err("cannot apply schema")?;
        }
        Ok(())
    }

    /// Insert a batch of block bundles, writing only the selected datasets.
    pub async fn insert_bundles(&self, bundles: &[BlockBundle], datasets: &[Dataset]) -> Result<()> {
        if datasets.contains(&Dataset::Blocks) {
            let mut insert = self.client.insert::<openchain_core::BlockRow>("blocks").await?;
            for b in bundles {
                insert.write(&b.block).await?;
            }
            insert.end().await?;
        }
        if datasets.contains(&Dataset::Transactions) {
            let mut insert = self.client.insert::<openchain_core::TxRow>("transactions").await?;
            for b in bundles {
                for tx in &b.txs {
                    insert.write(tx).await?;
                }
            }
            insert.end().await?;
        }
        if datasets.contains(&Dataset::Logs) {
            let mut insert = self.client.insert::<openchain_core::LogRow>("logs").await?;
            for b in bundles {
                for log in &b.logs {
                    insert.write(log).await?;
                }
            }
            insert.end().await?;
        }
        Ok(())
    }

    /// Latest fully synced block for a chain + dataset, if any.
    pub async fn watermark(&self, chain_id: u64, dataset: Dataset) -> Result<Option<u64>> {
        let rows: Vec<u64> = self
            .client
            .query("SELECT last_synced_block FROM sync_status FINAL WHERE chain_id = ? AND dataset = ?")
            .bind(chain_id)
            .bind(dataset.table())
            .fetch_all()
            .await?;
        Ok(rows.into_iter().next())
    }

    pub async fn set_watermark(&self, chain_id: u64, datasets: &[Dataset], block: u64) -> Result<()> {
        let mut insert = self.client.insert::<WatermarkRow>("sync_status").await?;
        for dataset in datasets {
            insert
                .write(&WatermarkRow {
                    chain_id,
                    dataset: dataset.table().to_string(),
                    last_synced_block: block,
                    version: now_millis(),
                })
                .await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Delete all rows at or above `from_block` across every block-scoped table.
    /// Used to rewind after a reorg before re-inserting canonical rows.
    /// Watermarks only ever move down here (a dataset that never reached the
    /// rewind point keeps its lower watermark).
    pub async fn rewind(&self, chain_id: u64, from_block: u64) -> Result<()> {
        tracing::warn!(chain_id, from_block, "rewinding datasets for reorg");
        for dataset in Dataset::ALL {
            self.client
                .query(&format!(
                    "DELETE FROM {} WHERE chain_id = ? AND block_number >= ?",
                    dataset.table()
                ))
                .bind(chain_id)
                .bind(from_block)
                .execute()
                .await?;
            if let Some(current) = self.watermark(chain_id, dataset).await? {
                let target = current.min(from_block.saturating_sub(1));
                self.set_watermark(chain_id, &[dataset], target).await?;
            }
        }
        Ok(())
    }

    pub async fn insert_abi(&self, row: &AbiRow) -> Result<()> {
        let mut insert = self.client.insert::<AbiRow>("abis").await?;
        insert.write(row).await?;
        insert.end().await?;
        Ok(())
    }

    pub async fn list_abis(&self, chain_id: u64) -> Result<Vec<AbiRow>> {
        Ok(self
            .client
            .query("SELECT * FROM abis FINAL WHERE chain_id = ? ORDER BY name")
            .bind(chain_id)
            .fetch_all::<AbiRow>()
            .await?)
    }

    /// Logs for a set of contract addresses in a block range. Exploits the
    /// logs table sort key (chain_id, address, topic0, block_number).
    pub async fn logs_for_addresses(
        &self,
        chain_id: u64,
        addresses: &[[u8; 20]],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<LogRow>> {
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let addr_list = addresses
            .iter()
            .map(|a| format!("unhex('{}')", hex_lower(a)))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(self
            .client
            .query(&format!(
                "SELECT * FROM logs FINAL WHERE chain_id = ? AND address IN ({addr_list}) \
                 AND block_number BETWEEN ? AND ? ORDER BY block_number, log_index"
            ))
            .bind(chain_id)
            .bind(from_block)
            .bind(to_block)
            .fetch_all::<LogRow>()
            .await?)
    }

    /// Lowest synced log block for a chain, used to clamp decode scans.
    pub async fn min_log_block(&self, chain_id: u64) -> Result<Option<u64>> {
        let rows: Vec<u64> = self
            .client
            .query("SELECT min(block_number) FROM logs WHERE chain_id = ? HAVING count() > 0")
            .bind(chain_id)
            .fetch_all()
            .await?;
        Ok(rows.into_iter().next())
    }

    pub async fn insert_decoded(&self, rows: &[DecodedEventRow]) -> Result<()> {
        let mut insert = self.client.insert::<DecodedEventRow>("decoded_events").await?;
        for row in rows {
            insert.write(row).await?;
        }
        insert.end().await?;
        Ok(())
    }

    pub async fn insert_traces(&self, rows: &[TraceRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut insert = self.client.insert::<TraceRow>("traces").await?;
        for row in rows {
            insert.write(row).await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Row count and block range per dataset, for `openchain status`.
    pub async fn dataset_stats(&self, chain_id: u64) -> Result<Vec<DatasetStats>> {
        let mut out = Vec::new();
        for dataset in Dataset::ALL {
            let watermark = self.watermark(chain_id, dataset).await?;
            let sql = format!(
                "SELECT count(), min(block_number), max(block_number) FROM {} WHERE chain_id = ? HAVING count() > 0",
                dataset.table()
            );
            let rows: Vec<(u64, u64, u64)> =
                self.client.query(&sql).bind(chain_id).fetch_all().await?;
            let (count, min_block, max_block) = rows.into_iter().next().unwrap_or((0, 0, 0));
            out.push(DatasetStats { dataset, watermark, rows: count, min_block, max_block });
        }
        Ok(out)
    }

    /// Recent canonical block hashes, used to seed the follow-mode hot window.
    pub async fn recent_block_hashes(&self, chain_id: u64, limit: u64) -> Result<Vec<BlockHashRow>> {
        let rows = self
            .client
            .query(
                "SELECT block_number, block_hash FROM blocks FINAL \
                 WHERE chain_id = ? ORDER BY block_number DESC LIMIT ?",
            )
            .bind(chain_id)
            .bind(limit)
            .fetch_all::<BlockHashRow>()
            .await?;
        Ok(rows)
    }
}
