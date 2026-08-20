use clickhouse::{Client, Row};
use eyre::{Context, Result};
use openchain_core::{now_millis, BlockBundle, ClickHouseConfig, Dataset};
use serde::{Deserialize, Serialize};

const SCHEMAS: &[&str] = &[
    include_str!("../../../schemas/blocks.sql"),
    include_str!("../../../schemas/transactions.sql"),
    include_str!("../../../schemas/logs.sql"),
    include_str!("../../../schemas/sync_status.sql"),
];

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

    /// Delete all rows at or above `from_block` across every dataset.
    /// Used to rewind after a reorg before re-inserting canonical rows.
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
        }
        self.set_watermark(chain_id, &Dataset::ALL, from_block.saturating_sub(1)).await
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
