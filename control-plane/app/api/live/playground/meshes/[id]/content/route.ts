import { NextRequest, NextResponse } from "next/server";
import { guardAdmin } from "@/lib/auth/guard";
import { gatewayMeshGet } from "@/lib/charo/gateway";
import { apiErrorMessage } from "@/lib/charo/errors";

export const runtime = "nodejs";
export const dynamic = "force-dynamic";

// A finished mesh (binary glTF), streamed through untouched: tens of MB at
// high detail, so it is never buffered here.
export async function GET(_req: NextRequest, { params }: { params: Promise<{ id: string }> }) {
  const denied = await guardAdmin();
  if (denied) return denied;
  const { id } = await params;
  try {
    const res = await gatewayMeshGet(id, true);
    if (!res.ok || !res.body) {
      const json = await res.json().catch(() => null);
      return NextResponse.json({ error: apiErrorMessage(json, "download failed") }, { status: res.status });
    }
    const headers = new Headers({ "Content-Type": res.headers.get("content-type") ?? "model/gltf-binary" });
    const length = res.headers.get("content-length");
    if (length) headers.set("Content-Length", length);
    return new Response(res.body, { status: 200, headers });
  } catch (e) {
    return NextResponse.json({ error: String(e) }, { status: 502 });
  }
}
