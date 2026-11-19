// @vitest-environment node
//
// Full loop against a running stack: control plane, gateway (admin and proxy)
// and their Postgres. Skipped unless CONNECT_E2E=1. Needed:
//   CONNECT_E2E_DASH_URL      control plane, e.g. http://localhost:3000
//   CONNECT_E2E_GATEWAY_URL   gateway proxy, e.g. http://localhost:8080
//   CONNECT_E2E_ADMIN_URL     gateway admin API, e.g. http://localhost:9180
//   OBLETH_ADMIN_TOKEN        its admin token
//   DATABASE_URL              the control plane's database
//   BETTER_AUTH_SECRET        the control plane's auth secret (to sign a session cookie)
//   CONNECT_E2E_ISSUER        the issuer of the control plane's OIDC provider
//                             (the discovery URL without /.well-known/openid-configuration)
//   CONNECT_E2E_PROVIDER_ID   that provider's providerId
// The test user is created here: an active user with an OIDC account, a personal
// tenant from the gateway and a session. The "sign-in" is the session cookie.
import { createHash, createHmac, randomBytes, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { Pool } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

const e = process.env;
const enabled = e.CONNECT_E2E === "1";
const dash = e.CONNECT_E2E_DASH_URL ?? "http://localhost:3000";
const gateway = e.CONNECT_E2E_GATEWAY_URL ?? "http://localhost:8080";
const admin = e.CONNECT_E2E_ADMIN_URL ?? "http://localhost:9180";

const b64url = (b: Buffer) => b.toString("base64url");

(enabled ? describe : describe.skip)("connect: full loop", () => {
  let pool: Pool;
  let cookie = "";
  let userId = "";
  let tenantId = "";
  let receiver: Server;
  let callbacks: URL[] = [];
  let port = 0;

  const adminApi = (path: string, init: RequestInit = {}) =>
    fetch(`${admin}/api/v1${path}`, {
      ...init,
      headers: { Authorization: `Bearer ${e.OBLETH_ADMIN_TOKEN}`, "Content-Type": "application/json" },
    });

  beforeAll(async () => {
    pool = new Pool({ connectionString: e.DATABASE_URL });
    const sub = `e2e-${randomBytes(4).toString("hex")}`;
    userId = randomUUID();
    const email = `${sub}@example.test`;
    const idRes = await adminApi("/identities", { method: "POST", body: JSON.stringify({ issuer: e.CONNECT_E2E_ISSUER, subject: sub, email }) });
    expect(idRes.status).toBe(200);
    tenantId = (await idRes.json()).tenant_id;
    await pool.query(`insert into "user" (id, name, email, "emailVerified", role, status, "tenantId") values ($1,$2,$3,true,'user','active',$4)`, [userId, sub, email, tenantId]);
    await pool.query(`insert into "account" (id, "accountId", "providerId", "userId") values ($1,$2,$3,$4)`, [randomUUID(), sub, e.CONNECT_E2E_PROVIDER_ID, userId]);
    const token = b64url(randomBytes(24));
    await pool.query(`insert into "session" (id, "expiresAt", token, "userId") values ($1, now() + interval '1 hour', $2, $3)`, [randomUUID(), token, userId]);
    const sig = createHmac("sha256", e.BETTER_AUTH_SECRET!).update(token).digest("base64");
    cookie = `better-auth.session_token=${encodeURIComponent(`${token}.${sig}`)}`;

    receiver = createServer((req, res) => {
      callbacks.push(new URL(req.url ?? "/", `http://127.0.0.1:${port}`));
      res.end("you can close this window");
    });
    await new Promise<void>((r) => receiver.listen(0, "127.0.0.1", r));
    port = (receiver.address() as AddressInfo).port;
  });
  afterAll(async () => {
    receiver?.close();
    if (userId) await pool?.query(`delete from "user" where id = $1`, [userId]);
    await pool?.end();
  });

  const pkce = () => {
    const verifier = b64url(randomBytes(48));
    return { verifier, challenge: b64url(createHash("sha256").update(verifier).digest()) };
  };
  const redirectUri = () => `http://127.0.0.1:${port}/obleth/callback`;
  const connectUrl = (q: Record<string, string>) => `${dash}/connect?${new URLSearchParams(q)}`;
  const hidden = (html: string, name: string) => new RegExp(`name="${name}"[^>]*value="([^"]*)"|value="([^"]*)"[^>]*name="${name}"`).exec(html)?.slice(1).find(Boolean);
  const decode = (s: string) => s.replace(/&amp;/g, "&").replace(/&quot;/g, '"').replace(/&#x27;/g, "'");

  async function approveInBrowser(c: { challenge: string }, extra: Record<string, string> = {}, expiryDays = "0") {
    const q = { redirect_uri: redirectUri(), state: "xyz-state", code_challenge: c.challenge, code_challenge_method: "S256", app: "DSH desktop", device: "e2e-laptop" };
    const page = await fetch(connectUrl(q), { headers: { cookie }, redirect: "manual" });
    expect(page.status).toBe(200);
    expect(page.headers.get("x-frame-options")).toBe("DENY");
    expect(page.headers.get("content-security-policy")).toContain("frame-ancestors 'none'");
    expect(page.headers.get("content-security-policy")).toContain(`form-action 'self' http://127.0.0.1:${port}`);
    const html = await page.text();
    expect(html).toContain("DSH desktop");
    const form = new URLSearchParams({
      csrf: decode(hidden(html, "csrf")!),
      redirect_uri: decode(hidden(html, "redirect_uri")!),
      state: decode(hidden(html, "state")!),
      code_challenge: decode(hidden(html, "code_challenge")!),
      app: decode(hidden(html, "app")!),
      device: decode(hidden(html, "device")!),
      tenant_id: tenantId,
      expiry_days: expiryDays,
      decision: "approve",
      ...extra,
    });
    const res = await fetch(`${dash}/connect/decision`, {
      method: "POST",
      headers: { cookie, origin: dash, "content-type": "application/x-www-form-urlencoded" },
      body: form,
      redirect: "manual",
    });
    return { res, form };
  }

  it("shows an error page, not a redirect, for a bad redirect_uri", async () => {
    const q = { redirect_uri: "http://127.0.0.1:1/obleth/callback?x=1", state: "s", code_challenge: pkce().challenge, code_challenge_method: "S256" };
    const res = await fetch(connectUrl(q), { headers: { cookie }, redirect: "manual" });
    expect(res.status).toBe(200);
    expect(res.headers.get("location")).toBeNull();
    expect(await res.text()).toContain("invalid return address");
  });

  it("redirects invalid_request for a missing state, and sends a signed-out visitor to sign in", async () => {
    const q = { redirect_uri: redirectUri(), code_challenge: pkce().challenge, code_challenge_method: "S256" };
    const res = await fetch(connectUrl(q), { headers: { cookie }, redirect: "manual" });
    expect(res.status).toBeGreaterThanOrEqual(300);
    expect(res.headers.get("location")).toBe(`${redirectUri()}?error=invalid_request`);

    const ok = { ...q, state: "s" };
    const out = await fetch(connectUrl(ok), { redirect: "manual" });
    expect(out.headers.get("location")).toContain("/login?next=%2Fconnect%3F");
  });

  it("deny redirects access_denied with the state", async () => {
    const c = pkce();
    const { form } = await approveInBrowser(c).then(async (r) => r); // consumes one approve; separate deny below
    form.set("decision", "deny");
    const res = await fetch(`${dash}/connect/decision`, { method: "POST", headers: { cookie, origin: dash, "content-type": "application/x-www-form-urlencoded" }, body: form, redirect: "manual" });
    expect(res.headers.get("location")).toBe(`${redirectUri()}?error=access_denied&state=xyz-state`);
  });

  it("refuses a post from another site and a tenant the user is not a member of", async () => {
    const c = pkce();
    const { form } = await approveInBrowser(c);
    const cross = await fetch(`${dash}/connect/decision`, { method: "POST", headers: { cookie, origin: "http://evil.example", "content-type": "application/x-www-form-urlencoded" }, body: form, redirect: "manual" });
    expect(cross.status).toBe(403);
    form.set("tenant_id", randomUUID());
    const other = await fetch(`${dash}/connect/decision`, { method: "POST", headers: { cookie, origin: dash, "content-type": "application/x-www-form-urlencoded" }, body: form, redirect: "manual" });
    expect(other.status).toBe(403);
    form.set("tenant_id", tenantId);
    form.set("csrf", "1.forged");
    expect((await fetch(`${dash}/connect/decision`, { method: "POST", headers: { cookie, origin: dash, "content-type": "application/x-www-form-urlencoded" }, body: form, redirect: "manual" })).status).toBe(403);
  });

  it("approve -> callback -> token -> key works on the gateway -> revoke -> 401", async () => {
    const c = pkce();
    callbacks = [];
    const { res } = await approveInBrowser(c);
    expect(res.status).toBe(302);
    const location = res.headers.get("location")!;
    expect(location.startsWith(`${redirectUri()}?code=`)).toBe(true);
    await fetch(location); // the browser follows the redirect to the app's loopback listener
    expect(callbacks).toHaveLength(1);
    const code = callbacks[0]!.searchParams.get("code")!;
    expect(callbacks[0]!.searchParams.get("state")).toBe("xyz-state");

    const exchange = (over: Record<string, string> = {}) =>
      fetch(`${dash}/api/connect/token`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ code, code_verifier: c.verifier, redirect_uri: redirectUri(), ...over }),
      });

    // A wrong redirect_uri burns nothing it should not: use a second code for it.
    const res1 = await exchange();
    expect(res1.status).toBe(200);
    const out = await res1.json();
    expect(out.api_key).toMatch(/\S{20,}/);
    expect(out.tenant.id).toBe(tenantId);
    expect(out.expires_at).toBeNull();
    expect((await exchange()).status).toBe(400); // replay

    const models = () => fetch(`${gateway}/v1/models`, { headers: { Authorization: `Bearer ${out.api_key}` } });
    expect((await models()).status).toBe(200);

    // The same device approving again replaces the key.
    const c2 = pkce();
    const second = await approveInBrowser(c2);
    const code2 = new URL(second.res.headers.get("location")!).searchParams.get("code")!;
    const res2 = await fetch(`${dash}/api/connect/token`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code: code2, code_verifier: c2.verifier, redirect_uri: redirectUri() }),
    });
    const out2 = await res2.json();
    expect(out2.key_id).not.toBe(out.key_id);
    expect((await models()).status).toBe(401);
    const m2 = () => fetch(`${gateway}/v1/models`, { headers: { Authorization: `Bearer ${out2.api_key}` } });
    expect((await m2()).status).toBe(200);

    const revoke = await fetch(`${dash}/api/connect/revoke`, { method: "POST", headers: { Authorization: `Bearer ${out2.api_key}` } });
    expect(revoke.status).toBe(204);
    expect((await m2()).status).toBe(401);
    expect((await fetch(`${dash}/api/connect/revoke`, { method: "POST", headers: { Authorization: `Bearer ${out2.api_key}` } })).status).toBe(204);
    expect((await fetch(`${dash}/api/connect/revoke`, { method: "POST" })).status).toBe(204);

    const audit = await (await adminApi("/audit?limit=50&action=connect_approve,connect_exchange,connect_revoke,connect_deny")).json();
    expect(audit.map((a: { action: string }) => a.action)).toEqual(expect.arrayContaining(["connect_approve", "connect_exchange", "connect_revoke", "connect_deny"]));
    const logged = JSON.stringify(audit);
    for (const secret of [code, c.verifier, out.api_key, out2.api_key]) expect(logged).not.toContain(secret);
  });

  it("token answers invalid_grant for a wrong redirect_uri", async () => {
    const c = pkce();
    const { res } = await approveInBrowser(c);
    const code = new URL(res.headers.get("location")!).searchParams.get("code")!;
    const bad = await fetch(`${dash}/api/connect/token`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code, code_verifier: c.verifier, redirect_uri: `http://127.0.0.1:${port + 1}/obleth/callback` }),
    });
    expect(bad.status).toBe(400);
    expect(await bad.json()).toEqual({ error: "invalid_grant" });
  });
});
