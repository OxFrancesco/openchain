# Performances

OpenChain must be fast AF. This file tracks measured numbers so speed regressions
are visible and claims stay honest. Update it whenever ingestion, decoding, or the
storage layout changes. Never publish a number here that wasn't measured.

## Test environment

| | |
|---|---|
| Date | 2026-08-20 |
| Machine | Apple M1 Pro, 8 cores, 16 GB RAM |
| ClickHouse | 26.7.4 (Docker, single node, default settings) |
| RPC | `ethereum-rpc.publicnode.com` (free public endpoint) |
| Build | `cargo build --release` (thin LTO) |
| Dataset | Ethereum mainnet, 551 blocks, 150,902 txs, 436,439 logs |

## 1. Sync (backfill) throughput

`openchain sync` on fresh 100-block mainnet ranges, wall-clock timed. Each block
costs 2 RPC calls (`eth_getBlockByNumber` full + `eth_getBlockReceipts`).

| concurrency | chunk | blocks/s | txs/s | logs/s | notes |
|---|---|---|---|---|---|
| 2 | 5 | 6.6 | ~1,700 | ~5,400 | under-parallelized |
| **4 (default)** | **10** | **13.5** | **~3,900** | **~11,300** | sweet spot on publicnode |
| 8 | 10 | 11.0 | ~3,000 | ~7,700 | throttling begins |
| 12 | 25 | 2.5 | ~680 | ~2,000 | rate-limited hard, retries dominate |

**Bottleneck: the RPC endpoint, not OpenChain.** CPU usage during the default run
was 1.1s user over 7.4s wall (~15% of one core). The free publicnode endpoint
rate-limits aggressively above ~8 concurrent block fetches. Against a local reth
node these numbers should be 1-2 orders of magnitude higher; re-measure when we
have one (see backlog).

## 2. Decode throughput

`openchain decode` full pipeline: read logs from ClickHouse, ABI-decode with
alloy dyn-abi, serialize params to JSON, insert into `decoded_events`.
3 registered contracts (USDT, USDC impl, WETH), single-threaded.

| metric | value |
|---|---|
| logs decoded | 164,442 |
| wall time | 1.33 s |
| **throughput** | **123,296 logs/s** |
| failures | 0 |

At this rate, all ~4.5B historical Ethereum logs would decode in ~10 hours on
one core. Parallelizing decode across cores with rayon is the obvious next step
(see backlog).

## 3. Query latency

ClickHouse-reported elapsed time (`statistics.elapsed`, FORMAT JSON), warm cache.

| query | latency | rows read |
|---|---|---|
| `count()` over logs | 11.6 ms | 1 (metadata) |
| top-5 Transfer emitters (topic0 filter + group by) | 22.0 ms | 436,439 |
| USDT transfer volume via `JSONExtractFloat` on decoded_events | 26.1 ms | 164,442 |
| logs for one contract address | **3.3 ms** | 48,755 |

The address-filtered query read only 48,755 of 436,439 rows: the
`(chain_id, address, topic0, block_number)` sort key prunes at the granule
level exactly as designed. This is the property that keeps per-contract
queries fast at billions of rows.

## 4. Follow-mode freshness

Delay between a block's onchain timestamp and its ClickHouse insert
(`insert_version - block.timestamp`), 7 live blocks, 3s HTTP polling:

| min | avg | max |
|---|---|---|
| 2.1 s | 5.4 s | 16.5 s |

Block timestamps mark slot start; propagation to a public RPC alone costs
~1-2s, so ~2s is near the floor for HTTP polling. Dune's freshness for
decoded tables is minutes. WS subscriptions and the reth ExEx path (sub-second)
are the planned upgrades.

## 5. Storage

| table | rows | on disk | bytes/row |
|---|---|---|---|
| logs | 436,439 | 28.8 MiB | ~69 |
| transactions | 150,902 | 31.8 MiB | ~221 |
| decoded_events | 164,442 | 12.3 MiB | ~79 |
| blocks | 551 | 68.5 KiB | ~127 |

Fixed-width binary columns (`FixedString(20/32)`) + ClickHouse compression.
Extrapolated, full mainnet logs (~4.5B rows) ≈ ~300 GB before tuning
codecs (ZSTD, delta encoding on block_number) — very manageable self-hosted.

## Reproduce

```bash
# 1. sync benchmark (pick a fresh range, time it)
time openchain sync --chain 1 --from <N> --to <N+99>

# 2. decode benchmark (reset decoded state, re-run)
openchain sql "TRUNCATE TABLE decoded_events"
openchain sql "INSERT INTO sync_status SELECT 1,'decoded_events',0,toUnixTimestamp64Milli(now64(3))"
time openchain decode --chain 1 --batch-blocks 10000

# 3. query latency (ClickHouse-side elapsed)
openchain sql "<query> FORMAT JSON"   # read statistics.elapsed

# 4. follow freshness
openchain sql "SELECT min(d), avg(d), max(d) FROM (SELECT insert_version/1000 - toUnixTimestamp(timestamp) AS d FROM blocks FINAL WHERE block_number >= <first followed block>)"
```

## Optimization backlog (ordered by expected impact)

1. **reth ExEx / local node source** — removes the RPC bottleneck entirely; sync
   throughput should jump from ~13 to hundreds of blocks/s, freshness to sub-second.
2. **Parallel decode with rayon** — decode is single-threaded today; 8 cores ≈ 8x.
3. **WS `newHeads` subscription in follow** — cuts the 0-3s polling delay.
4. **Column codecs** — `CODEC(Delta, ZSTD)` on block_number/timestamp columns,
   ZSTD level tuning; expect 30-50% smaller logs table.
5. **Batched RowBinary inserts with larger buffers** — insert overhead is currently
   negligible at these volumes; revisit at >100k rows/s sustained.
6. **Per-endpoint adaptive rate limiting** — replace fixed retry backoff with a
   governor that finds the endpoint's sustainable rate automatically.
