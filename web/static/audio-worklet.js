// Plays the board's PCM ring (48 kHz stereo s16, one u32 per frame) straight
// from shared wasm memory.
//
// The interpreted CPU cannot always produce audio in real time. Like the
// desktop player, an underrun holds and fades the last sample instead of
// dropping to zero (no clicks). The prebuffer starts at 50 ms, doubles after
// each underrun up to 500 ms so slow audio plays in longer runs, and halves
// again after 10 s without one.
const RATE = 48000;
const MIN_PREBUFFER = RATE / 20;
const MAX_PREBUFFER = RATE / 2;
const EXTRA_LATENCY = (RATE * 3) / 20;

class LilkaAudio extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { memory, base, layout } = options.processorOptions;
    this.layout = layout;
    this.u64 = new BigUint64Array(memory, base, 16);
    this.ring = new Int32Array(memory, base + layout.audio, layout.audio_capacity);
    this.capacity = BigInt(layout.audio_capacity);
    this.prebuffer = MIN_PREBUFFER;
    this.playing = false;
    this.steady = 0;
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
  process(_inputs, outputs) {
    const [left, right = left] = outputs[0];
    const { audio_head, audio_tail, played, underruns, playback_dropped } = this.layout;
    const head = Atomics.load(this.u64, audio_head / 8);
    let tail = Atomics.load(this.u64, audio_tail / 8);
    let available = Number(head - tail);
    if (available > this.prebuffer + EXTRA_LATENCY) {
      // The page fell behind (a hidden tab, a busy main thread): drop the oldest.
      const skip = available - this.prebuffer;
      tail += BigInt(skip);
      available -= skip;
      Atomics.add(this.u64, playback_dropped / 8, BigInt(skip));
    }
    if (!this.playing && available < this.prebuffer) {
      this.fade(left, right, 0);
      Atomics.store(this.u64, audio_tail / 8, tail);
      return true;
    }
    this.playing = true;
    const count = Math.min(left.length, available);
    for (let i = 0; i < count; i++) {
      const frame = this.ring[Number((tail + BigInt(i)) % this.capacity)];
      left[i] = ((frame << 16) >> 16) / 32768;
      right[i] = (frame >> 16) / 32768;
    }
    if (count > 0) this.last = [left[count - 1], right[count - 1]];
    if (count < left.length) {
      this.fade(left, right, count);
      this.playing = false;
      this.prebuffer = Math.min(this.prebuffer * 2, MAX_PREBUFFER);
      this.steady = 0;
      Atomics.add(this.u64, underruns / 8, 1n);
    } else if ((this.steady += count) > RATE * 10 && this.prebuffer > MIN_PREBUFFER) {
      this.prebuffer = Math.max(this.prebuffer / 2, MIN_PREBUFFER);
      this.steady = 0;
    }
    Atomics.store(this.u64, audio_tail / 8, tail + BigInt(count));
    Atomics.add(this.u64, played / 8, BigInt(count));
    return true;
  }
}
registerProcessor('lilka-audio', LilkaAudio);
