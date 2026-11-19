import { beforeEach, describe, expect, it, vi } from "vitest";

const { exchange, revoke } = vi.hoisted(() => ({ exchange: vi.fn(), revoke: vi.fn() }));
vi.mock("@/lib/connect/deps", () => ({ connectDeps: () => ({}) }));
vi.mock("@/lib/connect/service", async (orig) => ({ ...(await orig<object>()), exchange, revoke }));

import { resetRateLimits } from "@/lib/connect/rate-limit";
import { POST as token } from "./token/route";
import { POST as revokeRoute } from "./revoke/route";

const req = (path: string, init: RequestInit & { ip?: string } = {}) =>
  new Request(`http://dash.test/api/connect/${path}`, {
    method: "POST",
    ...init,
    headers: { "x-forwarded-for": init.ip ?? "1.2.3.4", ...(init.headers as object) },
  });

beforeEach(() => {
  resetRateLimits();
  exchange.mockReset().mockResolvedValue({ status: 200, body: { api_key: "k" } });
  revoke.mockReset().mockResolvedValue(undefined);
});

describe("POST /api/connect/token", () => {
  it("passes the JSON body to the exchange and answers its status, uncached, without CORS headers", async () => {
    const res = await token(req("token", { body: JSON.stringify({ code: "c" }) }));
    expect(res.status).toBe(200);
    expect(exchange).toHaveBeenCalledWith({}, { code: "c" });
    expect(res.headers.get("cache-control")).toBe("no-store");
    expect([...res.headers.keys()].filter((h) => h.startsWith("access-control-"))).toEqual([]);
  });
  it("answers 400 invalid_request for a body that is not JSON or is huge", async () => {
    expect(await (await token(req("token", { body: "{nope" }))).json()).toEqual({ error: "invalid_request" });
    expect((await token(req("token", { body: JSON.stringify({ code: "x".repeat(5000) }) }))).status).toBe(400);
    expect(exchange).not.toHaveBeenCalled();
  });
  it("answers 429 after 30 requests a minute from one address, and only for it", async () => {
    for (let i = 0; i < 30; i++) expect((await token(req("token", { body: "{}" }))).status).toBe(200);
    const res = await token(req("token", { body: "{}" }));
    expect(res.status).toBe(429);
    expect(res.headers.get("retry-after")).toBe("60");
    expect((await token(req("token", { body: "{}", ip: "5.6.7.8" }))).status).toBe(200);
  });
});

describe("POST /api/connect/revoke", () => {
  it("revokes the bearer key and answers 204", async () => {
    const res = await revokeRoute(req("revoke", { headers: { authorization: "Bearer obl-abc" } }));
    expect(res.status).toBe(204);
    expect(revoke).toHaveBeenCalledWith({}, "obl-abc");
  });
  it("answers 204 without a key too, and revokes nothing", async () => {
    expect((await revokeRoute(req("revoke"))).status).toBe(204);
    expect(revoke).not.toHaveBeenCalled();
  });
  it("answers 429 over the limit", async () => {
    for (let i = 0; i < 30; i++) await revokeRoute(req("revoke"));
    expect((await revokeRoute(req("revoke"))).status).toBe(429);
  });
});
