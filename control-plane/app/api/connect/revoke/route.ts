import { NextResponse } from "next/server";
import { connectDeps } from "@/lib/connect/deps";
import { allow, clientAddress } from "@/lib/connect/rate-limit";
import { bearerToken, revoke } from "@/lib/connect/service";

export const dynamic = "force-dynamic";

export async function POST(request: Request) {
  if (!allow(`revoke:${clientAddress(request.headers)}`)) {
    return NextResponse.json({ error: "rate_limited" }, { status: 429, headers: { "Retry-After": "60" } });
  }
  const key = bearerToken(request.headers.get("authorization"));
  if (key) {
    try {
      await revoke(connectDeps(), key);
    } catch {
      return NextResponse.json({ error: "server_error" }, { status: 500 });
    }
  }
  // Unknown, already deleted and missing keys all look the same.
  return new NextResponse(null, { status: 204 });
}
