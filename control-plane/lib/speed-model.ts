import type { ModelSpeedPoint } from "@/lib/obleth";
import { daysBetween, eachDay, SPLIT_TONES, type DayRange } from "@/lib/reports-model";
import { linkTime } from "@/lib/log-links";

/**
 * The Reports page's model speed section: one row per model with its speed
 * against the period before, the hours or days the chart and trend lines are
 * cut into, and the chart's rows.
 */

/** Under this many measured requests a median says little, so the row is marked. */
export const FEW_REQUESTS = 20;
/** A model this much slower than the period before counts as slowed down. */
export const SLOWDOWN = 0.2;
/** Most models drawn on the chart at once. */
export const CHART_MAX = SPLIT_TONES.length;

export type SpeedSort = "busiest" | "fastest" | "slowest" | "slowed";

export const SPEED_SORTS: { value: SpeedSort; label: string }[] = [
  { value: "busiest", label: "Busiest" },
  { value: "fastest", label: "Fastest" },
  { value: "slowest", label: "Slowest" },
  { value: "slowed", label: "Slowed down" },
];

export interface SpeedRow {
  model: string;
  requests: number;
  rated: number;
  tps: number;
  slowTps: number;
  promptTps: number;
  ttftMs: number;
  previousTps: number | null;
  /** Change in speed against the period before, as a fraction (-0.25 is 25% slower). */
  change: number | null;
  slowedDown: boolean;
  /** Too few measured requests for the median to mean much. */
  few: boolean;
  /** Median speed per hour or day, oldest first, leaving out ones with no measured requests. */
  trend: number[];
}

/**
 * One row per model that produced tokens in the range. Models with no measured
 * requests (embeddings, images, audio in, every request failed) are left out.
 * A change is only given when both periods have something to compare.
 */
export function speedRows(current: ModelSpeedPoint[], previous: ModelSpeedPoint[] | null, series: ModelSpeedPoint[]): SpeedRow[] {
  const before = new Map((previous ?? []).filter((p) => p.rated > 0).map((p) => [p.model, p]));
  const trends = new Map<string, ModelSpeedPoint[]>();
  for (const p of series) if (p.rated > 0) trends.set(p.model, [...(trends.get(p.model) ?? []), p]);
  return current
    .filter((p) => p.rated > 0)
    .map((p) => {
      const prev = before.get(p.model);
      const change = prev && prev.gen_tokens_per_sec > 0 ? (p.gen_tokens_per_sec - prev.gen_tokens_per_sec) / prev.gen_tokens_per_sec : null;
      const few = p.rated < FEW_REQUESTS;
      return {
        model: p.model,
        requests: p.requests,
        rated: p.rated,
        tps: p.gen_tokens_per_sec,
        slowTps: p.slow_tokens_per_sec,
        promptTps: p.prompt_tokens_per_sec,
        ttftMs: p.p50_ttft_ms,
        previousTps: prev ? prev.gen_tokens_per_sec : null,
        change,
        slowedDown: change !== null && change <= -SLOWDOWN && !few && (prev?.rated ?? 0) >= FEW_REQUESTS,
        few,
        trend: (trends.get(p.model) ?? []).sort((a, b) => a.bucket_ms - b.bucket_ms).map((t) => t.gen_tokens_per_sec),
      };
    });
}

export function sortSpeedRows(rows: SpeedRow[], sort: SpeedSort): SpeedRow[] {
  const by = (a: SpeedRow, b: SpeedRow) =>
    sort === "fastest" ? b.tps - a.tps
    : sort === "slowest" ? a.tps - b.tps
    : sort === "slowed" ? (a.change ?? Infinity) - (b.change ?? Infinity)
    : b.rated - a.rated;
  return [...rows].sort((a, b) => by(a, b) || b.rated - a.rated || a.model.localeCompare(b.model));
}

export type SpeedBucket = "hour" | "day";

/** Hours for a range of a day or two, days for anything longer. */
export function speedBucket(range: DayRange): SpeedBucket {
  return daysBetween(range) <= 2 ? "hour" : "day";
}

/** The start of every hour or day in the range, in the reader's time, as epoch millis. */
export function bucketStarts(range: DayRange, bucket: SpeedBucket, now = Date.now()): number[] {
  const days = eachDay(range).map((d) => linkTime(d, false)!);
  if (bucket === "day") return days;
  const out: number[] = [];
  const end = Math.min(linkTime(range.end, true)!, now);
  for (const d = new Date(days[0]); d.getTime() <= end && out.length < 24 * 3; d.setHours(d.getHours() + 1)) out.push(d.getTime());
  return out;
}

export interface SpeedChartRow {
  t: number;
  /** Each model's median speed, or null for an hour or day it wasn't measured. */
  [model: string]: number | null;
}

/**
 * The chart's rows. With one model it also carries that model's slowest tenth
 * (under `__slow`), so the line for typical requests sits over the line for
 * the unlucky ones.
 */
export function speedChartRows(starts: number[], series: ModelSpeedPoint[], models: string[]): SpeedChartRow[] {
  const wanted = new Set(models);
  const at = new Map<string, ModelSpeedPoint>();
  for (const p of series) if (wanted.has(p.model) && p.rated > 0) at.set(`${p.bucket_ms}|${p.model}`, p);
  return starts.map((t) => {
    const row: SpeedChartRow = { t };
    for (const m of models) row[m] = at.get(`${t}|${m}`)?.gen_tokens_per_sec ?? null;
    if (models.length === 1) row.__slow = at.get(`${t}|${models[0]}`)?.slow_tokens_per_sec ?? null;
    return row;
  });
}

/** "▼ 32%" style label for a change, or null. */
export function changeLabel(change: number | null): string | null {
  if (change === null || !Number.isFinite(change)) return null;
  const pct = Math.abs(change * 100);
  if (pct < 0.5) return "no change";
  return `${change > 0 ? "▲" : "▼"} ${pct >= 10 ? Math.round(pct) : pct.toFixed(1)}%`;
}

export function tps(v: number): string {
  if (!v || !Number.isFinite(v)) return "—";
  return v >= 100 ? `${Math.round(v)}` : v.toFixed(1);
}
