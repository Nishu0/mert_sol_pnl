"use client";

import { useState, useRef } from "react";

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

function BalanceChart({ points }: { points: BalancePoint[] }) {
  if (points.length === 0) return null;

  const w = 1000, h = 280;
  const pL = 80, pR = 20, pT = 20, pB = 40;

  const minT = points[0].block_time;
  const maxT = points[points.length - 1].block_time;
  const bals = points.map((p) => p.sol_balance);
  const minB = Math.min(...bals);
  const maxB = Math.max(...bals);
  const bRange = maxB - minB || 1;

  const x = (t: number) => pL + ((t - minT) / (maxT - minT || 1)) * (w - pL - pR);
  const y = (b: number) => pT + (1 - (b - minB) / bRange) * (h - pT - pB);

  const step = Math.max(1, Math.floor(points.length / 2000));
  const sampled = points.filter((_, i) => i % step === 0 || i === points.length - 1);

  const line = sampled
    .map((p, i) => `${i === 0 ? "M" : "L"}${x(p.block_time)},${y(p.sol_balance)}`)
    .join(" ");
  const area =
    line +
    ` L${x(sampled[sampled.length - 1].block_time)},${h - pB}` +
    ` L${x(sampled[0].block_time)},${h - pB} Z`;

  const yTicks = Array.from({ length: 6 }, (_, i) => {
    const val = minB + (bRange * i) / 5;
    return { val, cy: y(val) };
  });

  const xTicks = Array.from({ length: 7 }, (_, i) => {
    const t = minT + ((maxT - minT) * i) / 6;
    return {
      label: new Date(t * 1000).toLocaleDateString("en-US", { month: "short", day: "numeric" }),
      cx: x(t),
    };
  });

  return (
    <div className="bg-[#111118] border border-[#1a1a22] rounded-lg p-6 mb-6">
      <div className="text-sm text-zinc-500 mb-4">
        SOL Balance Over Time ({points.length.toLocaleString()} data points)
      </div>
      <svg viewBox={`0 0 ${w} ${h}`} className="w-full h-[300px]" preserveAspectRatio="none">
        <defs>
          <linearGradient id="ag" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#7c3aed" stopOpacity="0.3" />
            <stop offset="100%" stopColor="#7c3aed" stopOpacity="0.02" />
          </linearGradient>
        </defs>
        {yTicks.map((yt, i) => (
          <line key={i} x1={pL} y1={yt.cy} x2={w - pR} y2={yt.cy} stroke="#1a1a22" />
        ))}
        <path d={area} fill="url(#ag)" />
        <path d={line} fill="none" stroke="#7c3aed" strokeWidth="1.5" />
        {yTicks.map((yt, i) => (
          <text key={i} x={pL - 8} y={yt.cy + 4} textAnchor="end" fill="#555" fontSize="10" fontFamily="inherit">
            {yt.val.toFixed(yt.val > 100 ? 0 : 2)}
          </text>
        ))}
        {xTicks.map((xt, i) => (
          <text key={i} x={xt.cx} y={h - pB + 20} textAnchor="middle" fill="#555" fontSize="10" fontFamily="inherit">
            {xt.label}
          </text>
        ))}
      </svg>
    </div>
  );
}

export function BalanceExplorer() {
  const [address, setAddress] = useState("");
  const [loading, setLoading] = useState(false);
  const [phases, setPhases] = useState<PhaseLog[]>([]);
  const [summary, setSummary] = useState<Summary | null>(null);
  const [points, setPoints] = useState<BalancePoint[]>([]);
  const [error, setError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!address.trim() || loading) return;

    setLoading(true);
    setPhases([]);
    setSummary(null);
    setPoints([]);
    setError(null);

    const es = new EventSource(
      `/api/balance/stream?address=${encodeURIComponent(address.trim())}`
    );
    abortRef.current = { abort: () => es.close() } as AbortController;

    es.addEventListener("phase", (e) => {
      try {
        setPhases((prev) => [...prev, JSON.parse(e.data)]);
      } catch {}
    });

    es.addEventListener("summary", (e) => {
      try {
        setSummary(JSON.parse(e.data));
      } catch {}
    });

    es.addEventListener("data", (e) => {
      try {
        setPoints(JSON.parse(e.data));
      } catch {}
    });

    es.addEventListener("error", () => {
      // EventSource built-in error = connection issue
      if (es.readyState === EventSource.CLOSED) {
        setError("Connection to backend lost");
        setLoading(false);
      }
    });

    es.addEventListener("server_error", (e) => {
      try {
        setError(JSON.parse(e.data).error || "Unknown error");
      } catch {}
      es.close();
      setLoading(false);
    });

    es.addEventListener("done", () => {
      es.close();
      setLoading(false);
    });
  };

  return (
    <>
      <form className="flex gap-3 mb-8" onSubmit={handleSubmit}>
        <input
          type="text"
          placeholder="Enter Solana wallet address..."
          value={address}
          onChange={(e) => setAddress(e.target.value)}
          disabled={loading}
          className="flex-1 px-4 py-3 bg-[#111118] border border-[#222] rounded-lg text-white font-mono text-sm outline-none focus:border-violet-600 placeholder:text-zinc-600 disabled:opacity-50"
        />
        <button
          type="submit"
          disabled={loading}
          className="px-7 py-3 bg-violet-600 text-white rounded-lg font-semibold text-sm hover:bg-violet-700 disabled:bg-zinc-700 disabled:cursor-not-allowed whitespace-nowrap"
        >
          {loading ? (
            <span className="flex items-center gap-2">
              Fetching
              <span className="inline-block w-2 h-2 bg-white rounded-full animate-pulse" />
            </span>
          ) : (
            "Analyze"
          )}
        </button>
      </form>

      {error && (
        <div className="bg-red-950/30 border border-red-500/50 rounded-lg p-4 text-red-400 text-sm mb-6">
          {error}
        </div>
      )}

      {/* Phase logs */}
      {phases.length > 0 && (
        <div className="mb-6 space-y-0">
          {phases.map((p, i) => (
            <div
              key={i}
              className="flex items-start gap-3 py-2 border-b border-[#1a1a22] text-sm animate-in fade-in slide-in-from-top-1 duration-300"
            >
              <span className="bg-[#1a1a2e] text-violet-400 px-2 py-0.5 rounded text-xs font-semibold min-w-[72px] text-center">
                {p.phase}
              </span>
              <span className="text-zinc-600 min-w-[70px] text-right">{formatMs(p.elapsed_ms)}</span>
              <span className="text-zinc-400 flex-1">{p.message}</span>
            </div>
          ))}
        </div>
      )}

      {/* Summary stats */}
      {summary && (
        <div className="grid grid-cols-2 md:grid-cols-4 gap-3 mb-8">
          {[
            { label: "Total Latency", value: formatMs(summary.fetch_duration_ms), accent: true },
            { label: "RPC Calls", value: summary.rpc_calls.toLocaleString() },
            { label: "Txs Fetched", value: summary.total_txs_fetched.toLocaleString() },
            { label: "Balance Points", value: summary.balance_points.toLocaleString() },
          ].map((s) => (
            <div key={s.label} className="bg-[#111118] border border-[#1a1a22] rounded-lg p-4">
              <div className="text-[10px] text-zinc-500 uppercase tracking-wider mb-1">{s.label}</div>
              <div className={`text-xl font-bold ${s.accent ? "text-violet-400" : "text-white"}`}>
                {s.value}
              </div>
            </div>
          ))}
        </div>
      )}

      {/* Chart */}
      {points.length > 0 && <BalanceChart points={points} />}

      {/* Table */}
      {points.length > 0 && (
        <div className="bg-[#111118] border border-[#1a1a22] rounded-lg overflow-hidden max-h-[400px] overflow-y-auto">
          <table className="w-full text-xs">
            <thead>
              <tr className="bg-[#0d0d14] sticky top-0">
                <th className="px-4 py-2.5 text-left text-zinc-500 font-semibold uppercase tracking-wider text-[10px]">
                  Date
                </th>
                <th className="px-4 py-2.5 text-left text-zinc-500 font-semibold uppercase tracking-wider text-[10px]">
                  SOL Balance
                </th>
                <th className="px-4 py-2.5 text-left text-zinc-500 font-semibold uppercase tracking-wider text-[10px]">
                  Change
                </th>
                <th className="px-4 py-2.5 text-left text-zinc-500 font-semibold uppercase tracking-wider text-[10px]">
                  Slot
                </th>
              </tr>
            </thead>
            <tbody>
              {points
                .slice(-200)
                .reverse()
                .map((p, i) => {
                  const change = (p.post_balance - p.pre_balance) / 1e9;
                  const date = new Date(p.block_time * 1000);
                  return (
                    <tr key={i} className="hover:bg-[#141420]">
                      <td className="px-4 py-2 border-b border-[#1a1a22] text-zinc-500">
                        {date.toLocaleDateString()} {date.toLocaleTimeString()}
                      </td>
                      <td className="px-4 py-2 border-b border-[#1a1a22] text-white">
                        {p.sol_balance.toFixed(4)} SOL
                      </td>
                      <td
                        className={`px-4 py-2 border-b border-[#1a1a22] ${change >= 0 ? "text-green-400" : "text-red-400"}`}
                      >
                        {change >= 0 ? "+" : ""}
                        {change.toFixed(4)}
                      </td>
                      <td className="px-4 py-2 border-b border-[#1a1a22] text-zinc-600">
                        {p.slot.toLocaleString()}
                      </td>
                    </tr>
                  );
                })}
            </tbody>
          </table>
          {points.length > 200 && (
            <div className="px-4 py-3 text-zinc-600 text-xs">
              Showing last 200 of {points.length.toLocaleString()} balance changes
            </div>
          )}
        </div>
      )}
    </>
  );
}
