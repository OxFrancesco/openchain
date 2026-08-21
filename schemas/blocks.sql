CREATE TABLE IF NOT EXISTS blocks (
    chain_id UInt64,
    block_number UInt64 CODEC(Delta, ZSTD(1)),
    block_hash FixedString(32),
    parent_hash FixedString(32),
    timestamp DateTime CODEC(Delta, ZSTD(1)),
    miner FixedString(20),
    gas_used UInt64,
    gas_limit UInt64,
    base_fee_per_gas Nullable(UInt64),
    tx_count UInt32,
    insert_version UInt64 CODEC(Delta, ZSTD(1))
) ENGINE = ReplacingMergeTree(insert_version)
ORDER BY (chain_id, block_number)
