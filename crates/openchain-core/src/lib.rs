pub mod config;
pub mod types;

pub use config::{ChainConfig, ClickHouseConfig, Config};
pub use types::{BlockBundle, BlockRow, Dataset, LogRow, TxRow};

/// Current unix time in milliseconds, used as the ReplacingMergeTree version.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_millis() as u64
}
