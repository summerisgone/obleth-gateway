// The only redirect targets a desktop app may ask for (RFC 8252 loopback):
// http://127.0.0.1|[::1]|localhost:<port>/obleth/callback and nothing else.
// A whole-string match leaves no room for a query, fragment, userinfo, another
// host (`localhost.evil.com`) or another path.
const LOOPBACK = /^http:\/\/(127\.0\.0\.1|\[::1\]|localhost):([1-9][0-9]{0,4})\/obleth\/callback$/;

/** The redirect URI when it is an allowed loopback callback, else null. */
export function parseRedirectUri(raw: unknown): string | null {
  if (typeof raw !== "string") return null;
  const m = LOOPBACK.exec(raw);
  if (!m || Number(m[2]) > 65535) return null;
  return raw;
}

/** `http://127.0.0.1:1234`: what a form-action source list needs. */
export function redirectOrigin(redirectUri: string): string {
  return redirectUri.slice(0, redirectUri.indexOf("/obleth/callback"));
}

/** `redirect_uri?code=...&state=...` or `?error=...&state=...`. */
export function callbackUrl(redirectUri: string, params: { code: string } | { error: string }, state: string | null): string {
  const q = new URLSearchParams(params);
  if (state !== null) q.set("state", state);
  return `${redirectUri}?${q.toString()}`;
}
