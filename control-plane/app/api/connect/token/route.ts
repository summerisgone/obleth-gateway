import { NextResponse } from "next/server";
import { connectDeps } from "@/lib/connect/deps";
import { allow, clientAddress } from "@/lib/connect/rate-limit";
import { exchange } from "@/lib/connect/service";

export const dynamic = "force-dynamic";

// Server to server: the desktop host calls this with no session cookie, and no
// CORS header is sent, so a browser page cannot read the answer.
const NO_STORE = { "Cache-Control": "no-store" };

export async function POST(request: Request) {
  if (!allow(`token:${clientAddress(request.headers)}`)) {
    return NextResponse.json({ error: "rate_limited" }, { status: 429, headers: { ...NO_STORE, "Retry-After": "60" } });
  }
  let body: unknown;
  try {
    const text = await request.text();
    if (text.length > 4096) throw new Error("too large");
    body = JSON.parse(text);
  } catch {
    return NextResponse.json({ error: "invalid_request" }, { status: 400, headers: NO_STORE });
  }
  const result = await exchange(connectDeps(), body);
  return NextResponse.json(result.body, { status: result.status, headers: NO_STORE });
}
