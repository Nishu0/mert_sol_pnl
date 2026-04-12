import { NextRequest } from "next/server";

const RUST_BACKEND = process.env.RUST_BACKEND_URL || "http://localhost:3001";

export async function GET(req: NextRequest) {
  const address = req.nextUrl.searchParams.get("address");
  if (!address) {
    return new Response(JSON.stringify({ error: "address is required" }), {
      status: 400,
      headers: { "Content-Type": "application/json" },
    });
  }

  const url = `${RUST_BACKEND}/api/balance/stream?address=${encodeURIComponent(address)}`;

  const upstream = await fetch(url, {
    headers: { Accept: "text/event-stream" },
  });

  if (!upstream.ok || !upstream.body) {
    return new Response(
      JSON.stringify({ error: `Backend error: ${upstream.status}` }),
      { status: 502, headers: { "Content-Type": "application/json" } }
    );
  }

  return new Response(upstream.body, {
    headers: {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
    },
  });
}
