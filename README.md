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

## Ask in flags, not SQL

Every "show me …" question maps to a couple of obvious flags — no SQL needed,
no ABI registration required for transfers:

| you want | you type |
|---|---|
| all USDT transfers over 100k in the last 3 months | `openchain transfers --chain 1 --token usdt --since 3m --min-value 100k` |
| USDT sent *to* an address last week | `openchain transfers --chain 1 --token usdt --to 0xA9D1…3eef --since 7d` |
| USDT sent to vitalik.eth (ENS works) | `openchain transfers --chain 1 --token usdt --to vitalik.eth` |
| every big stablecoin move today, all configured chains | `openchain transfers --token usdt,usdc,dai --since 24h --min-value 10m` |
| who received the most USDT in the last 24 hours | `openchain top --chain 1 --token usdt --since 24h` |
| biggest ETH senders today | `openchain top --native --side senders --since 24h` |
| txs from an address worth over 10 ETH in a block range | `openchain txs --chain 1 --from 0x… --min-value 10eth --blocks 25800000..25803202` |
| reverted txs to a contract yesterday | `openchain txs --chain 1 --to 0x… --status reverted --until 2026-08-20` |
| WETH deposits in the last hour | `openchain events --chain 1 --event Deposit --contract WETH9 --since 1h` |
| just the number | append `--count` |
| machine output | append `--json`, `--jsonl`, or `--csv` |
| see (or tweak) the SQL behind it | append `--sql` |

Flag grammar, shared by all query commands:

- **time**: `--since` / `--until` take durations (`90s`, `30min`, `24h`, `7d`,
  `3w`, `3m` months, `1y`) or dates (`2024-01-01`, `2024-01-01T12:00:00Z`).
  `--blocks A..B` overrides time with an explicit range (open ends OK).
- **amounts**: human suffixes — `100k`, `1.5m`, `0.5b`. Token amounts are in
  token units; native ETH amounts accept `wei`/`gwei`/`eth`.
- **tokens**: `--token` takes a symbol (`usdt`) or contract address. Symbols
  resolve from a built-in list of majors; unknown addresses get decimals
  fetched on-chain once and cached in the `tokens` table.
- **addresses**: `--from`/`--to` accept ENS names (`vitalik.eth`) — resolved
  through the `[chains.1]` RPC — or plain `0x` addresses.
- **chains**: `--chain` is optional on query commands. Omit it to fan out
  across every chain in `openchain.toml`; chains without synced data or
  without that token are skipped with a note, results merge newest-first.
- **output**: table for humans; `--json`/`--jsonl`/`--csv` carry full-precision
  values plus both raw and decimal amounts for machines.

```bash
$ openchain transfers --chain 1 --token usdt --since 3m --min-value 100k --limit 4
age    value (USDT)   from             to               tx               block
---    ------------   -------------    -------------    -------------    --------
32m    201,700.50     0x99a3b8fd…9131  0xbbbbbbbb…ffcb  0xdb917fff…e4a8  25803202
32m    201,563.80     0x585d4472…8b4f  0x99a3b8fd…9131  0xdb917fff…e4a8  25803202
31m    140,699.36     0x8f10b468…f996  0x00000000…8a90  0xdb917fff…e4a8  25803202
30m    2,024,000.00   0x424b149f…5c36  0x11d863b9…805e  0x89923064…aaa7  25803201

4 rows
```

Rankings work too:

```bash
$ openchain top --chain 1 --token usdt --since 24h --limit 5
#   recipient (USDT)        total (USDT)   txs
-  -----------------  ------------------  ----
1    0xBBBBBBBB…FFCB  303,446,282.06903   241
2    0x28C6C062…1D60  134,212,522.321648  2827
3    0x06CFF708…F5EF  123,438,457.357841   126
4    0x77134CBC…35EC  119,927,320.448686    36
5    0x4B47D729…21B3  100,580,483.879971    22

5 rows
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
| `decode` | Incrementally decode raw logs into `decoded_events` and call inputs into `decoded_calls` (parallel across cores) |
| `transfers` | Token transfers with human flags: `--token usdt --since 3m --min-value 100k` |
| `txs` | Transactions with human flags: `--from 0x… --since 7d --min-value 10eth --status reverted` |
| `events` | Decoded events with human flags: `--event Swap --contract univ3 --since 30d` |
| `top` | Rank biggest movers: `top --token usdt --since 24h [--side senders] [--native]` |
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

## TUI

`openchain tui` opens an interactive dashboard over the same query layer —
no flags to remember, filters are editable in place:

```bash
openchain tui                # or: openchain tui --chain 8453
```

| key | action |
|---|---|
| `1-5` / `Tab` | switch view: status · transfers · txs · events · top |
| `enter` | run the query for this view |
| `f` | edit filters (Tab between fields, Enter runs) — same grammar as the CLI: `30d`, `100k`, `10eth`, ENS names |
| `↑ ↓` | scroll results |
| `s` | show the generated SQL for the current view |
| `r` | refresh · `c` cycle chains · `?` help · `q` quit |

The status view auto-refreshes every 10 s with per-dataset row counts and head
lag; transfers pre-loads on startup so data is on screen immediately.

## Performance

Measured numbers live in [Performances.md](Performances.md). Highlights
(M1 Pro, free public RPC): ~13 blocks/s backfill when the endpoint allows it
with adaptive throttling 2.6x faster than fixed concurrency under load,
~120k logs/s end-to-end decode (2.6x on decode CPU), per-contract log
queries at ~3 ms, sub-second best-case freshness in follow mode.

## Dune parity roadmap

Initial parity target tracked in [Performances.md](Performances.md):
raw tables (blocks/txs/logs/traces), decoded events, and decoded calls
are in; next up are multi-chain fan-out ergonomics and a reth ExEx source
for sub-second, rate-limit-free ingestion.

## Layout

| crate | role |
|---|---|
| `openchain-core` | Config, row types, dataset model |
| `openchain-evm` | JSON-RPC source, ABI event decoding |
| `openchain-sink` | ClickHouse schema + inserts |
| `openchain-cli` | The `openchain` binary |

## License

MIT OR Apache-2.0
