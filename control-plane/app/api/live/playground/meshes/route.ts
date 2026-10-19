import { NextRequest, NextResponse } from "next/server";
import { guardAdmin } from "@/lib/auth/guard";
import { gatewayMeshCreate } from "@/lib/charo/gateway";
import { apiErrorMessage } from "@/lib/charo/errors";

export const runtime = "nodejs";
export const dynamic = "force-dynamic";

// The playground's 3D mesh relay. Starts a job on the gateway's normal
// /v1/meshes data-plane endpoint as the reserved internal tenant, so
// admission, billing, and telemetry behave exactly as for a real client. The
// page then polls ./[id] and downloads ./[id]/content.
//
// Only the fields mesh backends commonly take are forwarded; anything else in
// the form is dropped rather than passed through.
const MAX_IMAGE_BYTES = 20 * 1024 * 1024;
const IMAGE_TYPES = new Set(["image/png", "image/jpeg", "image/webp"]);
const SETTINGS: Record<string, { min: number; max: number }> = {
  resolution: { min: 256, max: 2048 },
  faces: { min: 1_000, max: 2_000_000 },
  texture_size: { min: 256, max: 8192 },
  seed: { min: 0, max: 4_294_967_295 },
};

export async function POST(req: NextRequest) {
  const denied = await guardAdmin();
  if (denied) return denied;

  const form = await req.formData().catch(() => null);
  const model = form?.get("model");
  const image = form?.get("image");
  if (!form || typeof model !== "string" || !model.trim() || model.length > 200 || !(image instanceof File)) {
    return NextResponse.json({ error: "Choose a model and an image." }, { status: 400 });
  }
  if (!IMAGE_TYPES.has(image.type) || image.size > MAX_IMAGE_BYTES) {
    return NextResponse.json({ error: "The image must be a PNG, JPEG or WebP of at most 20 MB." }, { status: 400 });
  }

  const upstream = new FormData();
  upstream.set("model", model);
  upstream.set("image", image, image.name || "image.png");
  for (const [name, range] of Object.entries(SETTINGS)) {
    const raw = form.get(name);
    if (typeof raw !== "string" || !raw.trim()) continue;
    const n = Number(raw);
    if (!Number.isInteger(n) || n < range.min || n > range.max) {
      return NextResponse.json({ error: `Invalid ${name.replace("_", " ")}.` }, { status: 400 });
    }
    upstream.set(name, String(n));
  }

  try {
    const res = await gatewayMeshCreate(upstream);
    const json = await res.json().catch(() => null);
    if (!res.ok) {
      return NextResponse.json({ error: apiErrorMessage(json, "the mesh job could not be started") }, { status: res.status });
    }
    return NextResponse.json({ job: json, requestId: res.headers.get("x-obleth-request-id") });
  } catch (e) {
    return NextResponse.json({ error: String(e) }, { status: 502 });
  }
}
