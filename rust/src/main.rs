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
const INITIAL_SEGMENTS: usize = 256;
const NUM_CLIENTS: usize = 64;

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

struct HeliusClient {
    clients: Vec<Client>,
    endpoint: String,
    call_count: AtomicU32,
    next_client: AtomicU32,
    semaphore: Semaphore,
}

impl HeliusClient {
    fn new(rpc_url: &str, api_key: &str, max_concurrent: usize) -> Self {
        let endpoint = if rpc_url.contains("api-key") {
            rpc_url.to_string()
        } else {
            format!("{}/?api-key={}", rpc_url.trim_end_matches('/'), api_key)
        };
        let clients: Vec<Client> = (0..NUM_CLIENTS)
            .map(|_| {
                Client::builder()
                    .pool_max_idle_per_host(max_concurrent / NUM_CLIENTS + 1)
                    .tcp_keepalive(std::time::Duration::from_secs(30))
                    .gzip(true)
                    .brotli(true)
                    .deflate(true)
                    .http2_initial_stream_window_size(2 * 1024 * 1024)
                    .http2_initial_connection_window_size(32 * 1024 * 1024)
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
        }
    }

    fn reset_calls(&self) {
        self.call_count.store(0, Ordering::Relaxed);
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

        let mut last_err = String::new();
        for attempt in 0..5 {
            if attempt > 0 {
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
}

// ── Pipelined Segment Processor ───────────────────────────────────────────
//
// Each segment independently:
//   1. Discovers sigs (signatures mode, 1000/page, 1-3 pages)
//   2. Immediately fires full-fetch batches for those sigs (all parallel)
//   3. Returns all full txs
//
// All 128 segments run concurrently. No segment waits for another.
// The semaphore keeps ~256 calls in flight at all times, mixing sig + full
// calls from different segments. Zero idle time between phases.

async fn process_segment(
    client: &HeliusClient,
    address: &str,
    slot_gte: u64,
    slot_lte: u64,
) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
    // Step 1: Discover sigs (1 call + 1 overflow if needed)
    let filters = Some(json!({"slot": {"gte": slot_gte, "lte": slot_lte}}));
    let (mut all_sigs, next_token) = client
        .rpc_call(address, "signatures", "asc", SIG_PAGE_LIMIT, None, filters.clone(), true)
        .await?;

    if all_sigs.len() >= SIG_PAGE_LIMIT as usize {
        if let Some(token) = next_token {
            let (more, _) = client
                .rpc_call(address, "signatures", "asc", SIG_PAGE_LIMIT, Some(&token), filters, true)
                .await?;
            all_sigs.extend(more);
        }
    }

    if all_sigs.is_empty() {
        return Ok(vec![]);
    }

    // Dedup within segment
    let mut seen = HashSet::new();
    all_sigs.retain(|s| {
        let sig = s["signature"].as_str().unwrap_or("").to_string();
        !sig.is_empty() && seen.insert(sig)
    });
    all_sigs.sort_by_key(|s| s["slot"].as_u64().unwrap_or(0));

    // Step 2: Create full-fetch batches (~100 sigs each) and fire ALL in parallel
    let batch_size = 100;
    let mut futures = Vec::new();

    for chunk in all_sigs.chunks(batch_size) {
        let slot_min = chunk.first().unwrap()["slot"].as_u64().unwrap_or(0);
        let slot_max = chunk.last().unwrap()["slot"].as_u64().unwrap_or(u64::MAX);
        let f = Some(json!({"slot": {"gte": slot_min, "lte": slot_max}}));
        futures.push(client.rpc_call(address, "full", "asc", FULL_PAGE_LIMIT, None, f, true));
    }

    let results = join_all(futures).await;
    let mut txs = Vec::new();
    for result in results {
        let (data, _) = result?;
        txs.extend(data);
    }

    Ok(txs)
}

// ── Algorithm ─────────────────────────────────────────────────────────────

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

    // ── RTT 1: Bidirectional probe ──
    let (asc_result, desc_result) = tokio::join!(
        client.rpc_call(address, "full", "asc", FULL_PAGE_LIMIT, None, None, true),
        client.rpc_call(address, "full", "desc", FULL_PAGE_LIMIT, None, None, true),
    );
    let (asc_data, asc_next) = asc_result?;
    let (desc_data, desc_next) = desc_result?;

    log(&mut phases, "Probe", &format!(
        "asc={} desc={}", asc_data.len(), desc_data.len()
    ), &start);

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

    // Short-circuit
    let asc_complete = asc_data.len() < FULL_PAGE_LIMIT as usize || asc_next.is_none();
    let desc_complete = desc_data.len() < FULL_PAGE_LIMIT as usize || desc_next.is_none();
    let asc_last_slot = asc_data.last().and_then(|d| d["slot"].as_u64()).unwrap_or(0);
    let desc_last_slot = desc_data.last().and_then(|d| d["slot"].as_u64()).unwrap_or(u64::MAX);

    if asc_complete || desc_complete || asc_last_slot >= desc_last_slot {
        let mut all_txs = asc_data;
        all_txs.extend(desc_data);
        dedup_and_sort(&mut all_txs);
        let total_txs = all_txs.len();
        let points = extract_balance_points(address, &all_txs);
        log(&mut phases, "Probe", &format!(
            "Short-circuit! {} txs -> {} bal pts", total_txs, points.len()
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

    let gap_start = asc_last_slot + 1;
    let gap_end = desc_last_slot.saturating_sub(1);
    let gap_width = gap_end.saturating_sub(gap_start) + 1;
    let seg_width = (gap_width / INITIAL_SEGMENTS as u64).max(1);

    log(&mut phases, "Probe", &format!("Gap: {} slots", gap_width), &start);

    // ── Pipelined: 128 segments, each does sig discovery → full fetch independently ──
    let mut segment_futures = Vec::with_capacity(INITIAL_SEGMENTS);
    for i in 0..INITIAL_SEGMENTS {
        let seg_start = gap_start + (i as u64 * seg_width);
        let seg_end = if i == INITIAL_SEGMENTS - 1 { gap_end }
                      else { gap_start + ((i as u64 + 1) * seg_width) - 1 };
        if seg_start > gap_end { break; }
        segment_futures.push(process_segment(client, address, seg_start, seg_end));
    }

    let segment_results = join_all(segment_futures).await;

    let mut all_txs: Vec<Value> = Vec::new();
    all_txs.extend(asc_data);
    all_txs.extend(desc_data);
    for result in segment_results {
        all_txs.extend(result?);
    }

    log(&mut phases, "Fetch", &format!(
        "{} raw txs, {} calls", all_txs.len(), client.calls()
    ), &start);

    // ── Extract ──
    dedup_and_sort(&mut all_txs);
    let total_txs = all_txs.len();
    let points = extract_balance_points(address, &all_txs);

    log(&mut phases, "Done", &format!(
        "{} txs -> {} bal pts, {} calls",
        total_txs, points.len(), client.calls()
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
        .ok().and_then(|v| v.parse().ok()).unwrap_or(256);

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
        axum::serve(listener, app).await?;
    } else {
        let rpc_display = rpc_url.split('?').next().unwrap_or(&rpc_url);
        eprintln!("  RPC: {} (max {} concurrent, {} HTTP clients)", rpc_display, max_concurrent, NUM_CLIENTS);
        eprintln!();

        let test_addresses = vec![
            ("Sparse", "HN7cABqLq46Es1jh92dQQisAq662SmxELLLsHHe4YWrH"),
            ("Periodic", "CKs1E69a2e9TmH4mKKLrXFF8kD3ZnwKjoEuXa6sz9WqX"),
            ("Busy", "39cUkqsh91Vx5EtxuRENWCMBXeiwL7GDaNarTYSns75V"),
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
