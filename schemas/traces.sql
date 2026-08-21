CREATE TABLE IF NOT EXISTS traces (
    chain_id UInt64,
    block_number UInt64 CODEC(Delta, ZSTD(1)),
    tx_hash FixedString(32),
    tx_index UInt32,
    trace_address LowCardinality(String),
    kind LowCardinality(String),
    call_type LowCardinality(String),
    from_address FixedString(20),
    to_address Nullable(FixedString(20)),
    created_contract Nullable(FixedString(20)),
    value UInt128,
    gas UInt64,
    gas_used UInt64 CODEC(Delta, ZSTD(1)),
    error Nullable(String),
    subtraces UInt32,
    input String,
    output String,
    reward_type LowCardinality(String),
    refund_address Nullable(FixedString(20)),
    insert_version UInt64 CODEC(Delta, ZSTD(1))
) ENGINE = ReplacingMergeTree(insert_version)
ORDER BY (chain_id, block_number, tx_index, trace_address)
