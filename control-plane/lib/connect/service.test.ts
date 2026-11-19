import { beforeEach, describe, expect, it, vi } from "vitest";
import { challengeFor } from "./pkce";
import {
  approve,
  bearerToken,
  deny,
  exchange,
  revoke,
  sha256Hex,
  sweepExpired,
  type ApproveInput,
  type ConnectDeps,
} from "./service";
import type { CodeRow, CodeStore, KeyRecord, KeyStore } from "./store";

const VERIFIER = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT = "http://127.0.0.1:5000/obleth/callback";
const PERSONAL = { id: "t-personal", name: "u-1", kind: "personal" as const };
const PROJECT = { id: "t-proj", name: "proj", kind: "standard" as const };
const IDENTITY = { issuer: "https://iss", subject: "sub1" };
const USER = { id: "u1", email: "a@x.io" };

// Same contract as the Postgres store: redeem decides and marks in one step.
function memoryCodes(): CodeStore & { rows: Map<string, CodeRow> } {
  const rows = new Map<string, CodeRow>();
  return {
    rows,
    async insert(c) {
      rows.set(c.codeHash, { ...c, usedAt: null });
    },
    async redeem(hash, now) {
      const r = rows.get(hash);
      if (!r || r.usedAt || r.expiresAt <= now) return null;
      r.usedAt = now;
      return { ...r };
    },
  };
}
function memoryKeys(): KeyStore & { rows: Map<string, KeyRecord> } {
  const rows = new Map<string, KeyRecord>();
  return {
    rows,
    async insert(k) {
      rows.set(k.keyId, k);
    },
    async findByHash(h) {
      return [...rows.values()].find((k) => k.keyHash === h) ?? null;
    },
    async remove(id) {
      rows.delete(id);
    },
    async expired(now) {
      return [...rows.values()].filter((k) => k.expiresAt && k.expiresAt <= now);
    },
  };
}

function setup(over: Partial<ConnectDeps> = {}) {
  let clock = new Date("2026-11-19T10:00:00Z");
  let seq = 0;
  const owned = new Map<string, { id: string; name: string; tenantId: string }>();
  const audit = vi.fn(async () => {});
  const deleteKey = vi.fn(async (id: string) => {
    owned.delete(id);
  });
  const deps: ConnectDeps = {
    codes: memoryCodes(),
    keys: memoryKeys(),
    now: () => clock,
    identity: async () => IDENTITY,
    activeUser: async () => USER,
    tenants: async () => [PERSONAL, PROJECT],
    listOwnedKeys: async (tenantId) => [...owned.values()].filter((k) => k.tenantId === tenantId),
    createOwnedKey: async (tenantId, _i, name) => {
      const id = `key-${++seq}`;
      owned.set(id, { id, name, tenantId });
      return { id, secret: `obl-secret-${seq}` };
    },
    deleteKey,
    audit,
    ...over,
  };
  return { deps, owned, audit, deleteKey, tick: (ms: number) => (clock = new Date(clock.getTime() + ms)) };
}

const input = (over: Partial<ApproveInput> = {}): ApproveInput => ({
  redirectUri: REDIRECT,
  state: "st",
  codeChallenge: CHALLENGE,
  app: "DSH desktop",
  device: "laptop",
  tenantId: PERSONAL.id,
  expiryDays: 0,
  ...over,
});

async function issueCode(deps: ConnectDeps, over: Partial<ApproveInput> = {}): Promise<string> {
  const r = await approve(deps, USER, input(over));
  if (!r.ok) throw new Error(r.error);
  const url = new URL(r.redirectTo);
  expect(url.searchParams.get("state")).toBe(over.state ?? "st");
  return url.searchParams.get("code")!;
}
const body = (code: string, over: Record<string, unknown> = {}) => ({ code, code_verifier: VERIFIER, redirect_uri: REDIRECT, ...over });

let ctx: ReturnType<typeof setup>;
beforeEach(() => {
  ctx = setup();
});

describe("approve", () => {
  it("answers a redirect with a code and the state, and stores only the code hash", async () => {
    const code = await issueCode(ctx.deps);
    const codes = (ctx.deps.codes as ReturnType<typeof memoryCodes>).rows;
    expect([...codes.keys()]).toEqual([sha256Hex(code)]);
    expect(JSON.stringify([...codes.values()])).not.toContain(code);
    expect(ctx.audit).toHaveBeenCalledWith(USER.email, "connect_approve", "dsh-laptop", expect.objectContaining({ app: "DSH desktop", device: "laptop" }));
  });
  it("rejects a tenant the user may not create keys in", async () => {
    const r = await approve(ctx.deps, USER, input({ tenantId: "t-other" }));
    expect(r).toEqual({ ok: false, error: "tenant_not_allowed" });
    expect((ctx.deps.codes as ReturnType<typeof memoryCodes>).rows.size).toBe(0);
  });
  it("rejects a user without an identity, a bad expiry and a bad redirect", async () => {
    expect(await approve(setup({ identity: async () => null }).deps, USER, input())).toEqual({ ok: false, error: "tenant_not_allowed" });
    expect(await approve(ctx.deps, USER, input({ expiryDays: 7 }))).toEqual({ ok: false, error: "invalid_request" });
    expect(await approve(ctx.deps, USER, input({ redirectUri: "https://evil.example/obleth/callback" }))).toEqual({ ok: false, error: "invalid_request" });
  });
  it("accepts a project tenant", async () => {
    expect((await approve(ctx.deps, USER, input({ tenantId: PROJECT.id }))).ok).toBe(true);
  });
});

describe("deny", () => {
  it("redirects access_denied with the state and creates no code", async () => {
    const url = await deny(ctx.deps, USER, { redirectUri: REDIRECT, state: "st", codeChallenge: CHALLENGE, app: "a", device: "d" });
    expect(url).toBe(`${REDIRECT}?error=access_denied&state=st`);
    expect((ctx.deps.codes as ReturnType<typeof memoryCodes>).rows.size).toBe(0);
    expect(ctx.audit).toHaveBeenCalledWith(USER.email, "connect_deny", "dsh-d", expect.any(Object));
  });
});

describe("exchange", () => {
  it("mints a key named dsh-<device> and answers the contract", async () => {
    const code = await issueCode(ctx.deps);
    const r = await exchange(ctx.deps, body(code));
    expect(r).toEqual({
      status: 200,
      body: { api_key: "obl-secret-1", key_id: "key-1", tenant: { id: PERSONAL.id, name: PERSONAL.name }, expires_at: null },
    });
    expect([...ctx.owned.values()][0]!.name).toBe("dsh-laptop");
    expect(ctx.audit).toHaveBeenCalledWith(USER.email, "connect_exchange", "key-1", expect.any(Object));
  });
  it("sets expires_at from the chosen days", async () => {
    const code = await issueCode(ctx.deps, { expiryDays: 30 });
    const r = await exchange(ctx.deps, body(code));
    expect(r.status === 200 && r.body.expires_at).toBe("2026-12-19T10:00:00.000Z");
  });
  it("never leaks the code, verifier or key into audit entries", async () => {
    const code = await issueCode(ctx.deps);
    await exchange(ctx.deps, body(code));
    const logged = JSON.stringify(ctx.audit.mock.calls);
    for (const secret of [code, VERIFIER, "obl-secret-1"]) expect(logged).not.toContain(secret);
  });
  it("replaces an earlier key of the same name in the tenant, not other keys", async () => {
    ctx.owned.set("old", { id: "old", name: "dsh-laptop", tenantId: PERSONAL.id });
    ctx.owned.set("other-name", { id: "other-name", name: "dsh-phone", tenantId: PERSONAL.id });
    ctx.owned.set("other-tenant", { id: "other-tenant", name: "dsh-laptop", tenantId: PROJECT.id });
    await exchange(ctx.deps, body(await issueCode(ctx.deps)));
    expect(ctx.deleteKey).toHaveBeenCalledTimes(1);
    expect(ctx.deleteKey).toHaveBeenCalledWith("old", USER.email);
    expect([...ctx.owned.keys()].sort()).toEqual(["key-1", "other-name", "other-tenant"]);
  });

  it("is single use: a replay fails", async () => {
    const code = await issueCode(ctx.deps);
    expect((await exchange(ctx.deps, body(code))).status).toBe(200);
    expect(await exchange(ctx.deps, body(code))).toEqual({ status: 400, body: { error: "invalid_grant" } });
  });
  it("is single use under two concurrent exchanges: exactly one 200", async () => {
    const code = await issueCode(ctx.deps);
    const results = await Promise.all([exchange(ctx.deps, body(code)), exchange(ctx.deps, body(code))]);
    expect(results.map((r) => r.status).sort()).toEqual([200, 400]);
    expect(ctx.owned.size).toBe(1);
  });
  it("expires after 60 s: ok at 59 s, invalid_grant at 61 s", async () => {
    const code = await issueCode(ctx.deps);
    ctx.tick(59_000);
    expect((await exchange(ctx.deps, body(code))).status).toBe(200);
    const late = await issueCode(ctx.deps);
    ctx.tick(61_000);
    expect(await exchange(ctx.deps, body(late))).toEqual({ status: 400, body: { error: "invalid_grant" } });
  });
  it("answers invalid_grant for another redirect_uri, even the right code", async () => {
    const code = await issueCode(ctx.deps);
    expect(await exchange(ctx.deps, body(code, { redirect_uri: "http://127.0.0.1:5001/obleth/callback" }))).toEqual({
      status: 400,
      body: { error: "invalid_grant" },
    });
    expect(ctx.owned.size).toBe(0);
  });
  it("answers invalid_grant for the wrong verifier, and burns the code", async () => {
    const code = await issueCode(ctx.deps);
    const wrong = "x".repeat(43);
    expect(challengeFor(wrong)).not.toBe(CHALLENGE);
    expect((await exchange(ctx.deps, body(code, { code_verifier: wrong }))).body).toEqual({ error: "invalid_grant" });
    expect((await exchange(ctx.deps, body(code))).body).toEqual({ error: "invalid_grant" });
  });
  it("answers invalid_grant for an unknown code", async () => {
    expect((await exchange(ctx.deps, body("nope"))).body).toEqual({ error: "invalid_grant" });
  });
  it.each([
    ["no body", undefined],
    ["no code", { code_verifier: VERIFIER, redirect_uri: REDIRECT }],
    ["no verifier", { code: "c", redirect_uri: REDIRECT }],
    ["short verifier", { code: "c", code_verifier: "short", redirect_uri: REDIRECT }],
    ["no redirect_uri", { code: "c", code_verifier: VERIFIER }],
  ])("answers invalid_request for %s", async (_n, b) => {
    expect(await exchange(ctx.deps, b)).toEqual({ status: 400, body: { error: "invalid_request" } });
  });
  it("answers invalid_grant when the user lost the tenant or the account since approval", async () => {
    const code = await issueCode(ctx.deps);
    const lost = setup({ codes: ctx.deps.codes, tenants: async () => [PROJECT] });
    expect((await exchange(lost.deps, body(code))).body).toEqual({ error: "invalid_grant" });
    const code2 = await issueCode(ctx.deps);
    const gone = setup({ codes: ctx.deps.codes, activeUser: async () => null });
    expect((await exchange(gone.deps, body(code2))).body).toEqual({ error: "invalid_grant" });
  });
  it("answers server_error when the gateway refuses to mint", async () => {
    const failing = setup({ codes: ctx.deps.codes, createOwnedKey: async () => { throw new Error("boom with obl-secret"); } });
    const code = await issueCode(failing.deps);
    expect(await exchange(failing.deps, body(code))).toEqual({ status: 500, body: { error: "server_error" } });
  });
});

describe("revoke and expiry", () => {
  it("deletes the key behind a bearer secret, only that key", async () => {
    const a = await exchange(ctx.deps, body(await issueCode(ctx.deps)));
    const b = await exchange(ctx.deps, body(await issueCode(ctx.deps, { device: "desk" })));
    if (a.status !== 200 || b.status !== 200) throw new Error("setup");
    await revoke(ctx.deps, a.body.api_key);
    expect([...ctx.owned.keys()]).toEqual([b.body.key_id]);
    expect(ctx.audit).toHaveBeenCalledWith(USER.email, "connect_revoke", a.body.key_id, expect.any(Object));
  });
  it("is a quiet no-op for an unknown or already deleted key", async () => {
    const r = await exchange(ctx.deps, body(await issueCode(ctx.deps)));
    if (r.status !== 200) throw new Error("setup");
    await revoke(ctx.deps, r.body.api_key);
    ctx.deleteKey.mockClear();
    await revoke(ctx.deps, r.body.api_key);
    await revoke(ctx.deps, "never-issued");
    expect(ctx.deleteKey).not.toHaveBeenCalled();
  });
  it("sweeps keys past their expiry", async () => {
    const r = await exchange(ctx.deps, body(await issueCode(ctx.deps, { expiryDays: 30 })));
    const forever = await exchange(ctx.deps, body(await issueCode(ctx.deps, { device: "desk" })));
    if (r.status !== 200 || forever.status !== 200) throw new Error("setup");
    expect(await sweepExpired(ctx.deps)).toBe(0);
    ctx.tick(30 * 86_400_000 + 1);
    expect(await sweepExpired(ctx.deps)).toBe(1);
    expect([...ctx.owned.keys()]).toEqual([forever.body.key_id]);
  });
  it("reads a bearer header", () => {
    expect(bearerToken("Bearer abc")).toBe("abc");
    expect(bearerToken("bearer abc")).toBe("abc");
    expect(bearerToken("Basic abc")).toBeNull();
    expect(bearerToken(null)).toBeNull();
  });
});
