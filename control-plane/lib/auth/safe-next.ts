/**
 * Where sign-in may send the user afterwards: only the connect page with its
 * query, as a path on this site. Anything else (another host, `//host`, a
 * backslash trick, another path) is dropped.
 */
export function safeNext(raw: string | string[] | undefined): string | undefined {
  if (typeof raw !== "string" || raw.length > 2048) return undefined;
  return /^\/connect(\?[^\\\s#]*)?$/.test(raw) ? raw : undefined;
}
