// Plays the board's PCM ring (48 kHz stereo s16, one u32 per frame) straight
// from shared wasm memory, with the desktop's 50 ms prebuffer and 100 ms cap.
class LilkaAudio extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { memory, base, layout } = options.processorOptions;
    this.layout = layout;
    this.u64 = new BigUint64Array(memory, base, 16);
    this.ring = new Int32Array(memory, base + layout.audio, layout.audio_capacity);
    this.capacity = BigInt(layout.audio_capacity);
    this.prebuffer = 2400;
    this.limit = 4800;
    this.playing = false;
  }
  process(_inputs, outputs) {
    const [left, right = left] = outputs[0];
    const { audio_head, audio_tail, played, underruns, playback_dropped } = this.layout;
    const head = Atomics.load(this.u64, audio_head / 8);
    let tail = Atomics.load(this.u64, audio_tail / 8);
    let available = Number(head - tail);
    if (available > this.limit) {
      const skip = available - this.prebuffer;
      tail += BigInt(skip);
      available -= skip;
      Atomics.add(this.u64, playback_dropped / 8, BigInt(skip));
    }
    if (!this.playing && available < this.prebuffer) {
      left.fill(0);
      right.fill(0);
      return true;
    }
    this.playing = true;
    const count = Math.min(left.length, available);
    for (let i = 0; i < count; i++) {
      const frame = this.ring[Number((tail + BigInt(i)) % this.capacity)];
      left[i] = ((frame << 16) >> 16) / 32768;
      right[i] = (frame >> 16) / 32768;
    }
    left.fill(0, count);
    right.fill(0, count);
    if (count < left.length) {
      this.playing = false;
      Atomics.add(this.u64, underruns / 8, 1n);
    }
    Atomics.store(this.u64, audio_tail / 8, tail + BigInt(count));
    Atomics.add(this.u64, played / 8, BigInt(count));
    return true;
  }
}
registerProcessor('lilka-audio', LilkaAudio);
