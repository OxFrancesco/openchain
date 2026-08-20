CREATE TABLE IF NOT EXISTS abis (
    chain_id UInt64,
    address FixedString(20),
    name String,
    abi String,
    source LowCardinality(String),
    insert_version UInt64
) ENGINE = ReplacingMergeTree(insert_version)
ORDER BY (chain_id, address)
