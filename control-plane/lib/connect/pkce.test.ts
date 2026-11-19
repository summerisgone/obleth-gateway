import { describe, expect, it } from "vitest";
import { challengeFor, isCodeChallenge, isCodeVerifier, verifyPkce } from "./pkce";

// RFC 7636 appendix B.
const VERIFIER = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

describe("pkce", () => {
  it("matches the RFC 7636 appendix B vector", () => {
    expect(challengeFor(VERIFIER)).toBe(CHALLENGE);
    expect(verifyPkce(VERIFIER, CHALLENGE)).toBe(true);
  });
  it("rejects another verifier or challenge", () => {
    expect(verifyPkce(VERIFIER + "x", CHALLENGE)).toBe(false);
    expect(verifyPkce(VERIFIER, "A" + CHALLENGE.slice(1))).toBe(false);
    expect(verifyPkce(VERIFIER, "")).toBe(false);
  });
  it("checks the challenge format: 43 to 128 base64url characters", () => {
    expect(isCodeChallenge(CHALLENGE)).toBe(true);
    expect(isCodeChallenge("a".repeat(42))).toBe(false);
    expect(isCodeChallenge("a".repeat(129))).toBe(false);
    expect(isCodeChallenge("a".repeat(127) + "=")).toBe(false);
    expect(isCodeChallenge("a".repeat(43) + "+")).toBe(false);
    expect(isCodeChallenge(undefined)).toBe(false);
  });
  it("checks the verifier format", () => {
    expect(isCodeVerifier(VERIFIER)).toBe(true);
    expect(isCodeVerifier("a".repeat(42))).toBe(false);
    expect(isCodeVerifier("a".repeat(43) + " ")).toBe(false);
  });
});
