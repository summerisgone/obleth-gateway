import { describe, expect, it } from "vitest";
import type { ModelSpeedPoint } from "@/lib/obleth";
import { bucketStarts, changeLabel, sortSpeedRows, speedBucket, speedChartRows, speedRows } from "./speed-model";

const point = (over: Partial<ModelSpeedPoint> = {}): ModelSpeedPoint => ({
  bucket_ms: 0, model: "m", requests: 100, rated: 90, gen_tokens_per_sec: 50, slow_tokens_per_sec: 20,
  prompt_tokens_per_sec: 2000, p50_ttft_ms: 300, ...over,
});

describe("speedRows", () => {
  it("compares with the period before and flags a model that slowed down", () => {
    const rows = speedRows(
      [point({ model: "a", gen_tokens_per_sec: 30 }), point({ model: "b", gen_tokens_per_sec: 60 })],
      [point({ model: "a", gen_tokens_per_sec: 50 }), point({ model: "b", gen_tokens_per_sec: 55 })],
      [],
    );
    const a = rows.find((r) => r.model === "a")!;
    expect(a.change).toBeCloseTo(-0.4);
    expect(a.slowedDown).toBe(true);
    expect(rows.find((r) => r.model === "b")!.slowedDown).toBe(false);
  });

  it("leaves out models that produced no tokens and marks thin samples", () => {
    const rows = speedRows([point({ model: "embed", rated: 0 }), point({ model: "a", rated: 5 })], null, []);
    expect(rows.map((r) => r.model)).toEqual(["a"]);
    expect(rows[0].few).toBe(true);
    expect(rows[0].change).toBeNull();
  });

  it("does not call a thin sample slowed down", () => {
    const rows = speedRows([point({ rated: 5, gen_tokens_per_sec: 10 })], [point({ gen_tokens_per_sec: 50 })], []);
    expect(rows[0].change).toBeCloseTo(-0.8);
    expect(rows[0].slowedDown).toBe(false);
  });

  it("builds each trend in time order from measured buckets only", () => {
    const rows = speedRows([point()], null, [
      point({ bucket_ms: 2, gen_tokens_per_sec: 40 }),
      point({ bucket_ms: 1, gen_tokens_per_sec: 30 }),
      point({ bucket_ms: 3, rated: 0, gen_tokens_per_sec: 0 }),
    ]);
    expect(rows[0].trend).toEqual([30, 40]);
  });
});

describe("sortSpeedRows", () => {
  const rows = speedRows(
    [point({ model: "a", gen_tokens_per_sec: 10, rated: 500 }), point({ model: "b", gen_tokens_per_sec: 90, rated: 50 }), point({ model: "c", gen_tokens_per_sec: 40, rated: 100 })],
    [point({ model: "a", gen_tokens_per_sec: 10 }), point({ model: "b", gen_tokens_per_sec: 100 })],
    [],
  );
  it("sorts each way", () => {
    expect(sortSpeedRows(rows, "busiest").map((r) => r.model)).toEqual(["a", "c", "b"]);
    expect(sortSpeedRows(rows, "fastest").map((r) => r.model)).toEqual(["b", "c", "a"]);
    expect(sortSpeedRows(rows, "slowest").map((r) => r.model)).toEqual(["a", "c", "b"]);
    // b slowed by 10%, a held steady, c has nothing to compare with.
    expect(sortSpeedRows(rows, "slowed").map((r) => r.model)).toEqual(["b", "a", "c"]);
  });
});

describe("buckets", () => {
  it("uses hours for a day or two and days beyond", () => {
    expect(speedBucket({ start: "2026-10-22", end: "2026-10-23" })).toBe("hour");
    expect(speedBucket({ start: "2026-10-17", end: "2026-10-23" })).toBe("day");
  });

  it("starts each day at local midnight and stops hours at now", () => {
    expect(bucketStarts({ start: "2026-10-21", end: "2026-10-23" }, "day")).toEqual([
      new Date(2026, 9, 21).getTime(), new Date(2026, 9, 22).getTime(), new Date(2026, 9, 23).getTime(),
    ]);
    const now = new Date(2026, 9, 23, 2, 30).getTime();
    expect(bucketStarts({ start: "2026-10-23", end: "2026-10-23" }, "hour", now)).toHaveLength(3);
  });
});

describe("speedChartRows", () => {
  it("puts each model's median in its bucket and leaves gaps as null", () => {
    const rows = speedChartRows([1, 2], [point({ bucket_ms: 1, model: "a", gen_tokens_per_sec: 40 }), point({ bucket_ms: 2, model: "b" })], ["a", "b"]);
    expect(rows).toEqual([{ t: 1, a: 40, b: null }, { t: 2, a: null, b: 50 }]);
  });

  it("adds the slowest tenth for a single model", () => {
    const rows = speedChartRows([1], [point({ bucket_ms: 1, model: "a" })], ["a"]);
    expect(rows[0]).toEqual({ t: 1, a: 50, __slow: 20 });
  });
});

it("labels a change", () => {
  expect(changeLabel(-0.324)).toBe("▼ 32%");
  expect(changeLabel(0.05)).toBe("▲ 5.0%");
  expect(changeLabel(0.001)).toBe("no change");
  expect(changeLabel(null)).toBeNull();
});
