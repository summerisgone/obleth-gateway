import { describe, expect, it } from "vitest";
import { contentSecurityPolicy, generateNonce } from "./csp";

function directive(policy: string, name: string): string | undefined {
  return policy
    .split(";")
    .map((d) => d.trim())
    .find((d) => d.startsWith(`${name} `));
}

describe("contentSecurityPolicy", () => {
  it("allows scripts only by nonce in production", () => {
    const policy = contentSecurityPolicy("abc123", { development: false });
    const script = directive(policy, "script-src");
    expect(script).toContain("'nonce-abc123'");
    expect(script).toContain("'strict-dynamic'");
    expect(script).not.toContain("'unsafe-inline'");
    expect(script).not.toContain("'unsafe-eval'");
  });

  it("adds eval only for development tooling", () => {
    const script = directive(contentSecurityPolicy("n", { development: true }), "script-src");
    expect(script).toContain("'unsafe-eval'");
    expect(script).not.toContain("'unsafe-inline'");
  });

  it("keeps the framing, navigation, and object restrictions", () => {
    const policy = contentSecurityPolicy("n", { development: false });
    expect(directive(policy, "frame-ancestors")).toBe("frame-ancestors 'none'");
    expect(directive(policy, "base-uri")).toBe("base-uri 'self'");
    expect(directive(policy, "form-action")).toBe("form-action 'self'");
    expect(directive(policy, "object-src")).toBe("object-src 'none'");
    expect(directive(policy, "default-src")).toBe("default-src 'self'");
  });
});

describe("contentSecurityPolicy / media", () => {
  // Without its own directive media falls back to default-src 'self', which
  // blocks the Playground's replay of speech assembled into a blob: URL.
  it("lets audio play from same-origin and blob: URLs only", () => {
    expect(directive(contentSecurityPolicy("n", { development: false }), "media-src")).toBe("media-src 'self' blob:");
  });
});

describe("contentSecurityPolicy / connect", () => {
  // The 3D viewer fetches its model, and three.js its embedded textures, from
  // blob: URLs; with connect-src 'self' alone the browser refuses both.
  it("lets fetch read same-origin, blob: and data: URLs only", () => {
    expect(directive(contentSecurityPolicy("n", { development: false }), "connect-src")).toBe("connect-src 'self' blob: data:");
  });
});

describe("generateNonce", () => {
  it("returns base64 text that differs per call", () => {
    const a = generateNonce();
    const b = generateNonce();
    expect(a).toMatch(/^[A-Za-z0-9+/]+=*$/);
    expect(a).not.toBe(b);
    expect(Buffer.from(a, "base64")).toHaveLength(16);
  });
});
