import { afterEach, describe, expect, it, vi } from "vitest";
import { StreamingPlayer } from "./speech-player";

interface FakeSource {
  buffer: { duration: number; data: Float32Array[] } | null;
  onended: (() => void) | null;
  at?: number;
  connect: () => void;
  start: (at: number) => void;
  stop: ReturnType<typeof vi.fn>;
}

/** Just enough of an AudioContext to watch what gets scheduled when. */
class FakeContext {
  currentTime = 0;
  destination = {};
  sources: FakeSource[] = [];
  resume = vi.fn(async () => {});
  close = vi.fn(async () => {});
  createBuffer(channels: number, length: number, sampleRate: number) {
    const data = Array.from({ length: channels }, () => new Float32Array(length));
    return { duration: length / sampleRate, data, getChannelData: (c: number) => data[c] };
  }
  createBufferSource(): FakeSource {
    const source: FakeSource = {
      buffer: null,
      onended: null,
      connect: () => {},
      start(at) { source.at = at; },
      stop: vi.fn(),
    };
    this.sources.push(source);
    return source;
  }
  decodeAudioData = vi.fn(async () => this.createBuffer(1, 8000, 8000));
}

function player() {
  const ctx = new FakeContext();
  const onFinished = vi.fn();
  return { ctx, onFinished, player: new StreamingPlayer(ctx as unknown as AudioContext, onFinished) };
}

const RATE = 1000;
const mono = { sampleRate: RATE, channels: 1 };
const samples = (n: number, value = 0.5) => new Float32Array(n).fill(value);

afterEach(() => vi.unstubAllGlobals());

describe("StreamingPlayer", () => {
  it("waits for about 0.3 s of audio, then plays it all as one buffer", () => {
    const { ctx, player: p } = player();
    p.push(samples(100), mono);
    p.push(samples(100), mono);
    expect(ctx.sources).toHaveLength(0);
    p.push(samples(100), mono);
    expect(ctx.sources).toHaveLength(1);
    expect(ctx.sources[0].buffer!.duration).toBeCloseTo(0.3);
    expect(ctx.sources[0].at).toBeCloseTo(0.05);
  });

  it("schedules later pieces back to back, with no gap", () => {
    const { ctx, player: p } = player();
    p.push(samples(300), mono);
    ctx.currentTime = 0.1;
    p.push(samples(200), mono);
    expect(ctx.sources[0].at).toBeCloseTo(0.05);
    expect(ctx.sources[1].at).toBeCloseTo(0.35);
  });

  it("splits interleaved stereo into its channels", () => {
    const { ctx, player: p } = player();
    const frames = 400;
    const stereo = new Float32Array(frames * 2);
    for (let i = 0; i < frames; i++) { stereo[i * 2] = 0.25; stereo[i * 2 + 1] = -0.25; }
    p.push(stereo.subarray(0, 200), { sampleRate: RATE, channels: 2 });
    p.push(stereo.subarray(200), { sampleRate: RATE, channels: 2 });
    const [left, right] = ctx.sources[0].buffer!.data;
    expect(left).toHaveLength(frames);
    expect(left.every((v) => v === 0.25)).toBe(true);
    expect(right.every((v) => v === -0.25)).toBe(true);
  });

  it("plays a short tail at the end and reports finished only after it has played", () => {
    const { ctx, onFinished, player: p } = player();
    p.push(samples(50), mono);
    expect(ctx.sources).toHaveLength(0);
    p.end();
    expect(ctx.sources).toHaveLength(1);
    expect(onFinished).not.toHaveBeenCalled();
    ctx.sources[0].onended!();
    expect(onFinished).toHaveBeenCalledTimes(1);
    expect(ctx.close).toHaveBeenCalled();
  });

  it("finishes at once when the stream ends with no audio", () => {
    const { onFinished, player: p } = player();
    p.end();
    expect(onFinished).toHaveBeenCalledTimes(1);
  });

  it("buffers again after running dry mid-stream", () => {
    const { ctx, player: p } = player();
    p.push(samples(300), mono);
    ctx.currentTime = 2;
    p.push(samples(100), mono);
    expect(ctx.sources).toHaveLength(1);
    p.push(samples(200), mono);
    expect(ctx.sources).toHaveLength(2);
    expect(ctx.sources[1].at).toBeCloseTo(2.05);
  });

  it("stops every source, closes the context and ignores anything later", () => {
    const { ctx, onFinished, player: p } = player();
    p.push(samples(300), mono);
    p.stop();
    expect(ctx.sources[0].stop).toHaveBeenCalled();
    expect(ctx.close).toHaveBeenCalledTimes(1);
    p.push(samples(300), mono);
    p.end();
    expect(ctx.sources).toHaveLength(1);
    expect(onFinished).not.toHaveBeenCalled();
  });

  it("decodes and plays a whole file in another format", async () => {
    const { ctx, onFinished, player: p } = player();
    await expect(p.playEncoded(new ArrayBuffer(8))).resolves.toBe(1);
    p.end();
    expect(ctx.sources).toHaveLength(1);
    ctx.sources[0].onended!();
    expect(onFinished).toHaveBeenCalled();
  });

  it("is unavailable without Web Audio", () => {
    vi.stubGlobal("AudioContext", undefined);
    expect(StreamingPlayer.create(() => {})).toBeNull();
  });
});
