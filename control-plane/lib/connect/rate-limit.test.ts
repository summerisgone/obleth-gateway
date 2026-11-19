import { beforeEach, describe, expect, it } from "vitest";
import { allow, clientAddress, resetRateLimits } from "./rate-limit";

beforeEach(resetRateLimits);

describe("allow", () => {
  it("allows 30 a minute per key, then refuses, then resets", () => {
    for (let i = 0; i < 30; i++) expect(allow("a", 1000)).toBe(true);
    expect(allow("a", 1000)).toBe(false);
    expect(allow("b", 1000)).toBe(true);
    expect(allow("a", 61_001)).toBe(true);
  });
});

describe("clientAddress", () => {
  const h = (o: Record<string, string>) => new Headers(o);
  it("reads the entry the trusted edge appended", () => {
    expect(clientAddress(h({ "x-forwarded-for": "6.6.6.6, 1.2.3.4" }), 1)).toBe("1.2.3.4");
    expect(clientAddress(h({ "x-forwarded-for": "6.6.6.6, 1.2.3.4, 10.0.0.1" }), 2)).toBe("1.2.3.4");
  });
  it("falls back to x-real-ip, then unknown", () => {
    expect(clientAddress(h({ "x-real-ip": "9.9.9.9" }), 1)).toBe("9.9.9.9");
    expect(clientAddress(h({}), 1)).toBe("unknown");
  });
});
