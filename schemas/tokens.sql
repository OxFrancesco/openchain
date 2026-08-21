CREATE TABLE IF NOT EXISTS tokens (
    chain_id UInt64,
    address FixedString(20),
    symbol String,
    name String,
    decimals UInt8,
    source LowCardinality(String),
    updated_at DateTime CODEC(Delta, ZSTD(1))
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY (chain_id, address)
