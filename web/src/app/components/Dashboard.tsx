"use client";

import { useState, useRef, useEffect } from "react";

interface PhaseLog {
  phase: string;
  message: string;
  elapsed_ms: number;
}

interface BalancePoint {
  slot: number;
  block_time: number;
  pre_balance: number;
  post_balance: number;
  sol_balance: number;
}

interface Summary {
  fetch_duration_ms: number;
  rpc_calls: number;
  total_txs_fetched: number;
  balance_points: number;
}

function formatMs(ms: number) {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function formatSol(lamports: number) {
  const sol = lamports / 1e9;
  if (sol > 1000) return sol.toFixed(0);
  if (sol > 1) return sol.toFixed(2);
  return sol.toFixed(4);
}

// ── Heatmap Grid: each cell = 1 day, white = had tx, dark = no tx ──
function TimelineGrid({ points }: { points: BalancePoint[] }) {
  if (points.length === 0) return null;

  const firstDay = Math.floor(points[0].block_time / 86400);
  const lastDay = Math.floor(points[points.length - 1].block_time / 86400);
  const totalDays = lastDay - firstDay + 1;

  // Count txs per day
  const dayMap = new Map<number, number>();
  for (const p of points) {
    const day = Math.floor(p.block_time / 86400);
    dayMap.set(day, (dayMap.get(day) || 0) + 1);
  }

  const maxCount = Math.max(...dayMap.values(), 1);

  // Generate cells - cap at 192 (32x6) for the grid, sample if more days
  const maxCells = 192;
  const step = Math.max(1, Math.floor(totalDays / maxCells));
  const cells: { opacity: number }[] = [];

  for (let i = 0; i < Math.min(totalDays, maxCells); i++) {
    const dayOffset = Math.floor(i * step);
    const day = firstDay + dayOffset;
    const count = dayMap.get(day) || 0;
    const opacity = count === 0 ? 0.06 : 0.15 + (count / maxCount) * 0.85;
    cells.push({ opacity });
  }

  return (
    <div className="sensor-grid">
      {cells.map((c, i) => (
        <div key={i} className="sensor-node" style={{ opacity: c.opacity }} />
      ))}
    </div>
  );
}

// ── Balance chart for transition zone ──
function BalanceChartSVG({ points }: { points: BalancePoint[] }) {
  if (points.length < 2) return null;

  const w = 1200, h = 180;
  const pL = 0, pR = 0, pT = 20, pB = 10;

  const minT = points[0].block_time;
  const maxT = points[points.length - 1].block_time;
  const bals = points.map((p) => p.sol_balance);
  const minB = Math.min(...bals);
  const maxB = Math.max(...bals);
  const bRange = maxB - minB || 1;

  const x = (t: number) => pL + ((t - minT) / (maxT - minT || 1)) * (w - pL - pR);
  const y = (b: number) => pT + (1 - (b - minB) / bRange) * (h - pT - pB);

  const step = Math.max(1, Math.floor(points.length / 1500));
  const sampled = points.filter((_, i) => i % step === 0 || i === points.length - 1);

  const line = sampled
    .map((p, i) => `${i === 0 ? "M" : "L"}${x(p.block_time)},${y(p.sol_balance)}`)
    .join(" ");
  const area =
    line +
    ` L${x(sampled[sampled.length - 1].block_time)},${h}` +
    ` L${x(sampled[0].block_time)},${h} Z`;

  return (
    <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none">
      <defs>
        <linearGradient id="chartGrad" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="#f5f0eb" stopOpacity="0.25" />
          <stop offset="100%" stopColor="#f5f0eb" stopOpacity="0.02" />
        </linearGradient>
      </defs>
      <path d={area} fill="url(#chartGrad)" />
      <path d={line} fill="none" stroke="#f5f0eb" strokeWidth="1.5" opacity="0.6" />
    </svg>
  );
}

// ── Clock ──
function Clock() {
  const [time, setTime] = useState("");
  useEffect(() => {
    const tick = () => {
      const now = new Date();
      setTime(
        now.toLocaleTimeString("en-US", { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit" }) +
          "." + Math.floor(now.getMilliseconds() / 100)
      );
    };
    tick();
    const id = setInterval(tick, 100);
    return () => clearInterval(id);
  }, []);
  return <div className="value">{time}</div>;
}

export function Dashboard() {
  const [address, setAddress] = useState("");
  const [loading, setLoading] = useState(false);
  const [phases, setPhases] = useState<PhaseLog[]>([]);
  const [summary, setSummary] = useState<Summary | null>(null);
  const [points, setPoints] = useState<BalancePoint[]>([]);
  const [error, setError] = useState<string | null>(null);
  const esRef = useRef<EventSource | null>(null);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!address.trim() || loading) return;

    setLoading(true);
    setPhases([]);
    setSummary(null);
    setPoints([]);
    setError(null);

    if (esRef.current) esRef.current.close();

    const es = new EventSource(
      `/api/balance/stream?address=${encodeURIComponent(address.trim())}`
    );
    esRef.current = es;

    es.addEventListener("phase", (e) => {
      try { setPhases((prev) => [...prev, JSON.parse(e.data)]); } catch {}
    });
    es.addEventListener("summary", (e) => {
      try { setSummary(JSON.parse(e.data)); } catch {}
    });
    es.addEventListener("data", (e) => {
      try { setPoints(JSON.parse(e.data)); } catch {}
    });
    es.addEventListener("server_error", (e) => {
      try { setError(JSON.parse(e.data).error); } catch {}
      es.close();
      setLoading(false);
    });
    es.addEventListener("error", () => {
      if (es.readyState === EventSource.CLOSED) {
        if (!summary && points.length === 0) setError("Connection lost");
        setLoading(false);
      }
    });
    es.addEventListener("done", () => {
      es.close();
      setLoading(false);
    });
  };

  const lastBal = points.length > 0 ? points[points.length - 1] : null;
  const firstBal = points.length > 0 ? points[0] : null;

  return (
    <>
      {/* ══ DARK ZONE ══ */}
      <div className="zone-dark">
        <header>
          <div className="header-col">
            <span className="label">SYSTEM OPERATION</span>
            <div>SOL BALANCE // TRACKER</div>
          </div>
          <div className="header-col">
            <span className="label">ACTIVE PROTOCOL</span>
            <div>PARALLEL_RPC_V3 // EXPONENTIAL_PROBE</div>
          </div>
          <div className="header-col">
            <span className="label">LOCAL TIME</span>
            <Clock />
          </div>
        </header>

        {/* Search */}
        <form className="search-bar" onSubmit={handleSubmit}>
          <input
            type="text"
            placeholder="ENTER WALLET ADDRESS..."
            value={address}
            onChange={(e) => setAddress(e.target.value)}
            disabled={loading}
          />
          <button type="submit" disabled={loading}>
            {loading ? <>SCANNING<span className="pulse-dot" /></> : "ANALYZE"}
          </button>
        </form>

        <div className="primary-vis-layout">
          {/* Hero: current balance */}
          <div>
            <span className="label">
              {points.length > 0 ? "CURRENT BALANCE" : "AWAITING TARGET"}
            </span>
            <div className="dot-hero">
              {lastBal ? (
                <>
                  {formatSol(lastBal.post_balance)}
                  <span className="unit">SOL</span>
                </>
              ) : (
                <span style={{ opacity: 0.15 }}>--</span>
              )}
            </div>
          </div>

          {/* Sensor grid: transaction timeline */}
          <div className="sensor-container">
            <div style={{ display: "flex", justifyContent: "space-between" }}>
              <span className="label">TRANSACTION TIMELINE</span>
              <span className="value" style={{ opacity: 0.5 }}>
                {points.length > 0
                  ? `${points.length} EVENTS / ${Math.ceil(
                      (points[points.length - 1].block_time - points[0].block_time) / 86400
                    )} DAYS`
                  : "NO DATA"}
              </span>
            </div>
            {points.length > 0 ? (
              <TimelineGrid points={points} />
            ) : (
              <div className="sensor-grid">
                {Array.from({ length: 192 }, (_, i) => (
                  <div key={i} className="sensor-node" style={{ opacity: 0.06 }} />
                ))}
              </div>
            )}

            {/* Phase stream */}
            {phases.length > 0 && (
              <div className="phase-stream">
                {phases.slice(-4).map((p, i) => (
                  <div key={i} className="phase-line">
                    <span className="phase-tag">{p.phase}</span>
                    <span className="phase-time">{formatMs(p.elapsed_ms)}</span>
                    <span>{p.message}</span>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      </div>

      {/* ══ TRANSITION: balance chart ══ */}
      <div className="zone-transition">
        {points.length >= 2 ? (
          <BalanceChartSVG points={points} />
        ) : (
          <div className="empty-state" style={{ color: "rgba(255,255,255,0.1)" }}>
            {loading ? "PROCESSING..." : "BALANCE CURVE"}
          </div>
        )}
      </div>

      {/* ══ LIGHT ZONE ══ */}
      <div className="zone-light">
        {error && (
          <div style={{ padding: "16px 0", color: "#dc2626", fontSize: "11px" }}>
            ERROR: {error}
          </div>
        )}

        <div className="data-matrix">
          {/* Col 1: Performance Stats */}
          <div className="data-col">
            <div className="col-header">PERFORMANCE METRICS</div>
            {summary ? (
              <ul className="data-list">
                <li className="data-row">
                  <span className="row-key">TOTAL_LATENCY</span>
                  <span className="row-val">{formatMs(summary.fetch_duration_ms)}</span>
                </li>
                <li className="data-row">
                  <span className="row-key">RPC_CALLS</span>
                  <span className="row-val">{summary.rpc_calls}</span>
                </li>
                <li className="data-row">
                  <span className="row-key">TXS_FETCHED</span>
                  <span className="row-val">{summary.total_txs_fetched.toLocaleString()}</span>
                </li>
                <li className="data-row">
                  <span className="row-key">BALANCE_POINTS</span>
                  <span className="row-val">{summary.balance_points.toLocaleString()}</span>
                </li>
                <li className="data-row">
                  <span className="row-key">AVG_PER_CALL</span>
                  <span className="row-val">
                    {summary.rpc_calls > 0
                      ? formatMs(Math.round(summary.fetch_duration_ms / summary.rpc_calls))
                      : "--"}
                  </span>
                </li>
                <li className="data-row">
                  <span className="row-key">THROUGHPUT</span>
                  <span className="row-val">
                    {summary.fetch_duration_ms > 0
                      ? `${Math.round((summary.total_txs_fetched / summary.fetch_duration_ms) * 1000)}/s`
                      : "--"}
                  </span>
                </li>
              </ul>
            ) : (
              <div style={{ opacity: 0.2, paddingTop: 8 }}>AWAITING DATA</div>
            )}

            {/* Spark bars for phases */}
            {summary && (
              <div style={{ marginTop: 24 }}>
                {phases
                  .filter((p) => p.phase.startsWith("Phase"))
                  .map((p, i) => (
                    <div className="spark-row" key={i}>
                      <div style={{ display: "flex", justifyContent: "space-between" }}>
                        <span className="row-key" style={{ fontSize: 10 }}>{p.phase}</span>
                        <span className="row-val" style={{ fontSize: 10 }}>{formatMs(p.elapsed_ms)}</span>
                      </div>
                      <div className="spark-bar-container">
                        <div
                          className="spark-bar"
                          style={{
                            width: `${Math.min(100, (p.elapsed_ms / summary.fetch_duration_ms) * 100)}%`,
                          }}
                        />
                      </div>
                    </div>
                  ))}
              </div>
            )}
          </div>

          {/* Col 2: Algorithm Pipeline */}
          <div className="data-col">
            <div className="col-header">ALGORITHM PIPELINE</div>
            <ul className="data-list">
              {phases.length > 0 ? (
                phases.map((p, i) => (
                  <li className="data-row" key={i}>
                    <span className="row-key" style={{ maxWidth: "60%", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                      {p.message}
                    </span>
                    <span className="row-val">{formatMs(p.elapsed_ms)}</span>
                  </li>
                ))
              ) : (
                <div style={{ opacity: 0.2, paddingTop: 8 }}>IDLE</div>
              )}
            </ul>
          </div>

          {/* Col 3: Balance Snapshot */}
          <div className="data-col">
            <div className="col-header">BALANCE SNAPSHOT</div>
            {firstBal && lastBal ? (
              <ul className="data-list">
                <li className="data-row">
                  <span className="row-key">FIRST_SEEN</span>
                  <span className="row-val">{new Date(firstBal.block_time * 1000).toLocaleDateString()}</span>
                </li>
                <li className="data-row">
                  <span className="row-key">FIRST_BAL</span>
                  <span className="row-val">{firstBal.sol_balance.toFixed(4)} SOL</span>
                </li>
                <li className="data-row">
                  <span className="row-key">LAST_SEEN</span>
                  <span className="row-val">{new Date(lastBal.block_time * 1000).toLocaleDateString()}</span>
                </li>
                <li className="data-row">
                  <span className="row-key">LAST_BAL</span>
                  <span className="row-val">{lastBal.sol_balance.toFixed(4)} SOL</span>
                </li>
                <li className="data-row">
                  <span className="row-key">NET_CHANGE</span>
                  <span className={`row-val ${lastBal.post_balance >= firstBal.post_balance ? "positive" : "negative"}`}>
                    {((lastBal.post_balance - firstBal.post_balance) / 1e9).toFixed(4)} SOL
                  </span>
                </li>
                <li className="data-row">
                  <span className="row-key">MAX_BAL</span>
                  <span className="row-val">
                    {Math.max(...points.map((p) => p.sol_balance)).toFixed(4)} SOL
                  </span>
                </li>
                <li className="data-row">
                  <span className="row-key">MIN_BAL</span>
                  <span className="row-val">
                    {Math.min(...points.map((p) => p.sol_balance)).toFixed(4)} SOL
                  </span>
                </li>
              </ul>
            ) : (
              <div style={{ opacity: 0.2, paddingTop: 8 }}>NO TARGET</div>
            )}
          </div>

          {/* Col 4: Recent Transactions */}
          <div className="data-col">
            <div className="col-header">RECENT EVENTS // TRACE</div>
            <ul className="data-list">
              {points.length > 0 ? (
                points.slice(-20).reverse().map((p, i) => {
                  const change = (p.post_balance - p.pre_balance) / 1e9;
                  const date = new Date(p.block_time * 1000);
                  const ts = date.toLocaleTimeString("en-US", { hour12: false, hour: "2-digit", minute: "2-digit" });
                  const ds = `${(date.getMonth() + 1).toString().padStart(2, "0")}/${date.getDate().toString().padStart(2, "0")}`;
                  return (
                    <li className="data-row" key={i}>
                      <span className="row-key">{ds} {ts}</span>
                      <span className={`row-val ${change >= 0 ? "positive" : "negative"}`}>
                        {change >= 0 ? "+" : ""}{change.toFixed(4)}
                      </span>
                    </li>
                  );
                })
              ) : (
                <div style={{ opacity: 0.2, paddingTop: 8 }}>AWAITING SCAN</div>
              )}
            </ul>
          </div>
        </div>
      </div>
    </>
  );
}
