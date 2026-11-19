// Fixed-window limiter per client address, in memory: the control plane runs one
// replica (docs/adr/0001-browser-key-issuance.md).
export const LIMIT_PER_MINUTE = 30;
const WINDOW_MS = 60_000;

const windows = new Map<string, { start: number; count: number }>();

/** True when this request is allowed. */
export function allow(key: string, now = Date.now(), limit = LIMIT_PER_MINUTE): boolean {
  if (windows.size > 10_000) {
    for (const [k, w] of windows) if (now - w.start >= WINDOW_MS) windows.delete(k);
  }
  const w = windows.get(key);
  if (!w || now - w.start >= WINDOW_MS) {
    windows.set(key, { start: now, count: 1 });
    return true;
  }
  w.count += 1;
  return w.count <= limit;
}

export function resetRateLimits(): void {
  windows.clear();
}

/**
 * The client's address: the entry `hops` places from the right of
 * X-Forwarded-For (the edge appends the real peer; entries to its left are
 * client-controlled), else X-Real-IP, else "unknown".
 */
export function clientAddress(headers: Headers, hops = Number(process.env.CONNECT_TRUSTED_PROXY_HOPS ?? 1)): string {
  const xff = (headers.get("x-forwarded-for") ?? "").split(",").map((s) => s.trim()).filter(Boolean);
  if (xff.length > 0 && hops >= 1) return xff[Math.max(0, xff.length - hops)]!;
  return headers.get("x-real-ip")?.trim() || "unknown";
}
