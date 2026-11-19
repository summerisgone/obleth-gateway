import { NextResponse } from "next/server";
import { sentByAnotherSite } from "@/lib/auth/origin";
import { getSession } from "@/lib/auth/session";
import { verifyCsrf } from "@/lib/connect/csrf";
import { connectDeps } from "@/lib/connect/deps";
import { parseConnectRequest } from "@/lib/connect/request";
import { callbackUrl } from "@/lib/connect/redirect-uri";
import { approve, deny } from "@/lib/connect/service";
import { csrfFields } from "../fields";

export const dynamic = "force-dynamic";

const text = (message: string, status: number) =>
  new NextResponse(message, { status, headers: { "Content-Type": "text/plain; charset=utf-8", "Cache-Control": "no-store" } });
const go = (url: string) => NextResponse.redirect(url, 302);

export async function POST(request: Request) {
  if (sentByAnotherSite("POST", request.headers)) return text("This request came from another site.", 403);
  const session = await getSession();
  if (!session || session.status !== "active") return text("Sign in first.", 401);

  const form = await request.formData();
  const field = (k: string) => {
    const v = form.get(k);
    return typeof v === "string" ? v : undefined;
  };
  const req = parseConnectRequest({
    redirect_uri: field("redirect_uri"),
    state: field("state"),
    code_challenge: field("code_challenge"),
    code_challenge_method: "S256",
    app: field("app"),
    device: field("device"),
  });
  if (req.kind === "bad-redirect") return text("Invalid return address.", 400);
  if (req.kind === "invalid") return go(callbackUrl(req.redirectUri, { error: "invalid_request" }, req.state));
  const { params } = req;
  if (!verifyCsrf(field("csrf"), session.id, csrfFields(params))) return text("The form expired. Start again from the application.", 403);

  const user = { id: session.id, email: session.email };
  const deps = connectDeps();
  if (field("decision") === "deny") return go(await deny(deps, user, params));
  if (field("decision") !== "approve") return text("Unknown decision.", 400);

  const result = await approve(deps, user, {
    ...params,
    tenantId: field("tenant_id") ?? "",
    expiryDays: Number(field("expiry_days") ?? "0"),
  });
  if (!result.ok) return text(result.error === "tenant_not_allowed" ? "You cannot create keys in that tenant." : "Invalid request.", result.error === "tenant_not_allowed" ? 403 : 400);
  return go(result.redirectTo);
}
