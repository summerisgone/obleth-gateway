import { describe, expect, it } from "vitest";
import { CSRF_TTL_MS, signCsrf, verifyCsrf } from "./csrf";

const KEY = "k".repeat(40);
const f = ["http://localhost:1/obleth/callback", "s", "c", "app", "dev"];

describe("csrf", () => {
  it("verifies for the same user and fields", () => {
    expect(verifyCsrf(signCsrf("u1", f, 1000, KEY), "u1", f, 2000, KEY)).toBe(true);
  });
  it("fails for another user, other fields, a forged or expired token", () => {
    const t = signCsrf("u1", f, 1000, KEY);
    expect(verifyCsrf(t, "u2", f, 2000, KEY)).toBe(false);
    expect(verifyCsrf(t, "u1", [...f.slice(0, 4), "other"], 2000, KEY)).toBe(false);
    expect(verifyCsrf(t, "u1", f, 1000 + CSRF_TTL_MS + 1, KEY)).toBe(false);
    expect(verifyCsrf(t, "u1", f, 2000, "z".repeat(40))).toBe(false);
    expect(verifyCsrf(undefined, "u1", f, 2000, KEY)).toBe(false);
    expect(verifyCsrf("garbage", "u1", f, 2000, KEY)).toBe(false);
  });
});
