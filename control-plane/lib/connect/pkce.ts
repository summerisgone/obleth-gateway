import { createHash, timingSafeEqual } from "node:crypto";

const B64URL_43_128 = /^[A-Za-z0-9_-]{43,128}$/;
// RFC 7636 section 4.1: unreserved characters, 43 to 128.
const VERIFIER = /^[A-Za-z0-9._~-]{43,128}$/;

export const isCodeChallenge = (v: unknown): v is string => typeof v === "string" && B64URL_43_128.test(v);
export const isCodeVerifier = (v: unknown): v is string => typeof v === "string" && VERIFIER.test(v);

/** BASE64URL(SHA256(verifier)) without padding (RFC 7636 section 4.2, S256). */
export function challengeFor(verifier: string): string {
  return createHash("sha256").update(verifier, "ascii").digest("base64url");
}

export function verifyPkce(verifier: string, challenge: string): boolean {
  const a = Buffer.from(challengeFor(verifier));
  const b = Buffer.from(challenge);
  return a.length === b.length && timingSafeEqual(a, b);
}
