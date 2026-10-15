import { describe, expect, it } from "vitest";
import { WavParseError, WavStreamParser, pcm16Bytes, wavBlob } from "./wav-stream";

const OPEN = 0xffffffff;

function bytesOf(...parts: (Uint8Array | string | number[])[]): Uint8Array {
  const arrays = parts.map((p) => typeof p === "string" ? new TextEncoder().encode(p) : p instanceof Uint8Array ? p : new Uint8Array(p));
  const out = new Uint8Array(arrays.reduce((n, a) => n + a.length, 0));
  let at = 0;
  for (const a of arrays) { out.set(a, at); at += a.length; }
  return out;
}

const u16 = (n: number) => [n & 0xff, (n >> 8) & 0xff];
const u32 = (n: number) => [n & 0xff, (n >>> 8) & 0xff, (n >>> 16) & 0xff, (n >>> 24) & 0xff];

function fmtChunk({ tag = 1, channels = 1, rate = 24_000, bits = 16 } = {}): Uint8Array {
  const align = channels * (bits / 8);
  return bytesOf("fmt ", u32(16), u16(tag), u16(channels), u32(rate), u32(rate * align), u16(align), u16(bits));
}

/** RIFF + WAVE + fmt + any extra chunks + the data chunk header (no samples). */
function header(opts: { tag?: number; channels?: number; rate?: number; bits?: number; riffSize?: number; dataSize?: number; before?: Uint8Array[] } = {}): Uint8Array {
  return bytesOf("RIFF", u32(opts.riffSize ?? OPEN), "WAVE", fmtChunk(opts), ...(opts.before ?? []), "data", u32(opts.dataSize ?? OPEN));
}

function int16(values: number[]): Uint8Array {
  const out = new Uint8Array(values.length * 2);
  const view = new DataView(out.buffer);
  values.forEach((v, i) => view.setInt16(i * 2, v, true));
  return out;
}

function float32(values: number[]): Uint8Array {
  const out = new Uint8Array(values.length * 4);
  const view = new DataView(out.buffer);
  values.forEach((v, i) => view.setFloat32(i * 4, v, true));
  return out;
}

/** Push `bytes` in pieces of the given sizes (cycled), collecting every sample. */
function feed(parser: WavStreamParser, bytes: Uint8Array, sizes: number[]): number[] {
  const out: number[] = [];
  let at = 0;
  for (let i = 0; at < bytes.length; i++) {
    const n = sizes[i % sizes.length];
    out.push(...parser.push(bytes.subarray(at, at + n)));
    at += n;
  }
  return out;
}

const SAMPLES = [0, 16384, -16384, 32767, -32768, 1, -1, 8192, -8192, 4096];
const EXPECTED = SAMPLES.map((s) => s / 32768);

describe("WavStreamParser", () => {
  it("decodes a streaming header with unknown (0xFFFFFFFF) sizes and reads the rate from it", () => {
    const parser = new WavStreamParser();
    const out = parser.push(bytesOf(header({ rate: 22_050 }), int16(SAMPLES)));
    expect(parser.format).toEqual({ encoding: "pcm", channels: 1, sampleRate: 22_050, bitsPerSample: 16 });
    expect([...out]).toEqual(EXPECTED);
    expect(parser.frames).toBe(SAMPLES.length);
    expect(() => parser.end()).not.toThrow();
  });

  it("keeps going for more samples after an open-ended data chunk", () => {
    const parser = new WavStreamParser();
    expect(parser.push(header()).length).toBe(0);
    expect([...parser.push(int16([16384]))]).toEqual([0.5]);
    expect([...parser.push(int16([-16384, 0]))]).toEqual([-0.5, 0]);
  });

  it("treats a data size of 0 as open-ended too", () => {
    const parser = new WavStreamParser();
    expect([...parser.push(bytesOf(header({ dataSize: 0, riffSize: 0 }), int16([16384, 16384])))]).toEqual([0.5, 0.5]);
  });

  it("survives a header delivered one byte at a time", () => {
    const parser = new WavStreamParser();
    const all = bytesOf(header(), int16(SAMPLES));
    const out = feed(parser, all, [1]);
    expect(out).toEqual(EXPECTED);
    expect(parser.format?.sampleRate).toBe(24_000);
  });

  it("joins samples split across chunk boundaries at odd offsets", () => {
    for (const sizes of [[3], [5, 7], [13, 1, 2], [45], [44, 3], [43, 2, 9]]) {
      const parser = new WavStreamParser();
      expect(feed(parser, bytesOf(header(), int16(SAMPLES)), sizes)).toEqual(EXPECTED);
      expect(parser.frames).toBe(SAMPLES.length);
    }
  });

  it("returns nothing for half a sample, then the whole sample once the rest arrives", () => {
    const parser = new WavStreamParser();
    parser.push(header());
    const sample = int16([16384]);
    expect(parser.push(sample.subarray(0, 1)).length).toBe(0);
    expect([...parser.push(sample.subarray(1))]).toEqual([0.5]);
  });

  it("skips a LIST chunk (with its pad byte) that sits between fmt and data", () => {
    const list = bytesOf("LIST", u32(5), "INFOx", [0]);
    const parser = new WavStreamParser();
    expect(feed(parser, bytesOf(header({ before: [list] }), int16(SAMPLES)), [7])).toEqual(EXPECTED);
  });

  it("decodes interleaved float32 stereo and only ever returns whole frames", () => {
    const values = [0.25, -0.25, 0.5, -0.5, 1, -1];
    const parser = new WavStreamParser();
    const all = bytesOf(header({ tag: 3, channels: 2, bits: 32, rate: 48_000 }), float32(values));
    const lengths: number[] = [];
    const out: number[] = [];
    let at = 0;
    // 6-byte pieces split each 8-byte frame unevenly.
    while (at < all.length) {
      const got = parser.push(all.subarray(at, at + 6));
      lengths.push(got.length);
      out.push(...got);
      at += 6;
    }
    expect(parser.format).toMatchObject({ encoding: "float", channels: 2, sampleRate: 48_000 });
    expect(out).toEqual(values);
    expect(lengths.every((n) => n % 2 === 0)).toBe(true);
    expect(parser.frames).toBe(3);
  });

  it("reads a complete, normally sized WAV and ignores a chunk after the data", () => {
    const data = int16(SAMPLES);
    const trailer = bytesOf("LIST", u32(4), "INFO");
    const parser = new WavStreamParser();
    const all = bytesOf(header({ riffSize: 36 + data.length + trailer.length, dataSize: data.length }), data, trailer);
    expect(feed(parser, all, [all.length])).toEqual(EXPECTED);
    expect(parser.frames).toBe(SAMPLES.length);
  });

  it("decodes 8-bit unsigned and 24-bit signed PCM", () => {
    const eight = new WavStreamParser();
    expect([...eight.push(bytesOf(header({ bits: 8 }), [128, 192, 64, 0]))]).toEqual([0, 0.5, -0.5, -1]);
    const twentyFour = new WavStreamParser();
    // 0x400000 = 0.5, 0xC00000 = -0.5, 0xFFFFFF = -1/2^23
    expect([...twentyFour.push(bytesOf(header({ bits: 24 }), [0, 0, 0x40, 0, 0, 0xc0, 0xff, 0xff, 0xff]))]).toEqual([0.5, -0.5, -1 / 8_388_608]);
  });

  it("resolves WAVE_FORMAT_EXTENSIBLE to the sub-format it carries", () => {
    const fmt = bytesOf("fmt ", u32(40), u16(0xfffe), u16(1), u32(16_000), u32(64_000), u16(4), u16(32), u16(22), u16(32), u32(4),
      u16(3), [0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
    const parser = new WavStreamParser();
    const out = parser.push(bytesOf("RIFF", u32(OPEN), "WAVE", fmt, "data", u32(OPEN), float32([0.75])));
    expect(parser.format).toEqual({ encoding: "float", channels: 1, sampleRate: 16_000, bitsPerSample: 32 });
    expect([...out]).toEqual([0.75]);
  });

  it("accepts a second complete WAV appended to the first when the format matches", () => {
    const one = int16([16384]);
    const file = (data: Uint8Array) => bytesOf(header({ dataSize: data.length, riffSize: 36 + data.length }), data);
    const parser = new WavStreamParser();
    expect(feed(parser, bytesOf(file(one), file(one)), [5])).toEqual([0.5, 0.5]);
    const changed = new WavStreamParser();
    expect(() => changed.push(bytesOf(file(one), bytesOf(header({ rate: 8000, dataSize: 2 }), one)))).toThrow(WavParseError);
  });

  it("rejects bytes that are not WAV", () => {
    expect(() => new WavStreamParser().push(new TextEncoder().encode('{"error":"nope"}'))).toThrow(WavParseError);
    expect(() => new WavStreamParser().push(bytesOf("ID3", [4, 0, 0, 0, 0, 0, 0, 0, 0, 0]))).toThrow(WavParseError);
  });

  it("rejects encodings it cannot decode", () => {
    expect(() => new WavStreamParser().push(header({ tag: 6, bits: 8 }))).toThrow(/encoding/);
    expect(() => new WavStreamParser().push(header({ bits: 12 }))).toThrow(/sample size/);
  });

  it("reports a stream that ended before any audio", () => {
    expect(() => new WavStreamParser().end()).toThrow(/empty/);
    const parser = new WavStreamParser();
    parser.push(header().subarray(0, 30));
    expect(() => parser.end()).toThrow(/header/);
  });
});

describe("wavBlob", () => {
  it("builds a correctly sized 16-bit WAV that parses back to the same audio", async () => {
    const samples = new Float32Array([0, 0.5, -0.5, 1, -1, 2, -2]);
    const blob = wavBlob([pcm16Bytes(samples.subarray(0, 3)), pcm16Bytes(samples.subarray(3))], { sampleRate: 22_050, channels: 1 });
    expect(blob.type).toBe("audio/wav");
    const bytes = new Uint8Array(await blob.arrayBuffer());
    expect(bytes.length).toBe(44 + samples.length * 2);
    const view = new DataView(bytes.buffer);
    expect(view.getUint32(4, true)).toBe(36 + samples.length * 2);
    expect(view.getUint32(40, true)).toBe(samples.length * 2);
    const parser = new WavStreamParser();
    const back = [...parser.push(bytes)];
    expect(parser.format).toEqual({ encoding: "pcm", channels: 1, sampleRate: 22_050, bitsPerSample: 16 });
    // Out-of-range input is clipped, everything else survives to 16-bit precision.
    back.forEach((v, i) => expect(v).toBeCloseTo(Math.max(-1, Math.min(1, samples[i])), 4));
  });
});
