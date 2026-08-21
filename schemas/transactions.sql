CREATE TABLE IF NOT EXISTS transactions (
    chain_id UInt64,
    block_number UInt64 CODEC(Delta, ZSTD(1)),
    block_hash FixedString(32),
    tx_index UInt32,
    tx_hash FixedString(32),
    from_address FixedString(20),
    to_address Nullable(FixedString(20)),
    value UInt128,
    nonce UInt64,
    gas_limit UInt64,
    gas_used UInt64,
    effective_gas_price UInt128,
    tx_type UInt8,
    status UInt8,
    input String,
    insert_version UInt64 CODEC(Delta, ZSTD(1))
) ENGINE = ReplacingMergeTree(insert_version)
ORDER BY (chain_id, block_number, tx_index)
