# SOL Balance Over Time - Lowest Latency Challenge

## Problem

compute full SOL balance history (every data point, every balance change) for any wallet at runtime using only Helius `getTransactionsForAddress` RPC — no indexing, no pre-computation.

output: chronological `(timestamp, sol_balance)` after each balance-changing transaction.

## Benchmark Results

tested against wallet `8g6TWNoJK39JZ9tfPRy8kSkSnLtGEpSYX4nKR1yuNssr` (437 days of history, 1021 txs, 778 balance changes):

| version | latency | rpc calls | credits used | txs fetched |
|---|---|---|---|---|
| naive sequential | ~45s+ | 1000+ | 50,000+ | all |
| v1: parallel time chunks | 10.7s | 186 | 9,300 | 4,611 (no filter) |
| v2: two-phase sig+full | 5.7s | 231 | 11,550 | 1,021 |
| **v3: balanceChanged filter** | **6.9s** | **41** | **2,050** | **1,021** |

v3 uses **82% fewer RPC calls** than v2 (41 vs 231) and **96% fewer than naive**. latency is comparable because `balanceChanged` server-side filtering reduces the number of pages but each filtered call is slightly heavier.

### per-phase breakdown (v3, target wallet)

| phase | what happens | time | calls |
|---|---|---|---|
| boundary probe | 2 parallel calls (asc+desc limit=1) to find slot range | 646ms | 2 |
| density estimation | 7 parallel probes at log-spaced intervals with 1000-sig windows | 700ms | 7 |
| sig discovery | 5 parallel chunks across time range, signatures mode (1000/page) | 1131ms | ~10 |
| full tx fetch | 11 parallel batches by slot range, full mode (100/page) | 4460ms | ~22 |
| extract + sort | dedup by sig, sort by (slot, txIndex), extract pre/post balances | <3ms | 0 |
| **total** | | **6940ms** | **41** |

### results across wallet types

| wallet | time range | txs | bal changes | latency | calls |
|---|---|---|---|---|---|
| sparse (few txs/years) | 1884 days | 523 | 386 | 2.7s | 26 |
| periodic (DCA/staking) | 996 days | 248 | 193 | 2.1s | 19 |
| busy (active trader) | 3 days | 4,611 | 4,606 | 10.7s | 186 |
| target wallet | 437 days | 1,021 | 778 | 6.9s | 41 |

## Algorithm: Bidirectional Probe + 128-Segment Sig Discovery + Zero-Pagination Full Fetch

### why this design

the bottleneck is **network I/O** (~200-500ms per RPC round-trip). computation is negligible (<3ms). so the entire optimization problem reduces to: **eliminate all sequential call chains.**

two key insights:
1. `getTransactionsForAddress` supports `tokenAccounts: "balanceChanged"` — server-side filter that only returns transactions where any balance changed. a wallet might have 100,000 txs but only 10,000 that change its SOL balance.
2. signatures mode (1000/page) is 10x more efficient than full mode (100/page). use signatures to discover *what* to fetch, then full mode to fetch *only what you need* — all in parallel with zero sequential pagination.

### the 3 RTTs

```
                    ┌─────────────────────────────────────────────────┐
   RTT 1            │  bidirectional probe (2 parallel calls)         │
   2 calls          │  full mode, limit=100, asc + desc              │
                    │  → discovers boundaries, short-circuits ≤200 txs│
                    └──────────────────┬──────────────────────────────┘
                                       │
                    ┌──────────────────▼──────────────────────────────┐
   RTT 2            │  128-segment parallel sig discovery             │
   128+ calls       │  signatures mode, 1000/page, balanceChanged    │
                    │  → all tx signatures + slots discovered         │
                    └──────────────────┬──────────────────────────────┘
                                       │
                    ┌──────────────────▼──────────────────────────────┐
   RTT 3            │  zero-pagination parallel full fetch            │
   N calls          │  ~100-sig batches, non-overlapping slot ranges  │
                    │  → ALL batches fire simultaneously, no pagination│
                    └──────────────────┬──────────────────────────────┘
                                       │
                    ┌──────────────────▼──────────────────────────────┐
   Extract          │  dedup → sort → extract balance curve           │
   0 RTTs           │  → chronological (timestamp, sol_balance)       │
                    └─────────────────────────────────────────────────┘
```

### RTT 1: bidirectional probe (2 calls)

fire two full-mode calls in parallel:
- `sortOrder: "asc", limit: 100, balanceChanged` → oldest 100 balance-changing txs
- `sortOrder: "desc", limit: 100, balanceChanged` → newest 100 balance-changing txs

**short-circuit conditions** (all handled in RTT 1):
- either side returns < 100 txs → we've seen everything in that direction
- asc's last slot >= desc's last slot → the two ends overlap, nothing in between
- wallet has zero txs → done immediately

for any wallet with ≤ 200 balance-changing transactions, the entire job finishes in 1 RTT.

### RTT 2: 128-segment parallel sig discovery (128+ calls)

if there's a gap between the two probes, split it into 128 equal-width slot segments. fire all segments in parallel using signatures mode (1000/page) with `balanceChanged` filter.

signatures mode is 10x more efficient per page than full mode (1000 vs 100 per page). most segments complete in a single call. overflow segments paginate up to 5 pages each.

result: a deduplicated, sorted list of all transaction signatures and their slots. capped at 200k sigs for safety on token-heavy wallets.

### RTT 3: zero-pagination parallel full fetch (N calls)

this is the key optimization. instead of creating 128 segments that paginate sequentially (which creates O(pages_per_segment) sequential RTTs per segment), we use the sig discovery results to create **pre-sized batches that each fit in a single page**.

group the discovered sigs into batches of ~100. each batch uses the first/last sig's slot as its `slot: {gte, lte}` filter. fire ALL batches simultaneously — zero sequential pagination anywhere.

for a wallet with 128k balance-changing sigs, this fires ~1,290 parallel single-page fetches instead of 128 segments × 10 sequential pages each.

### extract: balance curve (0 RTTs, <3ms)

1. merge all txs from RTT 1 probe + RTT 3 full fetch
2. deduplicate by transaction signature (HashSet)
3. sort by `(slot, transactionIndex)` — deterministic ordering
4. for each tx, find wallet address in `accountKeys` + `loadedAddresses` (handles v0 ALT txs)
5. read `preBalances[i]` and `postBalances[i]`, emit point only where `pre != post`

### wire-level optimizations

- **gzip/brotli/deflate compression**: ~7x wire payload reduction per call
- **16 HTTP clients**: multiple TCP connections to avoid HTTP/2 multiplexing bottleneck
- **128 max concurrent requests**: saturates connection pool from the first wave
- **exponential backoff retries**: handles 429 rate limits and transient server errors

### RPC features exploited

| feature | how we use it | impact |
|---|---|---|
| `tokenAccounts: "balanceChanged"` | server-side filter on sig discovery + probe | only discover balance-changing txs |
| `slot: {gte, lte}` range filters | non-overlapping parallel batch ranges | zero duplicate fetches |
| `sortOrder: "asc"/"desc"` | bidirectional probe from both ends | 1 RTT boundary discovery + short-circuit |
| signatures mode (limit 1000) | RTT 2 sig discovery | 10x fewer pages than full mode |
| full mode (limit 100) | RTT 3 targeted fetch | only fetch txs we know exist |

### correctness

- **address lookup tables**: `preBalances`/`postBalances` are indexed by `[...accountKeys, ...loadedAddresses.writable, ...loadedAddresses.readonly]`. for v0 transactions using an ALT, the wallet may only appear in `loadedAddresses` — the extractor searches all three arrays.
- **dedup**: transactions deduplicated on their first signature using `HashSet<String>` across all phases.
- **sort key**: `(slot, transactionIndex)` for deterministic ordering when multiple txs land in the same slot.
- **failed transactions**: included via `balanceChanged` filter — failed txs still pay fees and their pre/post diff is the real fee delta.

## Architecture

```
rust/              rust backend (tokio + reqwest + axum)
  src/main.rs      algorithm + HTTP server with SSE streaming
  Cargo.toml       dependencies (tokio, reqwest, axum, serde, futures)

web/               next.js frontend (bun + tailwind)
  src/app/
    components/
      Dashboard.tsx    mission control HUD with live phase streaming
    api/balance/
      stream/route.ts  SSE proxy to rust backend
```

the rust backend runs the algorithm and exposes two endpoints:
- `GET /api/balance?address=<ADDR>` — JSON with full timeline + phase logs
- `GET /api/balance/stream?address=<ADDR>` — SSE stream, sends phase updates live then final data

the next.js frontend connects via SSE and renders:
- live phase-by-phase progress with timing
- transaction timeline heatmap (white dots = active days)
- SVG balance curve
- balance snapshot stats
- recent transaction trace

## Running

```bash
# 1. setup
cd rust
cp .env.example .env
# add HELIUS_API_KEY and optionally HELIUS_RPC_URL to .env

# 2. benchmark mode
cargo run --release

# 3. server mode
cargo run --release -- serve
# http://localhost:3001

# 4. frontend
cd ../web
bun install
bun run dev
# http://localhost:3000
```

## Environment Variables

| var | default | description |
|---|---|---|
| `HELIUS_API_KEY` | required | helius api key |
| `HELIUS_RPC_URL` | `https://mainnet.helius-rpc.com` | rpc endpoint (supports gatekeeper beta) |
| `MAX_CONCURRENT` | 64 | max parallel rpc connections |
| `MAX_SIGS` | 50000 | cap on sig discovery for mega-wallets |
| `PORT` | 3001 | server port (serve mode) |
