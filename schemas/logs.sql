CREATE TABLE IF NOT EXISTS logs (
    chain_id UInt64,
    block_number UInt64,
    block_hash FixedString(32),
    tx_hash FixedString(32),
    tx_index UInt32,
    log_index UInt32,
    address FixedString(20),
    topic0 FixedString(32),
    topic1 Nullable(FixedString(32)),
    topic2 Nullable(FixedString(32)),
    topic3 Nullable(FixedString(32)),
    data String,
    insert_version UInt64
) ENGINE = ReplacingMergeTree(insert_version)
ORDER BY (chain_id, address, topic0, block_number, log_index)
