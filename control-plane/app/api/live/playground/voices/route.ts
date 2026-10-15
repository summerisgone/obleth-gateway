import { NextRequest, NextResponse } from "next/server";
import { guardAdmin } from "@/lib/auth/guard";
import { gatewayVoices } from "@/lib/charo/gateway";
import { apiErrorMessage } from "@/lib/charo/errors";

export const runtime = "nodejs";
export const dynamic = "force-dynamic";

// The voices a speech model's server offers, for the playground's voice
// picker. Calls the gateway's /v1/audio/voices as the reserved internal
// tenant. Many servers cannot list their voices; the gateway then answers
// 404 (or 405/501), which is reported as `unsupported` rather than an error so
// the page falls back to a free-text voice field.
const UNSUPPORTED = new Set([404, 405, 501]);
const MAX_VOICES = 500;

/**
 * Voice names from `{voices, uploaded_voices}`. Entries are usually strings;
 * some servers send objects, so a `name`, `voice_id` or `id` is accepted too.
 */
function voiceNames(json: unknown): string[] {
  const body = json as { voices?: unknown; uploaded_voices?: unknown } | null;
  const out: string[] = [];
  for (const list of [body?.voices, body?.uploaded_voices]) {
    if (!Array.isArray(list)) continue;
    for (const entry of list) {
      const e = entry as { name?: unknown; voice_id?: unknown; id?: unknown } | null;
      const name = typeof entry === "string" ? entry : [e?.name, e?.voice_id, e?.id].find((v) => typeof v === "string");
      if (typeof name === "string" && name.trim() && !out.includes(name.trim())) out.push(name.trim());
    }
  }
  return out.slice(0, MAX_VOICES);
}

export async function GET(req: NextRequest) {
  const denied = await guardAdmin();
  if (denied) return denied;

  const model = req.nextUrl.searchParams.get("model")?.trim() ?? "";
  if (!model || model.length > 200) {
    return NextResponse.json({ error: "Invalid voices request." }, { status: 400 });
  }
  try {
    const upstream = await gatewayVoices(model, req.signal);
    if (UNSUPPORTED.has(upstream.status)) {
      return NextResponse.json({ voices: [], unsupported: true });
    }
    const json = await upstream.json().catch(() => null);
    if (!upstream.ok) {
      return NextResponse.json(
        { error: apiErrorMessage(json, `voices unavailable (HTTP ${upstream.status})`) },
        { status: upstream.status },
      );
    }
    return NextResponse.json({ voices: voiceNames(json) });
  } catch (e) {
    return NextResponse.json({ error: String(e) }, { status: 502 });
  }
}
