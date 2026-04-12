use axum::{extract::Query, extract::State, response::sse::{Event, Sse}, routing::get, Router};
use chrono::DateTime;
use futures::future::join_all;
use futures::stream::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;
use tower_http::cors::CorsLayer;

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct BalancePoint {
    slot: u64,
    block_time: i64,
    pre_balance: u64,
    post_balance: u64,
    sol_balance: f64,
}

#[derive(Debug, Serialize, Clone)]
struct PhaseLog {
    phase: String,
    message: String,
    elapsed_ms: u128,
}

#[derive(Debug, Serialize)]
struct BalanceTimeline {
    address: String,
    points: Vec<BalancePoint>,
    phases: Vec<PhaseLog>,
    fetch_duration_ms: u128,
    rpc_calls: u32,
    total_txs_fetched: usize,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    result: Option<RpcResult>,
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct RpcResult {
    data: Vec<Value>,
    #[serde(rename = "paginationToken")]
    pagination_token: Option<String>,
}

// ── RPC Client ─────────────────────────────────────────────────────────────

struct HeliusClient {
    client: Client,
    endpoint: String,
    call_count: AtomicU32,
    semaphore: Semaphore,
}

impl HeliusClient {
    fn new(rpc_url: &str, api_key: &str, max_concurrent: usize) -> Self {
        let endpoint = if rpc_url.contains("api-key") {
            rpc_url.to_string()
        } else {
            format!("{}/?api-key={}", rpc_url.trim_end_matches('/'), api_key)
        };
        Self {
            client: Client::builder()
                .pool_max_idle_per_host(max_concurrent)
                .tcp_keepalive(std::time::Duration::from_secs(30))
                .build()
                .unwrap(),
            endpoint,
            call_count: AtomicU32::new(0),
            semaphore: Semaphore::new(max_concurrent),
        }
    }

    fn reset_calls(&self) { self.call_count.store(0, Ordering::Relaxed); }
    fn calls(&self) -> u32 { self.call_count.load(Ordering::Relaxed) }

    async fn rpc(
        &self,
        address: &str,
        tx_details: &str,
        sort_order: &str,
        limit: u32,
        pagination_token: Option<&str>,
        filters: Option<Value>,
        token_accounts: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
        let _permit = self.semaphore.acquire().await?;
        self.call_count.fetch_add(1, Ordering::Relaxed);

        let mut opts = json!({
            "transactionDetails": tx_details,
            "sortOrder": sort_order,
            "limit": limit,
            "maxSupportedTransactionVersion": 0,
        });
        if tx_details == "full" {
            opts["encoding"] = json!("jsonParsed");
        }
        if let Some(f) = filters {
            opts["filters"] = f;
        }
        if let Some(ta) = token_accounts {
            // merge into filters
            if opts["filters"].is_null() {
                opts["filters"] = json!({"tokenAccounts": ta});
            } else {
                opts["filters"]["tokenAccounts"] = json!(ta);
            }
        }
        if let Some(token) = pagination_token {
            opts["paginationToken"] = json!(token);
        }

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTransactionsForAddress",
            "params": [address, opts],
        });

        let resp = self.client.post(&self.endpoint).json(&body).send().await?
            .json::<RpcResponse>().await?;

        if let Some(err) = resp.error {
            return Err(format!("RPC error: {}", err).into());
        }
        match resp.result {
            Some(r) => Ok((r.data, r.pagination_token)),
            None => Ok((vec![], None)),
        }
    }

    /// Fetch all full txs in a slot range, paginating until done.
    /// Uses balanceChanged filter to only get SOL-relevant txs.
    async fn fetch_full_range(
        &self,
        address: &str,
        slot_gte: u64,
        slot_lte: u64,
    ) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        let filters = Some(json!({"slot": {"gte": slot_gte, "lte": slot_lte}}));
        loop {
            let (data, next) = self.rpc(
                address, "full", "asc", 100, token.as_deref(),
                filters.clone(), Some("balanceChanged"),
            ).await?;
            let done = data.is_empty() || next.is_none();
            all.extend(data);
            if done { break; }
            token = next;
        }
        Ok(all)
    }

    /// Fetch full txs starting from a synthetic pagination token.
    async fn fetch_full_from_token(
        &self,
        address: &str,
        synthetic_token: &str,
        slot_lte: Option<u64>,
    ) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let mut all = Vec::new();
        let mut token: Option<String> = Some(synthetic_token.to_string());
        let filters = slot_lte.map(|s| json!({"slot": {"lte": s}}));
        loop {
            let (data, next) = self.rpc(
                address, "full", "asc", 100, token.as_deref(),
                filters.clone(), Some("balanceChanged"),
            ).await?;
            let done = data.is_empty() || next.is_none();
            all.extend(data);
            if done { break; }
            token = next;
        }
        Ok(all)
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Algorithm: Adaptive Parallel + Continuity Pruning
//
// 1-RTT: Boundary probe (oldest + newest tx, parallel)
// 1-RTT: Golomb-spaced probes with full data + balanceChanged
//        → extract pre/post balance at each probe boundary
//        → continuity pruning: skip gaps where balance unchanged
// 1-RTT: Parallel fetch of confirmed-changed ranges only
//        → synthetic pagination tokens for intra-range parallelism
// ════════════════════════════════════════════════════════════════════════════

/// Probe result: a window of data with boundary balances.
struct ProbeResult {
    slot_start: u64,
    slot_end: u64,
    txs: Vec<Value>,
    // balance of target address at first and last tx in this probe
    first_pre_balance: Option<u64>,
    last_post_balance: Option<u64>,
    is_full: bool, // hit the limit → there may be more txs
}

fn extract_addr_balance(tx: &Value, address: &str) -> Option<(u64, u64)> {
    let keys = tx["transaction"]["message"]["accountKeys"].as_array()?;
    let idx = keys.iter().position(|k| {
        k.as_str() == Some(address) || k["pubkey"].as_str() == Some(address)
    })?;
    let pre = tx["meta"]["preBalances"][idx].as_u64()?;
    let post = tx["meta"]["postBalances"][idx].as_u64()?;
    Some((pre, post))
}

async fn compute_balance_timeline(
    client: &HeliusClient,
    address: &str,
) -> Result<BalanceTimeline, Box<dyn std::error::Error + Send + Sync>> {
    let start = Instant::now();
    client.reset_calls();
    let mut phases: Vec<PhaseLog> = Vec::new();

    let log = |phases: &mut Vec<PhaseLog>, phase: &str, msg: &str, start: &Instant| {
        let entry = PhaseLog {
            phase: phase.to_string(),
            message: msg.to_string(),
            elapsed_ms: start.elapsed().as_millis(),
        };
        eprintln!("    [{}] {}: {}", &address[..8.min(address.len())], phase, msg);
        phases.push(entry);
    };

    // ── RTT 1: Boundary probe (parallel) ──
    let (oldest, newest) = tokio::join!(
        client.rpc(address, "full", "asc", 1, None, None, Some("balanceChanged")),
        client.rpc(address, "full", "desc", 1, None, None, Some("balanceChanged")),
    );
    let (oldest_data, _) = oldest?;
    let (newest_data, _) = newest?;

    if oldest_data.is_empty() || newest_data.is_empty() {
        log(&mut phases, "Bounds", "no transactions found", &start);
        return Ok(BalanceTimeline {
            address: address.to_string(), points: vec![], phases,
            fetch_duration_ms: start.elapsed().as_millis(),
            rpc_calls: client.calls(), total_txs_fetched: 0,
        });
    }

    let min_slot = oldest_data[0]["slot"].as_u64().unwrap_or(0);
    let max_slot = newest_data[0]["slot"].as_u64().unwrap_or(0);
    let first_bt = oldest_data[0]["blockTime"].as_i64().unwrap_or(0);
    let last_bt = newest_data[0]["blockTime"].as_i64().unwrap_or(0);

    // extract boundary balances
    let oldest_bal = extract_addr_balance(&oldest_data[0], address);
    let newest_bal = extract_addr_balance(&newest_data[0], address);

    log(&mut phases, "Bounds", &format!(
        "slot {}..{} ({} -> {}), {} calls",
        min_slot, max_slot,
        DateTime::from_timestamp(first_bt, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
        DateTime::from_timestamp(last_bt, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
        client.calls(),
    ), &start);

    let slot_range = max_slot - min_slot;

    // ── RTT 2: Golomb-spaced probes with continuity oracle ──
    // Use 12 probes (Golomb-ruler-ish spacing for maximum coverage)
    let num_probes = if slot_range < 10000 { 4 } else { 12.min(((slot_range as f64).log2() as usize).max(4)) };

    // Golomb-ruler-inspired offsets (normalized 0..1)
    let golomb_offsets: Vec<f64> = if num_probes <= 4 {
        vec![0.0, 0.33, 0.66, 1.0]
    } else {
        // Modified Golomb: 0, 1, 3, 7, 12, 20, 29, 38, 48, 59, 70, 82 (scaled to 0..1)
        let raw = vec![0.0, 0.012, 0.037, 0.085, 0.146, 0.244, 0.354, 0.463, 0.585, 0.72, 0.854, 1.0];
        raw[..num_probes].to_vec()
    };

    let mut probe_futures = Vec::with_capacity(num_probes);
    for &frac in &golomb_offsets {
        let probe_slot = min_slot + (frac * slot_range as f64) as u64;
        // each probe fetches a small window of 100 txs from that slot
        probe_futures.push(async move {
            let r = client.rpc(
                address, "full", "asc", 100, None,
                Some(json!({"slot": {"gte": probe_slot}})),
                Some("balanceChanged"),
            ).await;
            (probe_slot, r)
        });
    }

    let probe_results_raw = join_all(probe_futures).await;

    // Parse probe results and extract boundary balances
    let mut probes: Vec<ProbeResult> = Vec::new();
    for (probe_slot, result) in probe_results_raw {
        if let Ok((data, next_token)) = result {
            let first_bal = data.first().and_then(|tx| extract_addr_balance(tx, address));
            let last_bal = data.last().and_then(|tx| extract_addr_balance(tx, address));
            let last_slot = data.last().map(|tx| tx["slot"].as_u64().unwrap_or(probe_slot)).unwrap_or(probe_slot);

            probes.push(ProbeResult {
                slot_start: probe_slot,
                slot_end: last_slot,
                first_pre_balance: first_bal.map(|(pre, _)| pre),
                last_post_balance: last_bal.map(|(_, post)| post),
                is_full: data.len() >= 100 && next_token.is_some(),
                txs: data,
            });
        }
    }

    probes.sort_by_key(|p| p.slot_start);

    // Continuity pruning: if probe[i].last_post == probe[i+1].first_pre → gap is empty
    let mut ranges_to_fetch: Vec<(u64, u64)> = Vec::new();
    let mut pruned_gaps = 0usize;
    let mut total_probe_txs = 0usize;

    for p in &probes {
        total_probe_txs += p.txs.len();
    }

    // Check gaps between consecutive probes
    for i in 0..probes.len() {
        // If probe was full (truncated), we need to fetch the continuation
        // from where this probe left off to where the next probe starts
        if probes[i].is_full {
            let cont_start = probes[i].slot_end + 1;
            let cont_end = if i + 1 < probes.len() {
                probes[i + 1].slot_start.saturating_sub(1)
            } else {
                max_slot
            };
            if cont_start <= cont_end {
                ranges_to_fetch.push((cont_start, cont_end));
            }
        }

        // Check gap between this probe's coverage and the next probe
        if i + 1 < probes.len() {
            let gap_start = probes[i].slot_end + 1;
            let gap_end = probes[i + 1].slot_start.saturating_sub(1);

            if gap_start <= gap_end {
                let post_bal = probes[i].last_post_balance;
                let pre_bal = probes[i + 1].first_pre_balance;

                if post_bal.is_some() && pre_bal.is_some() && post_bal == pre_bal {
                    // Balance unchanged across this gap → skip it
                    pruned_gaps += 1;
                } else {
                    // Balance changed or unknown → must fetch this gap
                    ranges_to_fetch.push((gap_start, gap_end));
                }
            }
        }
    }

    // Also check the range before the first probe and after the last probe
    if !probes.is_empty() {
        if probes[0].slot_start > min_slot {
            // check if oldest boundary balance matches first probe
            let oldest_post = oldest_bal.map(|(_, post)| post);
            let first_pre = probes[0].first_pre_balance;
            if oldest_post.is_some() && first_pre.is_some() && oldest_post == first_pre {
                pruned_gaps += 1;
            } else {
                ranges_to_fetch.push((min_slot, probes[0].slot_start.saturating_sub(1)));
            }
        }
        let last_probe = probes.last().unwrap();
        if last_probe.slot_end < max_slot {
            let last_post = last_probe.last_post_balance;
            let newest_pre = newest_bal.map(|(pre, _)| pre);
            if last_post.is_some() && newest_pre.is_some() && last_post == newest_pre {
                pruned_gaps += 1;
            } else {
                ranges_to_fetch.push((last_probe.slot_end + 1, max_slot));
            }
        }
    }

    // Dedup and merge overlapping ranges
    ranges_to_fetch.sort_by_key(|r| r.0);
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for range in &ranges_to_fetch {
        if let Some(last) = merged.last_mut() {
            if range.0 <= last.1 + 1 {
                last.1 = last.1.max(range.1);
                continue;
            }
        }
        merged.push(*range);
    }

    log(&mut phases, "Probes", &format!(
        "{} probes, {} txs from probes, {} gaps pruned, {} ranges to fetch",
        probes.len(), total_probe_txs, pruned_gaps, merged.len()
    ), &start);

    // ── RTT 3: Parallel fetch of confirmed-changed ranges ──
    // Aggressively split large ranges into many parallel sub-ranges.
    // Target: each sub-range small enough for ~1-2 pages (100-200 txs).
    let mut fetch_futures: Vec<_> = Vec::new();

    for (range_start, range_end) in &merged {
        let range_slots = range_end - range_start;

        // Split into sub-ranges. More aggressive splitting = more parallelism.
        // Each sub-range targets ~100k slots (roughly a few hundred txs for active wallets).
        let chunk_size = 100_000u64;
        let num_splits = ((range_slots / chunk_size) + 1).max(1).min(64);
        let split_size = range_slots / num_splits;

        for j in 0..num_splits {
            let sub_start = range_start + j * split_size;
            let sub_end = if j == num_splits - 1 { *range_end } else { range_start + (j + 1) * split_size - 1 };
            fetch_futures.push(client.fetch_full_range(address, sub_start, sub_end));
        }
    }

    let fetch_results = join_all(fetch_futures).await;

    // Collect all txs: from probes + from fetched ranges
    let mut all_txs: Vec<Value> = Vec::new();

    // Add probe txs
    for p in probes {
        all_txs.extend(p.txs);
    }

    // Add boundary txs
    all_txs.extend(oldest_data);
    all_txs.extend(newest_data);

    // Add fetched range txs
    for result in fetch_results {
        all_txs.extend(result?);
    }

    // Dedup by signature
    let mut seen = HashSet::new();
    all_txs.retain(|tx| {
        let sig = tx["transaction"]["signatures"][0].as_str().unwrap_or("").to_string();
        !sig.is_empty() && seen.insert(sig)
    });

    // Sort by (slot, transactionIndex)
    all_txs.sort_by(|a, b| {
        let sa = a["slot"].as_u64().unwrap_or(0);
        let sb = b["slot"].as_u64().unwrap_or(0);
        let ia = a["transactionIndex"].as_u64().unwrap_or(0);
        let ib = b["transactionIndex"].as_u64().unwrap_or(0);
        (sa, ia).cmp(&(sb, ib))
    });

    let total_txs = all_txs.len();

    log(&mut phases, "Fetch", &format!(
        "{} total txs after dedup, {} rpc calls",
        total_txs, client.calls()
    ), &start);

    // ── Extract balance curve ──
    let points = extract_balance_points(address, &all_txs);

    log(&mut phases, "Done", &format!(
        "{} balance points from {} txs in {}ms",
        points.len(), total_txs, start.elapsed().as_millis()
    ), &start);

    Ok(BalanceTimeline {
        address: address.to_string(),
        points,
        phases,
        fetch_duration_ms: start.elapsed().as_millis(),
        rpc_calls: client.calls(),
        total_txs_fetched: total_txs,
    })
}

fn extract_balance_points(address: &str, txs: &[Value]) -> Vec<BalancePoint> {
    let mut points = Vec::with_capacity(txs.len());
    for tx in txs {
        let slot = tx["slot"].as_u64().unwrap_or(0);
        let block_time = tx["blockTime"].as_i64().unwrap_or(0);
        if let Some((pre, post)) = extract_addr_balance(tx, address) {
            if pre != post {
                points.push(BalancePoint {
                    slot, block_time,
                    pre_balance: pre,
                    post_balance: post,
                    sol_balance: post as f64 / 1e9,
                });
            }
        }
    }
    points
}

// ── HTTP Server ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BalanceQuery { address: String }

async fn handle_balance(
    State(client): State<Arc<HeliusClient>>,
    Query(q): Query<BalanceQuery>,
) -> axum::Json<Value> {
    match compute_balance_timeline(&client, &q.address).await {
        Ok(tl) => axum::Json(json!(tl)),
        Err(e) => axum::Json(json!({"error": e.to_string()})),
    }
}

async fn handle_balance_stream(
    State(client): State<Arc<HeliusClient>>,
    Query(q): Query<BalanceQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let address = q.address.clone();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("phase").data(
            json!({"phase": "start", "message": format!("analyzing {}", address)}).to_string()
        ));
        match compute_balance_timeline(&client, &address).await {
            Ok(tl) => {
                for phase in &tl.phases {
                    yield Ok(Event::default().event("phase").data(
                        serde_json::to_string(phase).unwrap_or_default()
                    ));
                }
                yield Ok(Event::default().event("summary").data(json!({
                    "fetch_duration_ms": tl.fetch_duration_ms,
                    "rpc_calls": tl.rpc_calls,
                    "total_txs_fetched": tl.total_txs_fetched,
                    "balance_points": tl.points.len(),
                }).to_string()));
                yield Ok(Event::default().event("data").data(
                    serde_json::to_string(&tl.points).unwrap_or_default()
                ));
                yield Ok(Event::default().event("done").data("complete"));
            }
            Err(e) => {
                yield Ok(Event::default().event("server_error").data(
                    json!({"error": e.to_string()}).to_string()
                ));
            }
        }
    };
    Sse::new(stream)
}

async fn handle_health() -> &'static str { "ok" }

// ── Main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();

    let api_key = std::env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY must be set");
    let rpc_url = std::env::var("HELIUS_RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.helius-rpc.com".to_string());

    if api_key.is_empty() {
        eprintln!("HELIUS_API_KEY is empty");
        std::process::exit(1);
    }

    let max_concurrent = std::env::var("MAX_CONCURRENT")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(64usize);

    let client = Arc::new(HeliusClient::new(&rpc_url, &api_key, max_concurrent));
    let mode = std::env::args().nth(1).unwrap_or_default();

    if mode == "serve" {
        let port: u16 = std::env::var("PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(3001);
        let app = Router::new()
            .route("/api/balance", get(handle_balance))
            .route("/api/balance/stream", get(handle_balance_stream))
            .route("/health", get(handle_health))
            .layer(CorsLayer::permissive())
            .with_state(client);
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
        eprintln!("  server on http://localhost:{}", port);
        axum::serve(listener, app).await?;
    } else {
        let rpc_display = rpc_url.split('?').next().unwrap_or(&rpc_url);
        eprintln!("  rpc: {} (max {} concurrent)", rpc_display, max_concurrent);
        eprintln!();

        let test_addresses = vec![
            ("Busy", "39cUkqsh91Vx5EtxuRENWCMBXeiwL7GDaNarTYSns75V"),
            ("Medium", "FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH"),
            ("Sparse", "HN7cABqLq46Es1jh92dQQisAq662SmxELLLsHHe4YWrH"),
            ("Periodic", "CKs1E69a2e9TmH4mKKLrXFF8kD3ZnwKjoEuXa6sz9WqX"),
            ("Active", "H8sMJSCQxfKiFTCfDR3DUMLPwcRbM61LGFJ8N4dK3WjS"),
        ];

        let mut results: Vec<(String, BalanceTimeline)> = Vec::new();

        for (name, addr) in &test_addresses {
            eprintln!("─── {} ({}) ───", name, &addr[..12]);
            match compute_balance_timeline(&client, addr).await {
                Ok(tl) => {
                    if let (Some(f), Some(l)) = (tl.points.first(), tl.points.last()) {
                        let fd = DateTime::from_timestamp(f.block_time, 0)
                            .map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
                        let ld = DateTime::from_timestamp(l.block_time, 0)
                            .map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
                        eprintln!("    {:.4} SOL @ {} -> {:.4} SOL @ {}", f.sol_balance, fd, l.sol_balance, ld);
                    }
                    eprintln!();
                    results.push((name.to_string(), tl));
                }
                Err(e) => eprintln!("    ERROR: {}\n", e),
            }
        }

        println!("\n┌────────────────┬──────────┬───────┬────────┬─────────┐");
        println!("│ address        │ latency  │ calls │ txs    │ points  │");
        println!("├────────────────┼──────────┼───────┼────────┼─────────┤");
        for (name, tl) in &results {
            println!("│ {:<14} │ {:>5} ms │ {:>5} │ {:>6} │ {:>7} │",
                name, tl.fetch_duration_ms, tl.rpc_calls, tl.total_txs_fetched, tl.points.len());
        }
        if !results.is_empty() {
            println!("├────────────────┼──────────┼───────┼────────┼─────────┤");
            let lats: Vec<u128> = results.iter().map(|r| r.1.fetch_duration_ms).collect();
            let avg = lats.iter().sum::<u128>() / lats.len() as u128;
            println!("│ avg            │ {:>5} ms │       │        │         │", avg);
            println!("│ min            │ {:>5} ms │       │        │         │", lats.iter().min().unwrap());
            println!("│ max            │ {:>5} ms │       │        │         │", lats.iter().max().unwrap());
        }
        println!("└────────────────┴──────────┴───────┴────────┴─────────┘");
    }

    Ok(())
}
