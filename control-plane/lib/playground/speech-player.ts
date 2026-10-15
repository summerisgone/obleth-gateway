// Gapless playback of audio that arrives in pieces, for the Playground's
// Speech mode. Each decoded piece becomes an AudioBuffer (at the source's own
// sample rate; Web Audio resamples to the device) scheduled to start exactly
// where the previous one ends.

/** Audio gathered before playback starts, so the first words do not stutter. */
export const START_BUFFER_SECONDS = 0.3;
/** Scheduling slack: a buffer is never started in the past. */
const LEAD_SECONDS = 0.05;

export interface PcmFormat {
  sampleRate: number;
  channels: number;
}

export class StreamingPlayer {
  private nextStart = 0;
  private queue: Float32Array[] = [];
  private queuedFrames = 0;
  private format: PcmFormat | null = null;
  private buffering = true;
  private ended = false;
  private closed = false;
  private readonly sources = new Set<AudioBufferSourceNode>();

  /** `onFinished` fires once, after `end()` and the last scheduled audio has played. Never after `stop()`. */
  constructor(private readonly ctx: AudioContext, private readonly onFinished: () => void) {
    void ctx.resume?.()?.catch(() => {});
  }

  /**
   * A player on a new AudioContext, or null when the browser has none (the
   * page then offers only the finished file). Call it from the click that
   * starts playback: browsers only let a context make sound after a gesture.
   */
  static create(onFinished: () => void): StreamingPlayer | null {
    if (typeof window === "undefined") return null;
    const Ctor = window.AudioContext ?? (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
    if (!Ctor) return null;
    try {
      return new StreamingPlayer(new Ctor(), onFinished);
    } catch {
      return null;
    }
  }

  /** Stopped, or finished playing. */
  get closedOrDone(): boolean {
    return this.closed;
  }

  /** Queue interleaved samples. Playback starts once enough is buffered. */
  push(samples: Float32Array, format: PcmFormat): void {
    if (this.closed || !samples.length) return;
    // Everything scheduled has already played and the model is still going:
    // gather a little again rather than play it in crumbs.
    if (!this.buffering && this.nextStart <= this.ctx.currentTime) this.buffering = true;
    this.format = format;
    this.queue.push(samples);
    this.queuedFrames += samples.length / format.channels;
    if (this.buffering && this.queuedFrames < START_BUFFER_SECONDS * format.sampleRate) return;
    this.flush();
  }

  /**
   * Decode and play a whole file in a format the browser knows (used when a
   * backend ignored the request for WAV). Resolves to its length in seconds.
   */
  async playEncoded(data: ArrayBuffer): Promise<number> {
    const buffer = await this.ctx.decodeAudioData(data);
    if (!this.closed) this.schedule(buffer);
    return buffer.duration;
  }

  /** No more audio is coming: play whatever is still buffered. */
  end(): void {
    if (this.closed) return;
    this.ended = true;
    this.flush();
    this.finishIfDone();
  }

  /** Silence everything now and release the audio device. */
  stop(): void {
    if (this.closed) return;
    this.closed = true;
    for (const source of this.sources) {
      source.onended = null;
      try { source.stop(); } catch { /* never started */ }
    }
    this.sources.clear();
    this.queue = [];
    void this.ctx.close().catch(() => {});
  }

  private flush(): void {
    this.buffering = false;
    if (!this.queue.length || !this.format) return;
    const { sampleRate, channels } = this.format;
    const buffer = this.ctx.createBuffer(channels, this.queuedFrames, sampleRate);
    for (let c = 0; c < channels; c++) {
      const data = buffer.getChannelData(c);
      let frame = 0;
      for (const chunk of this.queue) {
        if (channels === 1) { data.set(chunk, frame); frame += chunk.length; continue; }
        for (let i = c; i < chunk.length; i += channels) data[frame++] = chunk[i];
      }
    }
    this.queue = [];
    this.queuedFrames = 0;
    this.schedule(buffer);
  }

  private schedule(buffer: AudioBuffer): void {
    const source = this.ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(this.ctx.destination);
    const at = Math.max(this.nextStart, this.ctx.currentTime + LEAD_SECONDS);
    source.start(at);
    this.nextStart = at + buffer.duration;
    this.sources.add(source);
    source.onended = () => {
      this.sources.delete(source);
      this.finishIfDone();
    };
  }

  private finishIfDone(): void {
    if (!this.ended || this.closed || this.sources.size || this.queue.length) return;
    this.closed = true;
    void this.ctx.close().catch(() => {});
    this.onFinished();
  }
}
