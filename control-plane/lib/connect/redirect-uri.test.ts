import { describe, expect, it } from "vitest";
import { callbackUrl, parseRedirectUri, redirectOrigin } from "./redirect-uri";

describe("parseRedirectUri", () => {
  it.each([
    "http://127.0.0.1:53682/obleth/callback",
    "http://[::1]:8080/obleth/callback",
    "http://localhost:1/obleth/callback",
    "http://localhost:65535/obleth/callback",
  ])("accepts %s", (u) => expect(parseRedirectUri(u)).toBe(u));

  it.each([
    ["https", "https://127.0.0.1:1234/obleth/callback"],
    ["other host", "http://example.com:1234/obleth/callback"],
    ["other loopback address", "http://127.0.0.2:1234/obleth/callback"],
    ["other path", "http://127.0.0.1:1234/callback"],
    ["path suffix", "http://127.0.0.1:1234/obleth/callback/x"],
    ["trailing slash", "http://127.0.0.1:1234/obleth/callback/"],
    ["query", "http://127.0.0.1:1234/obleth/callback?x=1"],
    ["empty query", "http://127.0.0.1:1234/obleth/callback?"],
    ["fragment", "http://127.0.0.1:1234/obleth/callback#x"],
    ["userinfo", "http://user@127.0.0.1:1234/obleth/callback"],
    ["userinfo host trick", "http://127.0.0.1:1234@evil.com/obleth/callback"],
    ["localhost.evil.com", "http://localhost.evil.com:1234/obleth/callback"],
    ["no port", "http://127.0.0.1/obleth/callback"],
    ["port 0", "http://127.0.0.1:0/obleth/callback"],
    ["port too large", "http://127.0.0.1:65536/obleth/callback"],
    ["leading zero port", "http://127.0.0.1:0123/obleth/callback"],
    ["trailing newline", "http://127.0.0.1:1234/obleth/callback\n"],
    ["upper-case scheme", "HTTP://127.0.0.1:1234/obleth/callback"],
    ["empty", ""],
  ])("rejects %s", (_n, u) => expect(parseRedirectUri(u)).toBeNull());

  it("rejects non-strings", () => {
    expect(parseRedirectUri(undefined)).toBeNull();
    expect(parseRedirectUri(["http://localhost:1/obleth/callback"])).toBeNull();
  });
});

describe("callbackUrl", () => {
  it("adds code and state, escaped", () => {
    expect(callbackUrl("http://localhost:1/obleth/callback", { code: "c" }, "a b&c")).toBe(
      "http://localhost:1/obleth/callback?code=c&state=a+b%26c",
    );
  });
  it("answers an error without state when there is none", () => {
    expect(callbackUrl("http://localhost:1/obleth/callback", { error: "invalid_request" }, null)).toBe(
      "http://localhost:1/obleth/callback?error=invalid_request",
    );
  });
  it("gives the origin", () => expect(redirectOrigin("http://[::1]:9/obleth/callback")).toBe("http://[::1]:9"));
});
