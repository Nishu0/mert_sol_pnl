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

// ── Constants ─────────────────────────────────────────────────────────────

const FULL_PAGE_LIMIT: u32 = 100;
const SIG_PAGE_LIMIT: u32 = 1000;
const INITIAL_SEGMENTS: usize = 128;
const MAX_SIGS: usize = 200_000;        // Cap sig discovery to avoid token-heavy wallets
const MAX_FULL_BATCHES: usize = 2048;   // Cap parallel full fetches to avoid rate limits

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct BalancePoint {
    slot: u64,
    block_time: i64,
    pre_balance: u64,
    post_balance: u64,
    sol_balance: f64,
}

#[derive(Debug, Clone, Serialize)]
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

const NUM_CLIENTS: usize = 16; // Multiple HTTP clients = multiple TCP connections

struct HeliusClient {
    clients: Vec<Client>,
    endpoint: String,
    call_count: AtomicU32,
    next_client: AtomicU32,
    semaphore: Semaphore,
    retry_count: AtomicU32,
    slow_count: AtomicU32,
}

impl HeliusClient {
    fn new(rpc_url: &str, api_key: &str, max_concurrent: usize) -> Self {
        let endpoint = if rpc_url.contains("api-key") {
            rpc_url.to_string()
        } else {
            format!("{}/?api-key={}", rpc_url.trim_end_matches('/'), api_key)
        };
        // Create multiple HTTP clients — each gets its own TCP connection pool.
        // HTTP/2 multiplexes all requests over 1 connection per client.
        // N clients = N connections = N parallel pipes, mimicking HTTP/1.1 with N sockets.
        let clients: Vec<Client> = (0..NUM_CLIENTS)
            .map(|_| {
                Client::builder()
                    .pool_max_idle_per_host(max_concurrent / NUM_CLIENTS + 1)
                    .tcp_keepalive(std::time::Duration::from_secs(30))
                    .gzip(true)
                    .brotli(true)
                    .deflate(true)
                    .http2_initial_stream_window_size(2 * 1024 * 1024)
                    .http2_initial_connection_window_size(16 * 1024 * 1024)
                    .http2_adaptive_window(true)
                    .build()
                    .unwrap()
            })
            .collect();
        Self {
            clients,
            endpoint,
            call_count: AtomicU32::new(0),
            next_client: AtomicU32::new(0),
            semaphore: Semaphore::new(max_concurrent),
            retry_count: AtomicU32::new(0),
            slow_count: AtomicU32::new(0),
        }
    }

    fn reset_calls(&self) {
        self.call_count.store(0, Ordering::Relaxed);
        self.retry_count.store(0, Ordering::Relaxed);
        self.slow_count.store(0, Ordering::Relaxed);
    }

    fn calls(&self) -> u32 {
        self.call_count.load(Ordering::Relaxed)
    }

    async fn rpc_call(
        &self,
        address: &str,
        tx_details: &str,
        sort_order: &str,
        limit: u32,
        pagination_token: Option<&str>,
        filters: Option<Value>,
        use_balance_filter: bool,
    ) -> Result<(Vec<Value>, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
        let _permit = self.semaphore.acquire().await?;
        self.call_count.fetch_add(1, Ordering::Relaxed);
        let client_idx = (self.next_client.fetch_add(1, Ordering::Relaxed) as usize) % self.clients.len();
        let client = &self.clients[client_idx];

        let mut opts = json!({
            "transactionDetails": tx_details,
            "sortOrder": sort_order,
            "limit": limit,
            "maxSupportedTransactionVersion": 0,
        });
        let mut filter_obj = filters.unwrap_or(json!({}));
        if use_balance_filter {
            filter_obj["tokenAccounts"] = json!("balanceChanged");
        }
        opts["filters"] = filter_obj;
        if let Some(token) = pagination_token {
            opts["paginationToken"] = json!(token);
        }

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTransactionsForAddress",
            "params": [address, opts],
        });

        // Retry up to 5 times with exponential backoff
        let call_start = Instant::now();
        let mut last_err = String::new();
        for attempt in 0..5 {
            if attempt > 0 {
                self.retry_count.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_millis(200 * (1 << attempt))).await;
            }
            let result = client
                .post(&self.endpoint)
                .json(&body)
                .send()
                .await;

            let resp = match result {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };

            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                last_err = "429 rate limited".to_string();
                tokio::time::sleep(std::time::Duration::from_millis(1000 * (1 << attempt))).await;
                continue;
            }
            if status.is_server_error() {
                last_err = format!("server error {}", status);
                continue;
            }

            match resp.json::<RpcResponse>().await {
                Ok(parsed) => {
                    let elapsed = call_start.elapsed().as_millis();
                    if elapsed > 2000 {
                        self.slow_count.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(err) = parsed.error {
                        return Err(format!("RPC error: {}", err).into());
                    }
                    return match parsed.result {
                        Some(r) => Ok((r.data, r.pagination_token)),
                        None => Ok((vec![], None)),
                    };
                }
                Err(e) => {
                    last_err = format!("decode error (status {}): {}", status, e);
                    continue;
                }
            }
        }

        Err(format!("RPC call failed after 5 attempts: {}", last_err).into())
    }

    /// Fetch all signatures in a slot range, paginating up to max_pages.
    async fn fetch_sigs_in_slot_range(
        &self,
        address: &str,
        slot_gte: u64,
        slot_lte: u64,
        max_pages: usize,
    ) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        let filters = Some(json!({"slot": {"gte": slot_gte, "lte": slot_lte}}));
        let mut pages = 0usize;
        loop {
            let (data, next) = self
                .rpc_call(address, "signatures", "asc", SIG_PAGE_LIMIT, token.as_deref(), filters.clone(), true)
                .await?;
            let done = data.is_empty() || next.is_none();
            all.extend(data);
            pages += 1;
            if done || pages >= max_pages { break; }
            token = next;
        }
        Ok(all)
    }

}

// ── Algorithm ─────────────────────────────────────────────────────────────
//
// RTT 1 — Bidirectional probe (2 parallel calls, full mode, limit=100)
//         Discovers boundaries. Short-circuits if wallet has ≤200 balance-changing txs.
//
// RTT 2 — 128-segment parallel sig discovery (signatures mode, limit=1000)
//         10x more efficient per page than full mode.
//         Most segments complete in 1 call. Overflow paginates linearly (up to 5 pages).
//         Result: all tx signatures + slots discovered.
//
// RTT 3 — Parallel full tx fetch (N batches, full mode, limit=100)
//         Group discovered sigs into batches of ~100 with non-overlapping slot ranges.
//         Fire all batches in parallel.
//
// Extract — Dedup, sort by (slot, txIndex), extract pre/post balances.

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

    // ── RTT 1: Bidirectional probe (full mode, gets actual tx data) ──
    let (asc_result, desc_result) = tokio::join!(
        client.rpc_call(address, "full", "asc", FULL_PAGE_LIMIT, None, None, true),
        client.rpc_call(address, "full", "desc", FULL_PAGE_LIMIT, None, None, true),
    );
    let (asc_data, asc_next) = asc_result?;
    let (desc_data, desc_next) = desc_result?;

    log(&mut phases, "RTT 1", &format!(
        "Bidirectional probe: asc={} desc={}", asc_data.len(), desc_data.len()
    ), &start);

    // Empty wallet
    if asc_data.is_empty() && desc_data.is_empty() {
        return Ok(BalanceTimeline {
            address: address.to_string(),
            points: vec![],
            phases,
            fetch_duration_ms: start.elapsed().as_millis(),
            rpc_calls: client.calls(),
            total_txs_fetched: 0,
        });
    }

    // Short-circuit: check if both probes cover everything
    let asc_complete = asc_data.len() < FULL_PAGE_LIMIT as usize || asc_next.is_none();
    let desc_complete = desc_data.len() < FULL_PAGE_LIMIT as usize || desc_next.is_none();
    let asc_last_slot = asc_data.last().and_then(|d| d["slot"].as_u64()).unwrap_or(0);
    let desc_last_slot = desc_data.last().and_then(|d| d["slot"].as_u64()).unwrap_or(u64::MAX);
    let overlaps = asc_last_slot >= desc_last_slot;

    if asc_complete || desc_complete || overlaps {
        let mut all_txs = asc_data;
        all_txs.extend(desc_data);
        dedup_and_sort(&mut all_txs);
        let total_txs = all_txs.len();
        let points = extract_balance_points(address, &all_txs);
        log(&mut phases, "RTT 1", &format!(
            "Short-circuit! {} txs -> {} balance points (1 RTT)", total_txs, points.len()
        ), &start);
        return Ok(BalanceTimeline {
            address: address.to_string(),
            points,
            phases,
            fetch_duration_ms: start.elapsed().as_millis(),
            rpc_calls: client.calls(),
            total_txs_fetched: total_txs,
        });
    }

    // We have a gap between the two probes
    let min_slot = asc_data.first().and_then(|d| d["slot"].as_u64()).unwrap_or(0);
    let max_slot = desc_data.first().and_then(|d| d["slot"].as_u64()).unwrap_or(0);
    let gap_start = asc_last_slot + 1;
    let gap_end = desc_last_slot.saturating_sub(1);
    let gap_width = gap_end.saturating_sub(gap_start) + 1;

    if let (Some(first), Some(last)) = (asc_data.first(), desc_data.first()) {
        let first_bt = first["blockTime"].as_i64().unwrap_or(0);
        let last_bt = last["blockTime"].as_i64().unwrap_or(0);
        log(&mut phases, "RTT 1", &format!(
            "Gap: {} slots ({} days), range {} -> {}",
            gap_width, (last_bt - first_bt) / 86400, min_slot, max_slot
        ), &start);
    }

    // ── RTT 2: Parallel sig discovery (signatures mode, 1000/page) ──
    // Split gap into 128 segments, fetch sigs for each in parallel.
    // Signatures mode is 10x more efficient: 1000 sigs/page vs 100 txs/page.
    let num_segments = INITIAL_SEGMENTS;
    let seg_width = (gap_width / num_segments as u64).max(1);

    let mut sig_futures = Vec::with_capacity(num_segments);
    for i in 0..num_segments {
        let seg_start = gap_start + (i as u64 * seg_width);
        let seg_end = if i == num_segments - 1 { gap_end }
                      else { gap_start + ((i as u64 + 1) * seg_width) - 1 };
        if seg_start > gap_end { break; }
        // Up to 5 pages per segment = 5,000 sigs per segment (640k max total)
        sig_futures.push(client.fetch_sigs_in_slot_range(address, seg_start, seg_end, 5));
    }

    let sig_results = join_all(sig_futures).await;
    let mut all_sigs: Vec<Value> = Vec::new();
    for result in sig_results {
        all_sigs.extend(result?);
    }

    // Dedup and cap sigs
    let mut seen_sigs = HashSet::new();
    all_sigs.retain(|s| {
        let sig = s["signature"].as_str().unwrap_or("").to_string();
        !sig.is_empty() && seen_sigs.insert(sig)
    });
    all_sigs.sort_by_key(|s| s["slot"].as_u64().unwrap_or(0));
    let capped = all_sigs.len() > MAX_SIGS;
    if capped {
        all_sigs.truncate(MAX_SIGS);
    }

    log(&mut phases, "RTT 2", &format!(
        "Sig discovery: {} segments, {} unique sigs{} ({} calls)",
        num_segments, all_sigs.len(),
        if capped { format!(" (capped from {})", seen_sigs.len()) } else { String::new() },
        client.calls()
    ), &start);

    if all_sigs.is_empty() {
        // Only the probe data
        let mut all_txs = asc_data;
        all_txs.extend(desc_data);
        dedup_and_sort(&mut all_txs);
        let total_txs = all_txs.len();
        let points = extract_balance_points(address, &all_txs);
        return Ok(BalanceTimeline {
            address: address.to_string(),
            points,
            phases,
            fetch_duration_ms: start.elapsed().as_millis(),
            rpc_calls: client.calls(),
            total_txs_fetched: total_txs,
        });
    }

    // ── RTT 3: Parallel full tx fetch (zero sequential pagination) ──
    // We know every sig's slot from RTT 2. Group into batches of ~100 sigs
    // with non-overlapping slot ranges. Fire ALL batches simultaneously.
    // This eliminates sequential pagination entirely.
    let total_sigs = all_sigs.len();
    let batch_target = 100usize;
    let num_batches = ((total_sigs + batch_target - 1) / batch_target).max(1).min(MAX_FULL_BATCHES);
    let batch_size = (total_sigs + num_batches - 1) / num_batches;

    let mut full_futures = Vec::with_capacity(num_batches);
    for chunk in all_sigs.chunks(batch_size) {
        // Use the first sig's slot as range start, last sig's slot as range end
        let slot_min = chunk.first().unwrap()["slot"].as_u64().unwrap_or(0);
        let slot_max = chunk.last().unwrap()["slot"].as_u64().unwrap_or(u64::MAX);
        // Single-page fetch: no pagination needed since we sized batches to ~100 sigs
        let filters = Some(json!({"slot": {"gte": slot_min, "lte": slot_max}}));
        full_futures.push(
            client.rpc_call(address, "full", "asc", FULL_PAGE_LIMIT, None, filters, true)
        );
    }

    log(&mut phases, "RTT 3", &format!(
        "Firing {} parallel single-page fetches (0 sequential pagination)",
        num_batches
    ), &start);

    let full_results = join_all(full_futures).await;
    let mut all_txs: Vec<Value> = Vec::new();
    all_txs.extend(asc_data);
    all_txs.extend(desc_data);
    for result in full_results {
        let (data, _) = result?;
        all_txs.extend(data);
    }

    log(&mut phases, "RTT 3", &format!(
        "Full fetch done: {} batches, {} raw txs ({} calls)",
        num_batches, all_txs.len(), client.calls()
    ), &start);

    // ── Extract ──
    dedup_and_sort(&mut all_txs);
    let total_txs = all_txs.len();
    let points = extract_balance_points(address, &all_txs);

    log(&mut phases, "Done", &format!(
        "{} unique txs -> {} balance points, {} RPC calls (retries: {}, slow>2s: {})",
        total_txs, points.len(), client.calls(),
        client.retry_count.load(Ordering::Relaxed),
        client.slow_count.load(Ordering::Relaxed),
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

fn dedup_and_sort(txs: &mut Vec<Value>) {
    let mut seen = HashSet::new();
    txs.retain(|tx| {
        // Try full tx sig first, then bare sig
        let sig = tx["transaction"]["signatures"][0].as_str()
            .or_else(|| tx["signature"].as_str())
            .unwrap_or("")
            .to_string();
        !sig.is_empty() && seen.insert(sig)
    });
    txs.sort_by(|a, b| {
        let sa = a["slot"].as_u64().unwrap_or(0);
        let sb = b["slot"].as_u64().unwrap_or(0);
        let ia = a["transactionIndex"].as_u64().unwrap_or(0);
        let ib = b["transactionIndex"].as_u64().unwrap_or(0);
        (sa, ia).cmp(&(sb, ib))
    });
}

fn extract_balance_points(address: &str, txs: &[Value]) -> Vec<BalancePoint> {
    let mut points = Vec::with_capacity(txs.len());
    for tx in txs {
        let slot = tx["slot"].as_u64().unwrap_or(0);
        let block_time = tx["blockTime"].as_i64().unwrap_or(0);

        let account_keys = &tx["transaction"]["message"]["accountKeys"];
        let mut idx = None;
        let mut offset = 0usize;

        if let Some(keys) = account_keys.as_array() {
            idx = keys.iter().position(|k| {
                k.as_str() == Some(address) || k["pubkey"].as_str() == Some(address)
            });
            offset = keys.len();
        }

        // v0 transactions with Address Lookup Tables
        if idx.is_none() {
            let loaded = &tx["meta"]["loadedAddresses"];
            if let Some(writable) = loaded["writable"].as_array() {
                if let Some(pos) = writable.iter().position(|k| k.as_str() == Some(address)) {
                    idx = Some(offset + pos);
                } else {
                    offset += writable.len();
                }
            }
            if idx.is_none() {
                if let Some(readonly) = loaded["readonly"].as_array() {
                    if let Some(pos) = readonly.iter().position(|k| k.as_str() == Some(address)) {
                        idx = Some(offset + pos);
                    }
                }
            }
        }

        if let Some(i) = idx {
            let pre = tx["meta"]["preBalances"][i].as_u64().unwrap_or(0);
            let post = tx["meta"]["postBalances"][i].as_u64().unwrap_or(0);
            if pre != post {
                points.push(BalancePoint {
                    slot,
                    block_time,
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
struct BalanceQuery {
    address: String,
}

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
            json!({"phase": "start", "message": format!("Starting analysis for {}", address)}).to_string()
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

async fn handle_health() -> &'static str {
    "ok"
}

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

    let max_concurrent: usize = std::env::var("MAX_CONCURRENT")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(128);

    let client = Arc::new(HeliusClient::new(&rpc_url, &api_key, max_concurrent));

    let mode = std::env::args().nth(1).unwrap_or_default();

    if mode == "query" {
        let address = std::env::args().nth(2).expect("Usage: sol-pnl query <ADDRESS>");
        eprintln!("─── Query: {} ───", &address[..12.min(address.len())]);
        match compute_balance_timeline(&client, &address).await {
            Ok(tl) => {
                eprintln!("\n  Results:");
                eprintln!("    Latency:    {} ms", tl.fetch_duration_ms);
                eprintln!("    RPC calls:  {}", tl.rpc_calls);
                eprintln!("    Txs fetched: {}", tl.total_txs_fetched);
                eprintln!("    Bal points: {}", tl.points.len());
                if let (Some(first), Some(last)) = (tl.points.first(), tl.points.last()) {
                    eprintln!("    Start:      {:.9} SOL", first.sol_balance);
                    eprintln!("    End:        {:.9} SOL", last.sol_balance);
                }
            }
            Err(e) => eprintln!("    ERROR: {}", e),
        }
    } else if mode == "serve" {
        let port: u16 = std::env::var("PORT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(3001);

        let app = Router::new()
            .route("/api/balance", get(handle_balance))
            .route("/api/balance/stream", get(handle_balance_stream))
            .route("/health", get(handle_health))
            .layer(CorsLayer::permissive())
            .with_state(client);

        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
        eprintln!("  Server running on http://localhost:{}", port);
        eprintln!("  GET /api/balance?address=<ADDR>          -> JSON response");
        eprintln!("  GET /api/balance/stream?address=<ADDR>   -> SSE stream with phase logs");
        axum::serve(listener, app).await?;
    } else {
        // Benchmark mode
        let rpc_display = rpc_url.split('?').next().unwrap_or(&rpc_url);
        eprintln!("  RPC: {} (max {} concurrent)", rpc_display, max_concurrent);
        eprintln!("  Algorithm: Probe + 128-Segment Sig Discovery + Parallel Full Fetch");
        eprintln!("  Compression: gzip/brotli/deflate | Filter: balanceChanged");
        eprintln!();

        let test_addresses = vec![
            ("Busy", "39cUkqsh91Vx5EtxuRENWCMBXeiwL7GDaNarTYSns75V"),
            ("Medium", "FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH"),
            ("Sparse", "HN7cABqLq46Es1jh92dQQisAq662SmxELLLsHHe4YWrH"),
            ("Periodic", "CKs1E69a2e9TmH4mKKLrXFF8kD3ZnwKjoEuXa6sz9WqX"),
            ("Active", "H8sMJSCQxfKiFTCfDR3DUMLPwcRbM61LGFJ8N4dK3WjS"),
            ("120k", "9ogeCJhwdQ3hLbhSqTterv5nWeetnu9Z2LxjWuN6krhq"),
        ];

        let mut results: Vec<(String, BalanceTimeline)> = Vec::new();

        for (name, address) in &test_addresses {
            eprintln!("─── {} ({}) ───", name, &address[..12]);
            match compute_balance_timeline(&client, address).await {
                Ok(tl) => {
                    if let (Some(first), Some(last)) = (tl.points.first(), tl.points.last()) {
                        let fd = DateTime::from_timestamp(first.block_time, 0)
                            .map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
                        let ld = DateTime::from_timestamp(last.block_time, 0)
                            .map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
                        eprintln!("    {:.4} SOL @ {} -> {:.4} SOL @ {}",
                            first.sol_balance, fd, last.sol_balance, ld);
                    }
                    eprintln!();
                    results.push((name.to_string(), tl));
                }
                Err(e) => eprintln!("    ERROR: {}\n", e),
            }
        }

        println!("\n┌────────────────┬──────────┬───────┬────────┬─────────┐");
        println!("│ Address        │ Latency  │ Calls │ Txs    │ BalPts  │");
        println!("├────────────────┼──────────┼───────┼────────┼─────────┤");
        for (name, tl) in &results {
            println!("│ {:<14} │ {:>5} ms │ {:>5} │ {:>6} │ {:>7} │",
                name, tl.fetch_duration_ms, tl.rpc_calls, tl.total_txs_fetched, tl.points.len());
        }
        println!("├────────────────┼──────────┼───────┼────────┼─────────┤");
        if !results.is_empty() {
            let lats: Vec<u128> = results.iter().map(|r| r.1.fetch_duration_ms).collect();
            let avg = lats.iter().sum::<u128>() / lats.len() as u128;
            println!("│ AVG            │ {:>5} ms │       │        │         │", avg);
            println!("│ MIN            │ {:>5} ms │       │        │         │", lats.iter().min().unwrap());
            println!("│ MAX            │ {:>5} ms │       │        │         │", lats.iter().max().unwrap());
        }
        println!("└────────────────┴──────────┴───────┴────────┴─────────┘");
    }

    Ok(())
}
