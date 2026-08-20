CREATE TABLE IF NOT EXISTS decoded_events (
    chain_id UInt64,
    block_number UInt64,
    tx_hash FixedString(32),
    tx_index UInt32,
    log_index UInt32,
    address FixedString(20),
    contract_name LowCardinality(String),
    event_name LowCardinality(String),
    full_signature LowCardinality(String),
    params String,
    insert_version UInt64
) ENGINE = ReplacingMergeTree(insert_version)
ORDER BY (chain_id, address, event_name, block_number, log_index)
