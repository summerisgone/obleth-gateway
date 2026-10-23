import { NextRequest, NextResponse } from "next/server";
import { obleth } from "@/lib/obleth";
import type { UsageSpeedParams } from "@/lib/obleth";
import { guardAdmin } from "@/lib/auth/guard";

export async function GET(req: NextRequest) {
  const denied = await guardAdmin();
  if (denied) return denied;
  try {
    const p = req.nextUrl.searchParams;
    const untilMs = Number(p.get("until_ms") ?? Date.now());
    const rows = await obleth.usageSpeed({
      sinceMs: Number(p.get("since_ms") ?? untilMs - 86_400_000),
      untilMs,
      bucket: (p.get("bucket") as UsageSpeedParams["bucket"] | null) ?? undefined,
      tz: p.get("tz") ?? undefined,
      tenantId: p.get("tenant_id") ?? undefined,
      keyId: p.get("key_id") ?? undefined,
      model: p.get("model") ?? undefined,
    });
    return NextResponse.json(rows);
  } catch (e) {
    return NextResponse.json({ error: String(e) }, { status: 502 });
  }
}
