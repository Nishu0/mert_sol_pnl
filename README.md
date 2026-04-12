# SOL Balance Over Time - Lowest Latency Challenge

## Problem

compute full SOL balance history (every data point, every balance change) for any wallet at runtime using only Helius `getTransactionsForAddress` RPC — no indexing, no pre-computation.

output: chronological `(timestamp, sol_balance)` after each balance-changing transaction.

## Real Benchmarks

tested on Helius Developer plan (residential connection), `mainnet.helius-rpc.com` endpoint, 256 concurrent connections, 64 HTTP/1.1 clients with gzip compression.

### by wallet type

| wallet | txs | bal changes | time range | latency | rpc calls |
|---|---|---|---|---|---|
| Periodic `CKs1E69a...` | 255 | 193 | 996 days | **1.9s** | 280 |
| Sparse `HN7cABqL...` | 727 | 386 | 1890 days | **3.1s** | 363 |
| Busy `39cUkqsh...` | 8,262 | 8,257 | 3 days | **4.9s** | 514 |
| **120k `9ogeCJhw...`** | **124,954** | **97,584** | **335 days** | **13.6s** | **1,676** |

### call formula

```
calls = 2 (probe) + segments (sig discovery, 1 page each) + ceil(sigs / 100) (full fetch)
```

for a wallet with N balance-changing transactions and 256 segments:
- segments with data fire 1 sig call + ceil(sigs_in_segment / 100) full calls
- empty segments fire 1 sig call, 0 full calls
- all calls are pipelined: sig discovery and full fetch run concurrently within each segment

### projected latency by plan

the algorithm fires all calls in parallel, so higher RPS plans see near-linear speedup. projected from real benchmark data:

| wallet size | rpc calls | Developer (50 RPS) | Business (200 RPS) | Professional (500 RPS) | Enterprise (11K RPS) |
|---|---|---|---|---|---|
| 200 txs | 2 (probe short-circuit) | ~400ms | ~400ms | ~400ms | ~400ms |
| 1K txs | ~270 | 1.9s | 1.4s | 540ms | 25ms |
| 10K txs | ~510 | 4.9s | 2.6s | 1.0s | 46ms |
| 100K txs | ~1,600 | 13.6s | 8.0s | 3.2s | 145ms |
| 1M txs | ~10,500 | ~1.5 min | 52.5s | 21s | 955ms |

note: Developer plan numbers are real measured latencies from the benchmark above. Business/Professional/Enterprise are projected assuming linear scaling with RPS.

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
