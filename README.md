# OpenChain

Open-source, self-hostable onchain analytics engine. Backfills EVM chains into
ClickHouse, tails the head live with reorg handling, and ABI-decodes raw logs
into queryable event tables.

```
RPC ──sync──▶ ClickHouse ──decode──▶ decoded_events
 ▲                │
 └── follow (WS newHeads + polling fallback, reorg-safe)

datasets: blocks · transactions · logs · traces (internal txs / call tree)
```

## Why

Dune-style analytics locked behind a SaaS login. OpenChain is the self-hosted
alternative: your RPC endpoint, your ClickHouse, your SQL. Sort keys are laid
out so per-contract queries stay fast at billions of rows.

## Quick start

Requires Rust and a running ClickHouse instance.

```bash
cargo install --path crates/openchain-cli

openchain init                      # writes openchain.toml + creates schema
openchain sync --chain 1            # backfill (resumable, adaptive rate limiting)
openchain status --chain 1          # per-dataset rows, ranges, watermarks, head lag

# traces (internal transactions) need a tracing endpoint:
#   publicnode requires a paid token; drpc's free tier works
openchain sync --chain 1 --datasets blocks,transactions,logs,traces

openchain abi add 0xdAC17F958D2ee523a2206206994597C13D831ec7 --chain 1
openchain decode --chain 1          # decode raw logs for registered ABIs
openchain sql "SELECT count() FROM traces WHERE chain_id = 1"
openchain follow --chain 1 --datasets blocks,transactions,logs  # tail the head live
```

## Commands

| command | what it does |
|---|---|
| `init` | Write default config and create the ClickHouse schema |
| `sync` | Backfill a block range (resumable watermarks); `--datasets` picks blocks/transactions/logs/traces |
| `follow` | Tail the chain head live; WS `newHeads` when available, HTTP polling fallback, reorg rewind |
| `status` | Per-dataset row counts, block ranges, watermarks, and head lag |
| `abi add/list` | Register contract ABIs (Sourcify or local JSON) used by `decode` |
| `decode` | Incrementally decode raw logs into `decoded_events` (parallel across cores) |
| `sql` | Run SQL against the OpenChain database |

Sync concurrency adapts to the endpoint automatically: it starts at
`--concurrency`, grows +25% per 8 clean fetches up to `--max-concurrency`,
and halves when the endpoint signals throttling.

## Configuration

`openchain.toml`:

```toml
[clickhouse]
url = "http://localhost:8123"
database = "openchain"
user = "default"
password = ""

[chains.1]
name = "ethereum"
rpc = "https://ethereum-rpc.publicnode.com"
# ws = "wss://your-ws-endpoint"   # optional; defaults to rpc with scheme swapped
```

## Performance

Measured numbers live in [Performances.md](Performances.md). Highlights
(M1 Pro, free public RPC): ~13 blocks/s backfill when the endpoint allows it
with adaptive throttling 2.6x faster than fixed concurrency under load,
~120k logs/s end-to-end decode (2.6x on decode CPU), per-contract log
queries at ~3 ms, sub-second best-case freshness in follow mode.

## Dune parity roadmap

Initial parity target tracked in [Performances.md](Performances.md):
raw tables (blocks/txs/logs/traces) and decoded events are in; next up are
decoded calls (ABI-decoded `traces.input`), multi-chain fan-out ergonomics,
and a reth ExEx source for sub-second, rate-limit-free ingestion.

## Layout

| crate | role |
|---|---|
| `openchain-core` | Config, row types, dataset model |
| `openchain-evm` | JSON-RPC source, ABI event decoding |
| `openchain-sink` | ClickHouse schema + inserts |
| `openchain-cli` | The `openchain` binary |

## License

MIT OR Apache-2.0
