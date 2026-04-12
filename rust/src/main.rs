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

#[derive(Debug, Serialize)]
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
        let mut filter_obj = filters.unwrap_or(json!({}));
        // always use balanceChanged to only get txs that moved SOL
        filter_obj["tokenAccounts"] = json!("balanceChanged");
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

        let resp = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await?
            .json::<RpcResponse>()
            .await?;

        if let Some(err) = resp.error {
            return Err(format!("RPC error: {}", err).into());
        }

        match resp.result {
            Some(r) => Ok((r.data, r.pagination_token)),
            None => Ok((vec![], None)),
        }
    }

    async fn fetch_sigs_in_range(
        &self,
        address: &str,
        bt_gte: i64,
        bt_lte: i64,
        max_pages: usize,
    ) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        let filters = Some(json!({"blockTime": {"gte": bt_gte, "lte": bt_lte}}));
        let mut pages = 0usize;
        loop {
            let (data, next) = self
                .rpc_call(address, "signatures", "asc", 1000, token.as_deref(), filters.clone())
                .await?;
            let done = data.is_empty() || next.is_none();
            all.extend(data);
            pages += 1;
            if done || pages >= max_pages { break; }
            token = next;
        }
        Ok(all)
    }

    async fn fetch_full_in_slot_range(
        &self,
        address: &str,
        slot_gte: u64,
        slot_lte: u64,
    ) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        let filters = Some(json!({"slot": {"gte": slot_gte, "lte": slot_lte}}));
        loop {
            let (data, next) = self
                .rpc_call(address, "full", "asc", 100, token.as_deref(), filters.clone())
                .await?;
            let done = data.is_empty() || next.is_none();
            all.extend(data);
            if done { break; }
            token = next;
        }
        Ok(all)
    }
}

// ── Algorithm ──────────────────────────────────────────────────────────────

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

    // Phase 0: Probe boundaries
    let (oldest, newest) = tokio::join!(
        client.rpc_call(address, "signatures", "asc", 1, None, None),
        client.rpc_call(address, "signatures", "desc", 1, None, None),
    );
    let (oldest_data, _) = oldest?;
    let (newest_data, _) = newest?;

    log(&mut phases, "Phase 0", &format!("Boundary probe complete ({} calls)", client.calls()), &start);

    if oldest_data.is_empty() || newest_data.is_empty() {
        log(&mut phases, "Phase 0", "No transactions found", &start);
        return Ok(BalanceTimeline {
            address: address.to_string(),
            points: vec![],
            phases,
            fetch_duration_ms: start.elapsed().as_millis(),
            rpc_calls: client.calls(),
            total_txs_fetched: 0,
        });
    }

    let first_bt = oldest_data[0]["blockTime"].as_i64().unwrap_or(0);
    let last_bt = newest_data[0]["blockTime"].as_i64().unwrap_or(0);
    let time_range = (last_bt - first_bt).max(1);

    log(&mut phases, "Phase 0", &format!("Time range: {} days ({} -> {})",
        time_range / 86400,
        DateTime::from_timestamp(first_bt, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
        DateTime::from_timestamp(last_bt, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
    ), &start);

    // Phase 1: Density probing
    let num_probes = ((time_range as f64 / 86400.0).ln().ceil() as usize).max(2).min(16);
    let probe_window = (time_range / 100).max(60).min(86400);

    let mut probe_futures = Vec::with_capacity(num_probes);
    for i in 0..num_probes {
        let frac = i as f64 / (num_probes - 1).max(1) as f64;
        let probe_time = first_bt + (frac * time_range as f64) as i64;
        let gte = probe_time;
        let lte = (probe_time + probe_window).min(last_bt);
        probe_futures.push(async move {
            let result = client
                .rpc_call(address, "signatures", "asc", 1000, None,
                    Some(json!({"blockTime": {"gte": gte, "lte": lte}})))
                .await;
            (gte, lte, result)
        });
    }

    let probe_results = join_all(probe_futures).await;

    let mut total_estimated_sigs = 0u64;
    for (_gte, _lte, result) in &probe_results {
        if let Ok((data, next_token)) = result {
            let count = if data.len() >= 1000 && next_token.is_some() {
                data.len() * 5
            } else {
                data.len()
            };
            total_estimated_sigs += count as u64;
        }
    }

    let avg_per_window = if num_probes > 0 {
        total_estimated_sigs as f64 / num_probes as f64
    } else { 1.0 };
    let windows_in_range = time_range as f64 / probe_window as f64;
    let estimated_total = (avg_per_window * windows_in_range) as u64;

    let max_sigs: u64 = std::env::var("MAX_SIGS")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(50_000);
    let capped = estimated_total > max_sigs;

    log(&mut phases, "Phase 1", &format!(
        "Density probe: {} probes, est ~{} sigs (avg {:.0}/window){}",
        num_probes, estimated_total, avg_per_window,
        if capped { format!(", capped to {}", max_sigs) } else { String::new() }
    ), &start);

    // Phase 2: Parallel sig discovery
    let effective_total = estimated_total.min(max_sigs);
    let target_sigs_per_chunk = 1000u64;

    let num_chunks = if time_range < 604800 {
        ((time_range / 3600) + 1).max(4).min(64) as usize
    } else if effective_total == 0 {
        2
    } else {
        ((effective_total / target_sigs_per_chunk) + 1).max(2).min(64) as usize
    };

    let (fetch_first_bt, fetch_last_bt) = if capped && estimated_total > 0 {
        let frac = max_sigs as f64 / estimated_total as f64;
        let shrunk_range = (time_range as f64 * frac) as i64;
        (last_bt - shrunk_range, last_bt)
    } else {
        (first_bt, last_bt)
    };

    let fetch_range = (fetch_last_bt - fetch_first_bt).max(1);
    let chunk_dur = fetch_range / num_chunks as i64;
    let mut sig_futures = Vec::with_capacity(num_chunks);

    for i in 0..num_chunks {
        let gte = fetch_first_bt + (i as i64 * chunk_dur);
        let lte = if i == num_chunks - 1 { fetch_last_bt + 1 }
                  else { fetch_first_bt + ((i as i64 + 1) * chunk_dur) - 1 };
        sig_futures.push(client.fetch_sigs_in_range(address, gte, lte, 5));
    }

    let sig_results = join_all(sig_futures).await;
    let mut all_sigs: Vec<Value> = Vec::new();
    for result in sig_results {
        all_sigs.extend(result?);
    }

    let mut seen = HashSet::new();
    all_sigs.retain(|s| {
        let sig = s["signature"].as_str().unwrap_or("").to_string();
        !sig.is_empty() && seen.insert(sig)
    });

    log(&mut phases, "Phase 2", &format!(
        "Sig discovery: {} chunks parallel, found {} unique sigs",
        num_chunks, all_sigs.len()
    ), &start);

    if all_sigs.is_empty() {
        return Ok(BalanceTimeline {
            address: address.to_string(),
            points: vec![],
            phases,
            fetch_duration_ms: start.elapsed().as_millis(),
            rpc_calls: client.calls(),
            total_txs_fetched: 0,
        });
    }

    all_sigs.sort_by_key(|s| s["slot"].as_u64().unwrap_or(0));
    let total_sigs = all_sigs.len();

    // Phase 3: Parallel full tx fetch
    let batch_target = 100usize;
    let num_batches = ((total_sigs + batch_target - 1) / batch_target).max(1).min(128);
    let batch_size = (total_sigs + num_batches - 1) / num_batches;

    let mut full_futures = Vec::with_capacity(num_batches);
    let mut prev_slot_max = 0u64;
    for (i, chunk) in all_sigs.chunks(batch_size).enumerate() {
        let slot_min = if i == 0 {
            chunk.first().unwrap()["slot"].as_u64().unwrap_or(0)
        } else {
            prev_slot_max + 1
        };
        let slot_max = chunk.last().unwrap()["slot"].as_u64().unwrap_or(u64::MAX);
        prev_slot_max = slot_max;
        full_futures.push(client.fetch_full_in_slot_range(address, slot_min, slot_max));
    }

    let full_results = join_all(full_futures).await;
    let mut all_txs: Vec<Value> = Vec::new();
    for result in full_results {
        all_txs.extend(result?);
    }

    let mut seen2 = HashSet::new();
    all_txs.retain(|tx| {
        let sig = tx["transaction"]["signatures"][0].as_str().unwrap_or("").to_string();
        !sig.is_empty() && seen2.insert(sig)
    });

    all_txs.sort_by(|a, b| {
        let sa = a["slot"].as_u64().unwrap_or(0);
        let sb = b["slot"].as_u64().unwrap_or(0);
        let ia = a["transactionIndex"].as_u64().unwrap_or(0);
        let ib = b["transactionIndex"].as_u64().unwrap_or(0);
        (sa, ia).cmp(&(sb, ib))
    });

    let total_txs_fetched = all_txs.len();

    log(&mut phases, "Phase 3", &format!(
        "Full tx fetch: {} batches parallel, {} txs fetched",
        num_batches, total_txs_fetched
    ), &start);

    // Phase 4: Extract
    let points = extract_balance_points(address, &all_txs);

    log(&mut phases, "Phase 4", &format!(
        "Extracted {} balance change points from {} txs",
        points.len(), total_txs_fetched
    ), &start);

    Ok(BalanceTimeline {
        address: address.to_string(),
        points,
        phases,
        fetch_duration_ms: start.elapsed().as_millis(),
        rpc_calls: client.calls(),
        total_txs_fetched,
    })
}

fn extract_balance_points(address: &str, txs: &[Value]) -> Vec<BalancePoint> {
    let mut points = Vec::with_capacity(txs.len());
    for tx in txs {
        let slot = tx["slot"].as_u64().unwrap_or(0);
        let block_time = tx["blockTime"].as_i64().unwrap_or(0);
        let account_keys = &tx["transaction"]["message"]["accountKeys"];
        let idx = if let Some(keys) = account_keys.as_array() {
            keys.iter().position(|k| {
                k.as_str() == Some(address) || k["pubkey"].as_str() == Some(address)
            })
        } else { None };

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
                // Send each phase log
                for phase in &tl.phases {
                    yield Ok(Event::default().event("phase").data(
                        serde_json::to_string(phase).unwrap_or_default()
                    ));
                }

                // Send summary
                yield Ok(Event::default().event("summary").data(json!({
                    "fetch_duration_ms": tl.fetch_duration_ms,
                    "rpc_calls": tl.rpc_calls,
                    "total_txs_fetched": tl.total_txs_fetched,
                    "balance_points": tl.points.len(),
                }).to_string()));

                // Send balance data
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

    let max_concurrent = std::env::var("MAX_CONCURRENT")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(32usize);

    let client = Arc::new(HeliusClient::new(&rpc_url, &api_key, max_concurrent));

    let mode = std::env::args().nth(1).unwrap_or_default();

    if mode == "serve" {
        // HTTP server mode
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
        // Benchmark mode (default)
        let rpc_display = rpc_url.split('?').next().unwrap_or(&rpc_url);
        eprintln!("  RPC: {} (max {} concurrent)", rpc_display, max_concurrent);
        eprintln!("  Tip: run with `serve` arg to start HTTP server");
        eprintln!();

        let test_addresses = vec![
            ("Busy", "39cUkqsh91Vx5EtxuRENWCMBXeiwL7GDaNarTYSns75V"),
            ("Medium", "FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH"),
            ("Sparse", "HN7cABqLq46Es1jh92dQQisAq662SmxELLLsHHe4YWrH"),
            ("Periodic", "CKs1E69a2e9TmH4mKKLrXFF8kD3ZnwKjoEuXa6sz9WqX"),
            ("Active", "H8sMJSCQxfKiFTCfDR3DUMLPwcRbM61LGFJ8N4dK3WjS"),
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
