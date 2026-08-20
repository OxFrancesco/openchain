CREATE TABLE IF NOT EXISTS sync_status (
    chain_id UInt64,
    dataset LowCardinality(String),
    last_synced_block UInt64,
    version UInt64
) ENGINE = ReplacingMergeTree(version)
ORDER BY (chain_id, dataset)
