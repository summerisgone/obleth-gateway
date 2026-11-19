import { describe, expect, it } from "vitest";
import { displayName, parseConnectRequest } from "./request";

const CH = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const base = {
  redirect_uri: "http://127.0.0.1:5000/obleth/callback",
  state: "st4te",
  code_challenge: CH,
  code_challenge_method: "S256",
  app: "DSH desktop",
  device: "laptop",
};

describe("parseConnectRequest", () => {
  it("accepts a good request", () => {
    expect(parseConnectRequest(base)).toEqual({
      kind: "ok",
      params: { redirectUri: base.redirect_uri, state: "st4te", codeChallenge: CH, app: "DSH desktop", device: "laptop" },
    });
  });
  it("is an error page, not a redirect, for a bad redirect_uri", () => {
    expect(parseConnectRequest({ ...base, redirect_uri: "https://evil.example/obleth/callback" })).toEqual({ kind: "bad-redirect" });
    expect(parseConnectRequest({ ...base, redirect_uri: undefined })).toEqual({ kind: "bad-redirect" });
    expect(parseConnectRequest({ ...base, redirect_uri: [base.redirect_uri, base.redirect_uri] })).toEqual({ kind: "bad-redirect" });
  });
  it.each([
    ["missing state", { state: undefined }, null],
    ["empty state", { state: "" }, null],
    ["state with a space", { state: "a b" }, null],
    ["state too long", { state: "a".repeat(513) }, null],
    ["missing challenge", { code_challenge: undefined }, "st4te"],
    ["short challenge", { code_challenge: "abc" }, "st4te"],
    ["plain method", { code_challenge_method: "plain" }, "st4te"],
    ["missing method", { code_challenge_method: undefined }, "st4te"],
  ])("redirects invalid_request for %s", (_n, over, state) => {
    expect(parseConnectRequest({ ...base, ...over })).toEqual({ kind: "invalid", redirectUri: base.redirect_uri, state });
  });
  it("limits and cleans display names", () => {
    expect(displayName("x".repeat(100), "d")).toHaveLength(64);
    expect(displayName("a\nb\u0000c", "d")).toBe("a b c");
    expect(displayName("   ", "d")).toBe("d");
    expect(displayName(undefined, "d")).toBe("d");
  });
});
