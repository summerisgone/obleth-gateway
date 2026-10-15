// Incremental WAV decoding for the Playground's Speech mode.
//
// The gateway's /v1/audio/speech with `stream_format: "audio"` sends raw bytes
// as the model makes them. A streaming backend writes a WAV header whose RIFF
// and data sizes are placeholders (0xFFFFFFFF, sometimes 0) and then PCM for
// as long as it keeps talking; a non-streaming backend sends one complete WAV
// at the end. Network chunks land anywhere: inside the header, between two
// RIFF chunks, or in the middle of a sample. This parser takes those chunks in
// order and hands back whole frames as floats, so the page can play audio
// before the response is complete.

/** What the `fmt ` chunk says about the samples that follow. */
export interface WavFormat {
  /** `pcm` = signed integers (unsigned for 8-bit), `float` = IEEE floats. */
  encoding: "pcm" | "float";
  channels: number;
  sampleRate: number;
  /** Bits per sample as stored (the container size). */
  bitsPerSample: number;
}

/** The bytes are not a WAV this parser can decode. */
export class WavParseError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "WavParseError";
  }
}

const UNKNOWN_SIZE = 0xffffffff;
const WAVE_FORMAT_PCM = 1;
const WAVE_FORMAT_IEEE_FLOAT = 3;
const WAVE_FORMAT_EXTENSIBLE = 0xfffe;
// A `fmt ` chunk is 16–40 bytes in practice; anything far larger is not a
// header worth buffering.
const MAX_FMT_SIZE = 1024;
const EMPTY = new Uint8Array(0);

type Phase = "riff" | "chunk" | "wave" | "fmt" | "skip" | "data";

function ascii(bytes: Uint8Array, at: number): string {
  return String.fromCharCode(bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]);
}

function u16(bytes: Uint8Array, at: number): number {
  return bytes[at] | (bytes[at + 1] << 8);
}

function u32(bytes: Uint8Array, at: number): number {
  return (bytes[at] | (bytes[at + 1] << 8) | (bytes[at + 2] << 16) | (bytes[at + 3] << 24)) >>> 0;
}

function parseFmt(bytes: Uint8Array, at: number, size: number): WavFormat {
  let tag = u16(bytes, at);
  const channels = u16(bytes, at + 2);
  const sampleRate = u32(bytes, at + 4);
  const bitsPerSample = u16(bytes, at + 14);
  // WAVE_FORMAT_EXTENSIBLE carries the real format in the first two bytes of
  // its sub-format GUID, 24 bytes into the chunk.
  if (tag === WAVE_FORMAT_EXTENSIBLE && size >= 26) tag = u16(bytes, at + 24);
  const encoding = tag === WAVE_FORMAT_PCM ? "pcm" : tag === WAVE_FORMAT_IEEE_FLOAT ? "float" : null;
  if (!encoding) throw new WavParseError(`Unsupported WAV encoding (format tag ${tag}).`);
  const bitsOk = encoding === "pcm" ? [8, 16, 24, 32].includes(bitsPerSample) : [32, 64].includes(bitsPerSample);
  if (!bitsOk) throw new WavParseError(`Unsupported WAV sample size (${bitsPerSample}-bit ${encoding}).`);
  if (channels < 1 || channels > 32) throw new WavParseError(`Unsupported WAV channel count (${channels}).`);
  if (sampleRate < 1 || sampleRate > 768_000) throw new WavParseError(`Unsupported WAV sample rate (${sampleRate}).`);
  return { encoding, channels, sampleRate, bitsPerSample };
}

function sameFormat(a: WavFormat, b: WavFormat): boolean {
  return a.encoding === b.encoding && a.channels === b.channels && a.sampleRate === b.sampleRate && a.bitsPerSample === b.bitsPerSample;
}

/** Decode `length` bytes (a whole number of samples) to floats in [-1, 1]. */
function decode(bytes: Uint8Array, at: number, length: number, format: WavFormat): Float32Array {
  const view = new DataView(bytes.buffer, bytes.byteOffset + at, length);
  const size = format.bitsPerSample / 8;
  const out = new Float32Array(length / size);
  if (format.encoding === "float") {
    for (let i = 0; i < out.length; i++) out[i] = size === 4 ? view.getFloat32(i * 4, true) : view.getFloat64(i * 8, true);
    return out;
  }
  switch (size) {
    case 1: for (let i = 0; i < out.length; i++) out[i] = (view.getUint8(i) - 128) / 128; break;
    case 2: for (let i = 0; i < out.length; i++) out[i] = view.getInt16(i * 2, true) / 32768; break;
    case 3:
      for (let i = 0; i < out.length; i++) {
        const o = i * 3;
        out[i] = ((view.getInt8(o + 2) << 16) | (view.getUint8(o + 1) << 8) | view.getUint8(o)) / 8_388_608;
      }
      break;
    default: for (let i = 0; i < out.length; i++) out[i] = view.getInt32(i * 4, true) / 2_147_483_648;
  }
  return out;
}

function concatBytes(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (!a.length) return b;
  if (!b.length) return a;
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
}

function concatFloats(parts: Float32Array[]): Float32Array {
  if (parts.length === 1) return parts[0];
  const out = new Float32Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) { out.set(p, at); at += p.length; }
  return out;
}

/**
 * Feed it the response body chunk by chunk with `push`; each call returns the
 * whole frames that chunk completed, interleaved when there is more than one
 * channel (possibly none). Call `end` once the body is done.
 *
 * Walks the RIFF chunks properly: `fmt ` gives the format, anything else
 * before `data` (`LIST`, `fact`, ...) is skipped, and a data size of
 * 0xFFFFFFFF or 0 means "until the stream ends". A second complete WAV
 * appended after the first (some servers send one per sentence) is accepted
 * as long as its format matches.
 */
export class WavStreamParser {
  /** Known once the `fmt ` chunk has arrived. */
  format: WavFormat | null = null;
  /** Whole frames decoded so far. */
  frames = 0;

  private phase: Phase = "riff";
  private pending: Uint8Array = EMPTY;
  private fmtSize = 0;
  private skipLeft = 0;
  private dataLeft = 0;
  private dataPad = 0;
  private sawData = false;

  push(chunk: Uint8Array): Float32Array {
    const bytes = concatBytes(this.pending, chunk);
    const out: Float32Array[] = [];
    let pos = 0;
    for (;;) {
      const avail = bytes.length - pos;
      if (this.phase === "riff") {
        if (avail < 12) break;
        if (ascii(bytes, pos) !== "RIFF" || ascii(bytes, pos + 8) !== "WAVE") throw new WavParseError("The response is not WAV audio.");
        pos += 12;
        this.phase = "chunk";
      } else if (this.phase === "wave") {
        if (avail < 4) break;
        if (ascii(bytes, pos) !== "WAVE") throw new WavParseError("The response is not WAV audio.");
        pos += 4;
        this.phase = "chunk";
      } else if (this.phase === "chunk") {
        if (avail < 8) break;
        const id = ascii(bytes, pos);
        const size = u32(bytes, pos + 4);
        pos += 8;
        if (id === "data") {
          if (!this.format) throw new WavParseError("The WAV audio data arrived before its format.");
          this.sawData = true;
          const open = size === UNKNOWN_SIZE || size === 0;
          this.dataLeft = open ? Infinity : size;
          this.dataPad = open ? 0 : size & 1;
          this.phase = "data";
        } else if (id === "RIFF" && this.sawData) {
          // A whole second file follows the first; its own header comes next.
          this.phase = "wave";
        } else if (id === "fmt ") {
          if (size < 16 || size > MAX_FMT_SIZE) throw new WavParseError("The WAV format header is malformed.");
          this.fmtSize = size + (size & 1);
          this.phase = "fmt";
        } else {
          if (size === UNKNOWN_SIZE) throw new WavParseError(`The WAV "${id.trim()}" chunk has no length.`);
          this.skipLeft = size + (size & 1);
          this.phase = "skip";
        }
      } else if (this.phase === "fmt") {
        if (avail < this.fmtSize) break;
        const format = parseFmt(bytes, pos, this.fmtSize);
        if (this.format && !sameFormat(this.format, format)) throw new WavParseError("The audio format changed partway through.");
        this.format = format;
        pos += this.fmtSize;
        this.phase = "chunk";
      } else if (this.phase === "skip") {
        const n = Math.min(avail, this.skipLeft);
        pos += n;
        this.skipLeft -= n;
        if (this.skipLeft > 0) break;
        this.phase = "chunk";
      } else {
        const format = this.format!;
        const block = (format.bitsPerSample / 8) * format.channels;
        const take = Math.min(avail, this.dataLeft);
        const whole = take - (take % block);
        if (whole > 0) {
          out.push(decode(bytes, pos, whole, format));
          this.frames += whole / block;
          pos += whole;
          this.dataLeft -= whole;
        } else if (this.dataLeft < block && avail >= this.dataLeft) {
          // A declared size that is not a whole number of frames: drop the
          // stray bytes rather than wait forever for a frame that never comes.
          pos += this.dataLeft;
          this.dataLeft = 0;
        } else {
          break;
        }
        if (this.dataLeft === 0) {
          this.skipLeft = this.dataPad;
          this.phase = "skip";
        }
      }
    }
    // Only a partial header or a partial frame is ever carried over.
    this.pending = pos >= bytes.length ? EMPTY : bytes.slice(pos);
    return out.length ? concatFloats(out) : new Float32Array(0);
  }

  /** The body is complete. Throws when it ended before any audio data began. */
  end(): void {
    if (!this.sawData) {
      throw new WavParseError(this.phase === "riff" && !this.pending.length ? "The response was empty." : "The audio ended before its header was complete.");
    }
  }
}

/** Floats in [-1, 1] as little-endian 16-bit PCM bytes, the way a WAV stores them. */
export function pcm16Bytes(samples: Float32Array): Uint8Array {
  const out = new Uint8Array(samples.length * 2);
  const view = new DataView(out.buffer);
  for (let i = 0; i < samples.length; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    view.setInt16(i * 2, s < 0 ? Math.round(s * 32768) : Math.round(s * 32767), true);
  }
  return out;
}

/** A canonical 44-byte header for 16-bit PCM. */
export function wavHeader(dataBytes: number, sampleRate: number, channels: number): Uint8Array {
  const header = new Uint8Array(44);
  const view = new DataView(header.buffer);
  const text = (at: number, s: string) => { for (let i = 0; i < 4; i++) header[at + i] = s.charCodeAt(i); };
  text(0, "RIFF");
  view.setUint32(4, 36 + dataBytes, true);
  text(8, "WAVE");
  text(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, WAVE_FORMAT_PCM, true);
  view.setUint16(22, channels, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * channels * 2, true);
  view.setUint16(32, channels * 2, true);
  view.setUint16(34, 16, true);
  text(36, "data");
  view.setUint32(40, dataBytes, true);
  return header;
}

/**
 * A complete, correctly sized 16-bit WAV from the PCM collected while it
 * streamed (see `pcm16Bytes`), for the replay player and the download. The
 * chunks go into the Blob as they are, without another copy.
 */
export function wavBlob(chunks: Uint8Array[], format: Pick<WavFormat, "sampleRate" | "channels">): Blob {
  const dataBytes = chunks.reduce((n, c) => n + c.length, 0);
  return new Blob([wavHeader(dataBytes, format.sampleRate, format.channels), ...chunks] as BlobPart[], { type: "audio/wav" });
}
