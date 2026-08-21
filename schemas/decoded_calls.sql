CREATE TABLE IF NOT EXISTS decoded_calls (
    chain_id UInt64,
    block_number UInt64 CODEC(Delta, ZSTD(1)),
    tx_hash FixedString(32),
    tx_index UInt32,
    trace_address LowCardinality(String),
    address FixedString(20),
    contract_name LowCardinality(String),
    function_name LowCardinality(String),
    full_signature LowCardinality(String),
    params String,
    succeeded UInt8,
    insert_version UInt64 CODEC(Delta, ZSTD(1))
) ENGINE = ReplacingMergeTree(insert_version)
ORDER BY (chain_id, address, function_name, block_number, tx_index)
