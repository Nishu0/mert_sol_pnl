# SOL Balance Over Time - Lowest Latency Challenge

## Problem Statement

**Challenge:** Lowest latency algorithm for computing SOL balance over time at runtime with no indexing and only RPC.

**Context:** With Helius's new `getTransactionsForAddress` RPC method, you can query any range (start, end, middle) and parallelize calls — unlike the old `getSignaturesForAddress` which only traverses recent -> old.

**Core Question:** How do you search the set of transactions for an address most efficiently given you do not know how sparse they are for a given wallet?

## What We're Computing

- **Full SOL balance history over time** — every data point, every transaction that changed the balance
- NOT a single PnL number (not `post[last] - pre[first]`)
- NOT USDC values, just native SOL lamports/SOL
- Output: chronological list of `(timestamp, sol_balance)` after each transaction

## API: getTransactionsForAddress (Helius)

- **Endpoint:** `https://mainnet.helius-rpc.com/?api-key=<KEY>`
- **Key features:** bidirectional traversal, blockTime/slot range filters, full tx data mode
- **Full mode:** returns complete transaction + meta (preBalances, postBalances) — limit 100/request
- **Signatures mode:** returns signature stubs — limit 1000/request
- **Pagination:** via `paginationToken` (format: `"slot:position"`)
- **Filters:** `blockTime: {gte, lte}`, `slot: {gte, lte}`, `sortOrder: asc|desc`
- **Cost:** 50 credits/request

## Algorithm Strategy

### The Key Insight
Since we need **every** data point (full balance history), we must fetch ALL transactions. The optimization is about **parallelizing the fetch** — not skipping transactions.

### Approach: Adaptive Parallel Time-Range Chunking
1. **Probe phase:** Fire a small request (limit=1, asc + desc) to find first and last tx timestamps
2. **Split phase:** Divide the time range into N parallel chunks using blockTime filters
3. **Fetch phase:** Fire all chunks in parallel, each paginating independently
4. **Adaptive rebalance:** If some chunks return way more results than others, subdivide the dense chunks further
5. **Merge phase:** Merge all results chronologically, extract balance timeline

### Why This Beats Sequential
- Old method: N sequential pages, each waiting for the previous
- New method: K parallel streams, each covering a time slice — wall-clock time = max(stream_latencies) instead of sum

## Test Cases (20 addresses)

Mix of wallet types:
- **Busy wallets** — high-frequency traders, DEX bots (thousands of txs)
- **Sparse wallets** — occasional holders (few txs spread over months/years)
- **Periodic wallets** — DCA/staking wallets with regular intervals

## Implementation Plan

### Phase 1: Rust
- Async parallel RPC calls with tokio + reqwest
- Adaptive time-range splitting algorithm
- Benchmark harness: run each address, measure wall-clock latency, compute averages

### Phase 2: Zig (stretch)
- Port hot path to Zig for lower overhead
- Compare latency numbers against Rust

## Running

```bash
cd rust
cp .env.example .env  # Add your HELIUS_API_KEY
cargo run --release
```

## Metrics

For each test address, we measure:
- Wall-clock time from start to complete balance history
- Number of RPC calls made
- Total transactions fetched
- Average latency per RPC call
