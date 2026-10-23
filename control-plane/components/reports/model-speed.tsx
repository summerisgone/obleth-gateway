"use client";

import { useState } from "react";
import Link from "next/link";
import { useQuery } from "@tanstack/react-query";
import { CartesianGrid, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { ArrowUpRight, X } from "lucide-react";
import { axisTick, chartGrid, compactAxis, timeCursor } from "@/components/chart-tooltip";
import { Meter, Panel, Segmented, Sparkline } from "@/components/overview/ui";
import type { ModelSpeedPoint } from "@/lib/obleth";
import { linkTime, type LogsLink } from "@/lib/log-links";
import { modelHref } from "@/lib/model-links";
import { compact, formatMs } from "@/lib/overview-model";
import { SPLIT_TONES, type DayRange } from "@/lib/reports-model";
import {
  bucketStarts,
  CHART_MAX,
  changeLabel,
  FEW_REQUESTS,
  SLOWDOWN,
  sortSpeedRows,
  speedBucket,
  speedChartRows,
  speedRows,
  SPEED_SORTS,
  tps,
  type SpeedBucket,
  type SpeedSort,
} from "@/lib/speed-model";
import { cn, getJson } from "@/lib/utils";

const SLOW_TONE = "hsl(240 4% 46%)";

function zone(): string | undefined {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || undefined;
  } catch {
    return undefined;
  }
}

function useSpeed(range: DayRange | null, filters: { tenantId: string; keyId: string; model: string }, bucket: "none" | SpeedBucket) {
  return useQuery({
    queryKey: ["usage-speed", range?.start, range?.end, filters.tenantId, filters.keyId, filters.model, bucket],
    enabled: !!range,
    queryFn: () => {
      const p = new URLSearchParams({
        since_ms: String(linkTime(range!.start, false)),
        until_ms: String(linkTime(range!.end, true)! + 1),
        bucket,
      });
      const tz = zone();
      if (tz && bucket !== "none") p.set("tz", tz);
      if (filters.tenantId) p.set("tenant_id", filters.tenantId);
      if (filters.keyId) p.set("key_id", filters.keyId);
      if (filters.model) p.set("model", filters.model);
      return getJson<ModelSpeedPoint[]>(`/api/live/usage/speed?${p}`);
    },
    staleTime: 60_000,
  });
}

function bucketLabel(t: number, bucket: SpeedBucket) {
  const d = new Date(t);
  return bucket === "day"
    ? d.toLocaleDateString([], { month: "short", day: "numeric" })
    : d.toLocaleString([], { weekday: "short", hour: "numeric" });
}

function SpeedTip({ active, payload, label, bucket, lines }: { active?: boolean; payload?: { payload: Record<string, number | null> }[]; label?: number; bucket: SpeedBucket; lines: { key: string; label: string; color: string; dashed?: boolean }[] }) {
  if (!active || !payload?.length || label === undefined) return null;
  const row = payload[0].payload;
  return (
    <div className="min-w-48 rounded-lg border border-border bg-[hsl(240_5%_9%)] px-3 py-2 text-xs shadow-xl">
      <p className="mb-1.5 font-mono text-muted-foreground">{bucketLabel(label, bucket)}</p>
      {lines.map((l) => (
        <div key={l.key} className="flex justify-between gap-6">
          <span className="inline-flex items-center gap-1.5">
            {l.dashed ? <span className="w-3 border-t-2 border-dashed" style={{ borderColor: l.color }} /> : <span className="h-2 w-2 rounded-[2px]" style={{ background: l.color }} />}
            {l.label}
          </span>
          <span className="font-mono tabular-nums">{row[l.key] == null ? "no requests" : `${tps(Number(row[l.key]))} tok/s`}</span>
        </div>
      ))}
    </div>
  );
}

/**
 * How fast each model answered over the report's days: one row per model with
 * its median speed against the period before, and a chart of the chosen
 * models over time. Speeds come from the request log, so they cover only the
 * days it still keeps.
 */
export function ModelSpeed({ range, previous, filters, against, retentionDays, onModel, logsFor }: {
  range: DayRange | null;
  previous: DayRange | null;
  filters: { tenantId: string; keyId: string; model: string };
  against: string;
  retentionDays: number | null;
  onModel: (model: string) => void;
  logsFor: (extra?: LogsLink) => string;
}) {
  const [sort, setSort] = useState<SpeedSort>("busiest");
  const [picked, setPicked] = useState<string[] | null>(null);
  const bucket = range ? speedBucket(range) : "day";

  const now = useSpeed(range, filters, "none");
  const before = useSpeed(previous, filters, "none");
  const series = useSpeed(range, filters, bucket);

  const rows = speedRows(now.data ?? [], before.data ?? null, series.data ?? []);
  const sorted = sortSpeedRows(rows, sort);
  const busiest = sortSpeedRows(rows, "busiest");
  const known = new Set(rows.map((r) => r.model));
  const shown = (filters.model ? [filters.model] : (picked ?? busiest.slice(0, CHART_MAX).map((r) => r.model))).filter((m) => known.has(m));
  const single = shown.length === 1;
  const starts = range ? bucketStarts(range, bucket) : [];
  const chartRows = speedChartRows(starts, series.data ?? [], shown);
  const lines: { key: string; label: string; color: string; dashed?: boolean }[] = [
    ...shown.map((m, i) => ({ key: m, label: single ? `${m} · median` : m, color: SPLIT_TONES[i % SPLIT_TONES.length] })),
    ...(single ? [{ key: "__slow", label: "Slowest tenth of requests", color: SLOW_TONE, dashed: true }] : []),
  ];
  const slowed = rows.filter((r) => r.slowedDown);
  const reliable = rows.filter((r) => !r.few);
  const fastest = [...reliable].sort((a, b) => b.tps - a.tps)[0];
  const slowest = [...reliable].sort((a, b) => a.tps - b.tps)[0];
  const maxTps = Math.max(...rows.map((r) => r.tps), 0);
  const olderThanLog = !!(range && retentionDays && linkTime(range.start, false)! < Date.now() - retentionDays * 86_400_000);

  const toggle = (model: string) => {
    if (filters.model) return;
    const cur = picked ?? busiest.slice(0, CHART_MAX).map((r) => r.model);
    setPicked(cur.includes(model) ? cur.filter((m) => m !== model) : [...cur, model].slice(-CHART_MAX));
  };

  const summary = rows.length === 0 ? null : [
    `${rows.length} model${rows.length === 1 ? "" : "s"}`,
    fastest && !filters.model ? `fastest ${fastest.model} at ${tps(fastest.tps)} tok/s` : null,
    slowest && !filters.model && slowest !== fastest ? `slowest ${slowest.model} at ${tps(slowest.tps)} tok/s` : null,
    previous ? (slowed.length ? `${slowed.length} slowed down by ${SLOWDOWN * 100}% or more vs ${against}` : `none slowed down by ${SLOWDOWN * 100}% or more vs ${against}`) : null,
  ].filter(Boolean).join(" · ");

  return (
    <Panel
      label="Model speed"
      title={filters.model ? `How fast ${filters.model} answered` : "How fast each model answered"}
      subtitle={`Output tokens a second per request, after the first token, for the median request${single ? " (the dashed line is the slowest tenth)" : ""} · successful, uncached requests only${olderThanLog ? ` · the request log keeps ${retentionDays} days, so older days have no speed` : ""}`}
      action={!filters.model && rows.length > 1 ? <Segmented<SpeedSort> label="Order" value={sort} onChange={setSort} options={SPEED_SORTS} /> : undefined}
    >
      <div className="px-[18px] pb-4 pt-3">
        {summary && <p className="mb-3 text-[12.5px] text-secondary-foreground">{summary}</p>}
        {series.isLoading || now.isLoading ? (
          <div className="skeleton h-[240px] rounded-lg" />
        ) : shown.length === 0 ? (
          <div className="flex h-[240px] items-center justify-center rounded-lg border border-dashed border-border text-sm text-muted-foreground">
            {rows.length ? "Pick models below to chart them" : "No token-producing requests in these days"}
          </div>
        ) : (
          <>
            <div className="h-[240px]">
              <ResponsiveContainer width="100%" height="100%">
                <LineChart data={chartRows} margin={{ top: 8, right: 4, left: 0, bottom: 0 }}>
                  <CartesianGrid {...chartGrid} vertical={false} />
                  <XAxis dataKey="t" type="category" tick={axisTick} axisLine={false} tickLine={false} minTickGap={28} height={20} tickFormatter={(t: number) => bucketLabel(t, bucket)} />
                  <YAxis tick={axisTick} axisLine={false} tickLine={false} width={52} tickFormatter={(v: number) => compactAxis(v)} />
                  <Tooltip cursor={timeCursor} content={<SpeedTip bucket={bucket} lines={lines} />} />
                  {lines.map((l) => (
                    <Line key={l.key} type="monotone" dataKey={l.key} stroke={l.color} strokeWidth={2} strokeDasharray={l.dashed ? "5 4" : undefined} dot={chartRows.length <= 31 ? { r: 2, strokeWidth: 0, fill: l.color } : false} connectNulls isAnimationActive={false} />
                  ))}
                </LineChart>
              </ResponsiveContainer>
            </div>
            <div className="flex flex-wrap items-center gap-x-4 gap-y-2 pt-3 text-xs text-secondary-foreground">
              {lines.map((l) => (
                <span key={l.key} className="inline-flex items-center gap-1.5">
                  {l.dashed ? <span className="w-4 border-t-2 border-dashed" style={{ borderColor: l.color }} /> : <span className="h-2.5 w-2.5 rounded-[2px]" style={{ background: l.color }} />}
                  <span className={cn(!l.dashed && "font-mono")}>{l.label}</span>
                  {!filters.model && !l.dashed && (
                    <button type="button" onClick={() => toggle(l.key)} aria-label={`Take ${l.key} off the chart`} className="text-muted-foreground hover:text-foreground"><X className="h-3 w-3" /></button>
                  )}
                </span>
              ))}
              <span className="text-muted-foreground">{bucket === "hour" ? "Per hour" : "Per day"} · tok/s{!filters.model ? ` · click a row to chart it (up to ${CHART_MAX})` : ""}</span>
            </div>
          </>
        )}
      </div>
      {rows.length > 0 && (
        <div className="overflow-x-auto border-t border-border">
          <table className="w-full min-w-[980px] text-[13px]">
            <thead>
              <tr className="border-b border-border text-left text-[11px] uppercase tracking-[0.07em] text-muted-foreground">
                <th className="py-2.5 pl-[18px] pr-3 font-semibold">Model</th>
                <th className="py-2.5 pr-3 text-right font-semibold">Requests</th>
                <th className="py-2.5 pr-3 font-semibold">Median tok/s</th>
                <th className="py-2.5 pr-3 text-right font-semibold">Slowest tenth</th>
                <th className="py-2.5 pr-3 text-right font-semibold">{previous ? "vs before" : "Change"}</th>
                <th className="py-2.5 pr-3 text-right font-semibold">First token</th>
                <th className="py-2.5 pr-3 text-right font-semibold">Prompt tok/s</th>
                <th className="w-40 py-2.5 pr-[18px] font-semibold">{bucket === "hour" ? "By hour" : "By day"}</th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((r) => {
                const on = shown.includes(r.model);
                const label = changeLabel(r.change);
                return (
                  <tr
                    key={r.model}
                    onClick={() => toggle(r.model)}
                    className={cn("border-t border-border/70 first:border-t-0", !filters.model && "cursor-pointer hover:bg-muted/30", on && !filters.model && "bg-muted/20")}
                    aria-selected={on}
                  >
                    <td className="max-w-[20rem] py-2 pl-[18px] pr-3">
                      <span className="inline-flex min-w-0 items-center gap-2">
                        {!filters.model && <span className="h-2 w-2 shrink-0 rounded-[2px]" style={{ background: on ? SPLIT_TONES[shown.indexOf(r.model) % SPLIT_TONES.length] : "transparent", outline: on ? undefined : "1px solid hsl(240 4% 30%)" }} />}
                        <span className="truncate font-mono text-[12.5px]">{r.model}</span>
                        {!filters.model && (
                          <button type="button" onClick={(e) => { e.stopPropagation(); onModel(r.model); }} className="shrink-0 text-[11.5px] text-muted-foreground hover:text-foreground" title="Report on this model only">only</button>
                        )}
                        <Link href={modelHref(r.model)} onClick={(e) => e.stopPropagation()} className="shrink-0 text-muted-foreground hover:text-foreground" title="Open the model"><ArrowUpRight className="h-3.5 w-3.5" /></Link>
                      </span>
                    </td>
                    <td className="py-2 pr-3 text-right font-mono text-[12px]">
                      <Link href={logsFor({ model: r.model })} onClick={(e) => e.stopPropagation()} className="underline-offset-2 hover:underline" title={`${r.rated.toLocaleString()} of ${r.requests.toLocaleString()} measured · see these requests`}>{compact(r.requests)}</Link>
                      {r.few && <span className="ml-1.5 text-[11px] text-muted-foreground" title={`Fewer than ${FEW_REQUESTS} measured requests, so the median says little`}>few</span>}
                    </td>
                    <td className="py-2 pr-3">
                      <div className="flex items-center gap-2">
                        <span className={cn("w-12 text-right font-mono text-[12.5px]", r.few && "text-muted-foreground")}>{tps(r.tps)}</span>
                        <Meter value={r.tps} max={maxTps || 1} className="w-28" strong={r.slowedDown} />
                      </div>
                    </td>
                    <td className="py-2 pr-3 text-right font-mono text-[12px] text-muted-foreground">{tps(r.slowTps)}</td>
                    <td className={cn("py-2 pr-3 text-right font-mono text-[12px]", r.slowedDown ? "font-semibold text-foreground" : "text-muted-foreground")} title={r.previousTps !== null ? `${tps(r.previousTps)} tok/s ${against}` : `Not used ${against}`}>
                      {label ?? (previous ? "new" : "—")}
                    </td>
                    <td className="py-2 pr-3 text-right font-mono text-[12px] text-muted-foreground">{formatMs(r.ttftMs)}</td>
                    <td className="py-2 pr-3 text-right font-mono text-[12px] text-muted-foreground">{r.promptTps ? compact(r.promptTps) : "—"}</td>
                    <td className="py-1 pr-[18px]">
                      <Sparkline values={r.trend} className="h-7" label={`${r.model} median tok/s ${bucket === "hour" ? "by hour" : "by day"}`} />
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}
