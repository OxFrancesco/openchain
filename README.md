# OpenChain

Open-source, self-hostable onchain analytics engine. Backfills EVM chains into
ClickHouse, tails the head live with reorg handling, and ABI-decodes raw logs
into queryable event tables.

```
RPC ──sync──▶ ClickHouse ──decode──▶ decoded_events
 ▲                │
 └── follow (WS newHeads + polling fallback, reorg-safe)
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
openchain sync --chain 1            # backfill (resumable)
openchain abi add 0xdAC17F958D2ee523a2206206994597C13D831ec7 --chain 1
openchain decode --chain 1          # decode raw logs for registered ABIs
openchain sql "SELECT count() FROM logs WHERE chain_id = 1"
openchain follow --chain 1          # tail the head live
```

## Commands

| command | what it does |
|---|---|
| `init` | Write default config and create the ClickHouse schema |
| `sync` | Backfill a block range into `blocks`/`transactions`/`logs` (resumable watermarks) |
| `follow` | Tail the chain head live; WS `newHeads` when available, HTTP polling fallback, reorg rewind |
| `abi add/list` | Register contract ABIs (Sourcify or local JSON) used by `decode` |
| `decode` | Incrementally decode raw logs into `decoded_events` (parallel across cores) |
| `sql` | Run SQL against the OpenChain database |

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
(M1 Pro, free public RPC): ~13 blocks/s backfill (RPC-bound), ~120k logs/s
single-batch decode, per-contract log queries at ~3 ms, sub-6s average
freshness in follow mode.

## Layout

| crate | role |
|---|---|
| `openchain-core` | Config, row types, dataset model |
| `openchain-evm` | JSON-RPC source, ABI event decoding |
| `openchain-sink` | ClickHouse schema + inserts |
| `openchain-cli` | The `openchain` binary |

## License

MIT OR Apache-2.0
