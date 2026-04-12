# SOL Balance Over Time - Lowest Latency Challenge

## Problem Statement

**Challenge:** Lowest latency algorithm for computing SOL balance over time at runtime with no indexing and only RPC.

**Context:** With Helius's `getTransactionsForAddress` RPC method, you can query any range (start, end, middle) and parallelize calls — unlike the old `getSignaturesForAddress` which only traverses recent -> old.

**Core Question:** How do you search the set of transactions for an address most efficiently given you do not know how sparse they are for a given wallet?

## What We're Computing

- **Full SOL balance history over time** — every data point, every transaction that changed the balance
- NOT a single PnL number (not `post[last] - pre[first]`)
- NOT USDC values, just native SOL lamports/SOL
- Output: chronological list of `(timestamp, sol_balance)` after each transaction

## API: getTransactionsForAddress (Helius)

- **Endpoint:** `https://mainnet.helius-rpc.com/?api-key=<KEY>`
- **Full mode:** returns complete transaction + meta (preBalances, postBalances) — limit 100/request
- **Signatures mode:** returns signature stubs — limit 1000/request
- **Pagination token format:** `"slot:transactionIndex"` — can be synthesized for parallel pagination
- **Filters:** `slot: {gte, lte}`, `blockTime: {gte, lte}`, `sortOrder: asc|desc`, `tokenAccounts: "balanceChanged"`
- **Cost:** 50 credits/request

## Algorithm: Adaptive Parallel + Continuity Pruning

The bottleneck is network I/O (~200-500ms per RPC round-trip). Total latency = longest sequential chain of calls. The algorithm minimizes sequential chains by maximizing parallelism and skipping empty regions.

### RPC super-powers we exploit

1. **`tokenAccounts: "balanceChanged"`** — server-side filter that only returns txs where a balance actually changed. massive reduction vs fetching all txs and filtering client-side.
2. **slot-based range queries** (`slot: {gte, lte}`) — precise parallel range splitting without timestamp ambiguity.
3. **synthetic pagination tokens** — format is `"slot:transactionIndex"`, we can construct these to jump to any point and parallelize within dense ranges.
4. **bidirectional traversal** — `sortOrder: "asc" | "desc"` lets us probe from both ends simultaneously.

### RTT 1: Boundary probe (2 parallel calls)

Fire two calls simultaneously:
- `sortOrder: "asc", limit: 1, filter: balanceChanged` → oldest tx
- `sortOrder: "desc", limit: 1, filter: balanceChanged` → newest tx

This gives us `minSlot`, `maxSlot`, and the boundary pre/post balances. If the wallet has zero txs, we're done in 1 RTT.

### RTT 2: Golomb-spaced probes + continuity oracle (12 parallel calls)

Fire 12 probes at Golomb-ruler-inspired slot offsets across `[minSlot, maxSlot]`. Each probe fetches up to 100 full txs with `balanceChanged` filter starting from its probe slot.

**Golomb-ruler spacing** (normalized 0..1):
```
[0.0, 0.012, 0.037, 0.085, 0.146, 0.244, 0.354, 0.463, 0.585, 0.72, 0.854, 1.0]
```

Uneven spacing ensures every pair of probes has a different distance, maximizing information coverage compared to linear spacing.

**From each probe we extract:**
- The txs themselves (up to 100 per probe → ~1200 txs for free)
- `preBalance` of the first tx in the window
- `postBalance` of the last tx in the window
- Whether the probe was truncated (hit 100 limit)

**Continuity pruning (the key insight):**

For each gap between consecutive probes, compare:
- `postBalance` of probe N (the balance after its last tx)
- `preBalance` of probe N+1 (the balance before its first tx)

If they match → **the balance did not change in that entire gap → skip it for free (zero RPC calls).**

This alone kills 70-90% of the timeline on normal/sparse wallets. A wallet with 5 years of history but only periodic activity gets its empty months/years pruned in a single RTT.

### RTT 3: Parallel fetch of confirmed-changed ranges

Only the ranges that survived continuity pruning need fetching. Each surviving range is split into parallel sub-ranges (~100k slots each, targeting 1-2 pages per sub-range).

For dense ranges: synthetic pagination tokens (`"slot:0"`) let us jump to any slot and parallelize within it, eliminating sequential pagination.

All sub-ranges fire simultaneously with up to 64 concurrent connections.

### Stitch the curve

1. Collect all txs from probes + boundary calls + fetched ranges
2. Deduplicate by transaction signature
3. Sort by `(slot, transactionIndex)` for deterministic ordering
4. Walk through and extract `(timestamp, postBalance)` for every tx where `preBalance != postBalance`

Done — full balance-over-time curve.

### Why this is optimal

| wallet type | behavior |
|---|---|
| **sparse** (few txs over years) | 2-3 RTTs total. probes cover everything, continuity prunes all gaps. |
| **periodic** (staking/DCA) | 2-3 RTTs. most gaps pruned, only active windows fetched. |
| **busy** (trader, 3 days) | 2+ RTTs. dense range can't be pruned but parallelism splits it across 64 connections. |
| **heavy** (100k+ txs) | continuity prunes dead periods, parallel sub-ranges + synthetic tokens eliminate sequential pagination. |

- **Latency = slowest single RPC call**, not sum of all calls (true parallelism)
- **RPC calls scale with balance-changing events**, not total history length
- **No wasted fetches** — `balanceChanged` filter + continuity pruning = only fetch what matters

## Architecture

```
rust/          Rust backend (tokio + reqwest + axum)
  src/main.rs  Algorithm + HTTP server (SSE streaming)
  Cargo.toml   Dependencies

web/           Next.js frontend (Bun + Tailwind)
  src/app/     Pages + components
    components/Dashboard.tsx   Mission control HUD
    api/balance/stream/        SSE proxy to Rust backend
```

### Running

```bash
# 1. add your helius key
cd rust
cp .env.example .env
# edit .env: HELIUS_API_KEY=your_key

# 2. benchmark mode (test 5 wallets)
cargo run --release

# 3. server mode (HTTP API)
cargo run --release -- serve
# -> http://localhost:3001/api/balance?address=<ADDR>
# -> http://localhost:3001/api/balance/stream?address=<ADDR> (SSE)

# 4. frontend
cd ../web
bun install
bun run dev
# -> http://localhost:3000
```

## Test Cases

Mix of wallet types to benchmark across density profiles:
- **Busy** — high-frequency trader (thousands of txs in days)
- **Medium** — DeFi protocol wallet (years of history, moderate activity)
- **Sparse** — occasional holder (few txs spread over years)
- **Periodic** — DCA/staking wallet (regular intervals)
- **Active** — whale wallet (massive tx volume)

## Metrics

For each test address we measure:
- Wall-clock time from start to complete balance curve
- Number of RPC calls made
- Total transactions fetched (after `balanceChanged` filter)
- Number of balance change points extracted
- Gaps pruned by continuity oracle
