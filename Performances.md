# Performances

OpenChain must be fast AF. This file tracks measured numbers so speed regressions
are visible and claims stay honest. Update it whenever ingestion, decoding, or the
storage layout changes. Never publish a number here that wasn't measured.

## Test environment

| | |
|---|---|
| Date | 2026-08-21 |
| Machine | Apple M1 Pro, 8 cores, 16 GB RAM |
| ClickHouse | 26.7.4 (Docker, single node, default settings) |
| RPC | `ethereum-rpc.publicnode.com` (free public endpoint) |
| Build | `cargo build --release` (thin LTO) |
| Dataset | Ethereum mainnet, 551 blocks, 150,902 txs, 436,439 logs |

## 1. Sync (backfill) throughput

`openchain sync` on fresh 100-block mainnet ranges, wall-clock timed. Each block
costs 2 RPC calls (`eth_getBlockByNumber` full + `eth_getBlockReceipts`).
Numbers move with the endpoint's load, so comparisons are interleaved A/B runs
minutes apart.

| run | concurrency | blocks/s | notes |
|---|---|---|---|
| fixed (pre-governor) | 4 | 13.5 | quiet endpoint (2026-08-20) |
| fixed (pre-governor) | **10** | **2.5** | rate-limited hard, retries dominate |
| adaptive governor | 4 -> ~15 | 2.6-11 | tracks the endpoint's momentary ceiling |
| **adaptive governor** | **under throttle** | **7.2 vs 2.7 fixed** | **2.6x in interleaved A/B** |

The governor starts at `--concurrency` and adapts: +25% per 8 clean fetches,
halved whenever a fetch shows throttle signs (HTTP 429, -32005/-32029,
timeouts — detected inside the retry loop, not after it gives up). Under a
throttling endpoint this is what kills the "retries dominate" death spiral;
on a healthy or local endpoint it ramps to `--max-concurrency` on its own.

## 2. Decode throughput

`openchain decode` full pipeline: read logs from ClickHouse, ABI-decode with
alloy dyn-abi, serialize params to JSON, insert into `decoded_events`.
3 registered contracts (USDT, USDC impl, WETH), 188,204 logs in range.

| metric | value |
|---|---|
| logs decoded | 188,204 |
| wall time | 1.56 s |
| **throughput** | **~120,000 logs/s** |
| failures | 0 |

End-to-end wall time is dominated by the ClickHouse read + write round-trips,
not decode CPU. Decode itself was benchmarked in isolation (1M synthetic WETH9
Transfer logs, `cargo run --release -p openchain-evm --example decode_bench`,
same machine):

| path | throughput | notes |
|---|---|---|
| sequential | 350,507 logs/s | pre-OxAlpha behavior |
| **rayon parallel** | **904,922 logs/s** | **2.58x speedup**, 8-core M1 Pro |

Batches under 50k logs stay sequential: below that threshold the thread-pool
wake-up and fold/reduce overhead outweigh the CPU saved (measured as a ~15%
regression on a single 188k-log batch before the guard was added).

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
(`insert_version - block.timestamp`), 10 live blocks tailed at head via WS
`newHeads` (HTTP polling fallback if the endpoint has no WS), 3s safety-net
poll:

| min | avg | max |
|---|---|---|
| 0.6 s | 5.8 s | 24.2 s |

The 0.6s floor is what WS wake-up delivers when the endpoint serves the full
block + receipts immediately. The max is the first block after subscribing:
the head announcement races the RPC's read model, so `fetch_bundle` burns its
retry backoff (0.5+1+2+4s) before the data is servable. The remaining ~2-4s
steady-state is publicnode propagation + fetch time, not OpenChain. For
comparison, pure HTTP polling on the same endpoint measured min 2.1 / avg
5.4 / max 16.5 (2026-08-20): WS removes the polling delay from the floor but
cannot remove the endpoint's own latency. A local reth gets both to
sub-second.

Block timestamps mark slot start; propagation to a public RPC alone costs
~1-2s, so ~2s is near the floor for HTTP polling. Dune's freshness for
decoded tables is minutes. The reth ExEx path remains the endgame.

## 5. Storage

| table | rows | on disk | bytes/row |
|---|---|---|---|
| logs | 436,439 | 28.8 MiB | ~69 |
| transactions | 150,902 | 31.8 MiB | ~221 |
| decoded_events | 164,442 | 12.3 MiB | ~79 |
| traces | 21,349 | 1.33 MiB | ~65 |
| blocks | 551 | 68.5 KiB | ~127 |

Traces run ~1.4-2.4k per recent mainnet block at ~65 compressed bytes/row —
full mainnet history is multiple TB. Sync them for recent windows or against
a local node, not all of mainnet over a free RPC.

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

# 2b. isolated decode CPU benchmark (no ClickHouse I/O)
cargo run --release -p openchain-evm --example decode_bench

# 3. query latency (ClickHouse-side elapsed)
openchain sql "<query> FORMAT JSON"   # read statistics.elapsed

# 4. follow freshness
openchain sql "SELECT min(d), avg(d), max(d) FROM (SELECT insert_version/1000 - toUnixTimestamp(timestamp) AS d FROM blocks FINAL WHERE block_number >= <first followed block>)"
```

## Optimization backlog (ordered by expected impact)

1. **reth ExEx / local node source** — removes the RPC bottleneck entirely; sync
   throughput should jump from ~13 to hundreds of blocks/s, freshness to sub-second.
2. ~~**Parallel decode with rayon**~~ — DONE (OxAlpha): 2.58x on decode CPU
   (350k -> 905k logs/s isolated); batches <50k logs stay sequential.
3. ~~**WS `newHeads` subscription in follow**~~ — DONE (OxAlpha): freshness
   floor 2.1s -> 0.6s; polling remains as fallback and safety net.
4. ~~**Column codecs**~~ — MEASURED, NOT WORTH IT on `logs`: Delta+ZSTD on
   block_number/insert_version shrank the table by only ~0.6% (48.77 ->
   48.47 MiB over ~720k rows). The table is dominated by high-entropy
   tx_hash/data/topic columns that neither codec can compress; the 30-50%
   estimate was wrong. Codecs stay in the schema for fresh tables (free), but
   real storage wins would come from denormalizing block_hash out of logs or
   dictionary-encoding repeated hashes.
5. **Batched RowBinary inserts with larger buffers** — insert overhead is currently
   negligible at these volumes; revisit at >100k rows/s sustained.
6. ~~**Per-endpoint adaptive rate limiting**~~ — DONE (OxAlpha): AIMD governor
   in the sync path; 2.6x over fixed concurrency under endpoint throttling
   (see section 1).

## Feature parity roadmap (vs Dune, "initial" scope)

Shipped on OxAlpha:

- **traces dataset** — Parity-style internal transactions, creates,
  selfdestructs via `trace_block` (`--datasets traces`; needs a tracing
  endpoint — publicnode requires a paid token, drpc's free tier works).
  ~2.4k traces/block on mainnet; ORDER BY (chain_id, block_number, tx_index)
  keeps range scans and joins local.
- **`openchain status`** — per-dataset rows/ranges/watermarks + head lag.

Still missing for credible initial parity, in order:

1. **Decoded calls** — decode `traces.input` against registered ABIs (swap(),
   transfer()...) into `decoded_calls`. All the pieces exist: we already store
   traces and have an ABI registry; mirror `openchain decode` for calls.
2. **Multi-chain ergonomics** — config already supports N chains; add
   `--chain all` fan-out to sync/follow and per-chain status.
3. **reth ExEx / local node source** (backlog #1) — also unlocks sub-second
   freshness and unblocks trace-heavy ingestion without paid RPCs.
