import { NextRequest, NextResponse } from "next/server";
import { guardAdmin } from "@/lib/auth/guard";
import { gatewayMeshGet } from "@/lib/charo/gateway";
import { apiErrorMessage } from "@/lib/charo/errors";

export const runtime = "nodejs";
export const dynamic = "force-dynamic";

// One mesh job's status and progress, polled by the playground while it runs.
export async function GET(_req: NextRequest, { params }: { params: Promise<{ id: string }> }) {
  const denied = await guardAdmin();
  if (denied) return denied;
  const { id } = await params;
  try {
    const res = await gatewayMeshGet(id, false);
    const json = await res.json().catch(() => null);
    if (!res.ok) {
      return NextResponse.json({ error: apiErrorMessage(json, "job lookup failed") }, { status: res.status });
    }
    return NextResponse.json({ job: json });
  } catch (e) {
    return NextResponse.json({ error: String(e) }, { status: 502 });
  }
}
