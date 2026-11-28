// Plays the board's PCM ring (48 kHz stereo s16, one u32 per frame) straight
// from shared wasm memory.
//
// Guest time does not run at exactly wall speed. While the interpreted CPU is
// late refilling I2S buffers (KeiraOS writes its startup sound one sample at a
// time), QEMU holds the clock back and PCM arrives at 93-98% of 48 kHz; after
// that, the clock catches up at up to 102%. A fixed-rate player underruns in
// the first case and overflows in the second. This one measures how fast
// frames arrive and plays them at that rate, resampled, while steering the
// buffered amount toward TARGET. Beyond the 90-105% it follows, an underrun
// holds and fades the last sample (no clicks), and a backlog over MAX_FILL
// (the audio context was suspended) is skipped.
//
// web/test/audio.mjs checks this against slow, fast, bursty and recorded producers.
const RATE = 48000;
const TARGET = RATE * 0.08;
const MAX_FILL = RATE * 0.5;
const MIN_RATIO = 0.9;
const MAX_RATIO = 1.05;
// Buffered frames beyond TARGET speed playback up by this share of TARGET.
const GAIN = 0.05;
// The arrival rate is measured over WINDOW output frames and smoothed.
const WINDOW = RATE / 4;
const RATE_SMOOTHING = 0.25;
const FILL_SMOOTHING = 0.02;

class LilkaAudio extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { memory, base, layout } = options.processorOptions;
    this.layout = layout;
    this.u64 = new BigUint64Array(memory, base, 16);
    this.ring = new Int32Array(memory, base + layout.audio, layout.audio_capacity);
    this.capacity = layout.audio_capacity;
    // Ring frames per output frame when the board runs at wall speed.
    this.nominal = RATE / sampleRate;
    this.rate = 1;
    this.window = { frames: 0, produced: 0, head: null };
    this.fill = 0;
    this.phase = 0;
    this.playing = false;
    this.last = [0, 0];
  }
  fade(left, right, from) {
    for (let i = from; i < left.length; i++) {
      this.last[0] *= 31 / 32;
      this.last[1] *= 31 / 32;
      left[i] = this.last[0];
      right[i] = this.last[1];
    }
  }
  /** Arrival rate relative to wall speed, smoothed over about a second. */
  measure(head, frames) {
    // Nothing arrives until QEMU has started; that is not a slow board.
    if (head === 0) return;
    const window = this.window;
    if (window.head !== null) window.produced += head - window.head;
    window.head = head;
    window.frames += frames;
    if (window.frames >= WINDOW) {
      // A window with no frames at all is a pause, not a slow board.
      const measured = window.produced / window.frames / this.nominal;
      if (window.produced > 0) this.rate += (measured - this.rate) * RATE_SMOOTHING;
      window.frames = window.produced = 0;
    }
  }
  process(_inputs, outputs) {
    const [left, right = left] = outputs[0];
    const { audio_head, audio_tail, played, underruns, playback_dropped } = this.layout;
    const head = Number(Atomics.load(this.u64, audio_head / 8));
    let tail = Number(Atomics.load(this.u64, audio_tail / 8));
    this.measure(head, left.length);
    let available = head - tail;
    if (available > MAX_FILL) {
      const skip = available - TARGET;
      tail += skip;
      available -= skip;
      this.phase = 0;
      Atomics.add(this.u64, playback_dropped / 8, BigInt(skip));
    }
    this.fill += (available - this.fill) * FILL_SMOOTHING;
    if (!this.playing && available < TARGET) {
      this.fade(left, right, 0);
      Atomics.store(this.u64, audio_tail / 8, BigInt(tail));
      return true;
    }
    this.playing = true;
    const steer = this.rate * (1 + GAIN * (this.fill - TARGET) / TARGET);
    const step = this.nominal * Math.min(Math.max(steer, MIN_RATIO), MAX_RATIO);
    const { ring, capacity } = this;
    const start = tail % capacity;
    // Linear interpolation between ring frames `whole` and `whole + 1`.
    let position = this.phase;
    let i = 0;
    for (; i < left.length; i++) {
      const whole = Math.floor(position);
      if (whole + 1 >= available) break;
      const a = ring[(start + whole) % capacity];
      const b = ring[(start + whole + 1) % capacity];
      const t = position - whole;
      left[i] = (((a << 16) >> 16) * (1 - t) + ((b << 16) >> 16) * t) / 32768;
      right[i] = ((a >> 16) * (1 - t) + (b >> 16) * t) / 32768;
      position += step;
    }
    if (i > 0) this.last = [left[i - 1], right[i - 1]];
    if (i < left.length) {
      this.fade(left, right, i);
      this.playing = false;
      Atomics.add(this.u64, underruns / 8, 1n);
    }
    const consumed = Math.min(Math.floor(position), available);
    this.phase = position - Math.floor(position);
    Atomics.store(this.u64, audio_tail / 8, BigInt(tail + consumed));
    Atomics.add(this.u64, played / 8, BigInt(consumed));
    return true;
  }
}
registerProcessor('lilka-audio', LilkaAudio);
