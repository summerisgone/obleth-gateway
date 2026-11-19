import { createHmac, timingSafeEqual } from "node:crypto";

export const CSRF_TTL_MS = 15 * 60_000;

function secret(): string {
  const s = process.env.BETTER_AUTH_SECRET ?? process.env.DASHBOARD_SESSION_SECRET;
  if (!s || s.length < 32) throw new Error("BETTER_AUTH_SECRET (or DASHBOARD_SESSION_SECRET) must be set");
  return s;
}

function mac(key: string, exp: number, userId: string, fields: string[]): string {
  return createHmac("sha256", key).update(JSON.stringify(["connect-csrf", exp, userId, ...fields])).digest("base64url");
}

/** A token bound to the signed-in user and to the request the form shows. */
export function signCsrf(userId: string, fields: string[], now = Date.now(), key = secret()): string {
  const exp = now + CSRF_TTL_MS;
  return `${exp}.${mac(key, exp, userId, fields)}`;
}

export function verifyCsrf(token: unknown, userId: string, fields: string[], now = Date.now(), key = secret()): boolean {
  if (typeof token !== "string") return false;
  const [expText, given] = token.split(".");
  const exp = Number(expText);
  if (!given || !Number.isInteger(exp) || exp < now) return false;
  const a = Buffer.from(mac(key, exp, userId, fields));
  const b = Buffer.from(given);
  return a.length === b.length && timingSafeEqual(a, b);
}
