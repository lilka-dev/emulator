// Runs the page's audio worklet (web/static/audio-worklet.js) in Node against
// simulated producers and checks that, once playing, it never underruns,
// never drops frames and never lets the board's ring overflow.
//
//   node web/test/audio.mjs [TRACE.csv] [--worklet FILE]
//
// The producers cover what QEMU delivers: 93-94% of 48 kHz while KeiraOS
// writes its startup sound (measured in Firefox and Chromium), the 102%
// catch-up after it, uneven delivery, and a pause. TRACE.csv is a recording
// from `node web/test/boot.mjs FIRMWARE.bin --audio-trace TRACE.csv`.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const at = args.indexOf('--worklet');
const workletFile = at < 0 ? path.join(here, '../static/audio-worklet.js') : args.splice(at, 2)[1];
const [traceFile] = args;

const RATE = 48000;
const QUANTUM = 128;
// Offsets as the worklet sees them in the live protocol buffer.
const layout = {
  audio_head: 8, audio_tail: 16, played: 24, underruns: 32, playback_dropped: 40,
  audio: 128, audio_capacity: 96000,
};

let Processor;
globalThis.sampleRate = RATE;
globalThis.AudioWorkletProcessor = class {};
globalThis.registerProcessor = (_name, processor) => (Processor = processor);
await import(pathToFileURL(path.resolve(workletFile)));

/**
 * Plays `seconds` of output while `produce(t)` says how many frames the board
 * has made by wall time t (seconds); frames arrive in chunks every `chunk` s.
 */
function simulate({ seconds, produced, chunk = 0.005, warmup = 2 }) {
  const memory = new SharedArrayBuffer(layout.audio + layout.audio_capacity * 4);
  const u64 = new BigUint64Array(memory, 0, 16);
  const ring = new Int32Array(memory, layout.audio, layout.audio_capacity);
  const worklet = new Processor({ processorOptions: { memory, base: 0, layout } });
  const left = new Float32Array(QUANTUM);
  const right = new Float32Array(QUANTUM);
  const result = { overflow: 0, underruns: 0, maxLatency: 0, minRatio: Infinity, maxRatio: 0 };
  let head = 0;
  let nextChunk = 0;
  let lastPlayed = 0;
  let underruns = 0;
  for (let n = 0; n * QUANTUM < seconds * RATE; n++) {
    const t = (n * QUANTUM) / RATE;
    // The board pushes what it made since its last push; a full ring drops the rest.
    while (nextChunk <= t) {
      const target = Math.floor(produced(nextChunk));
      const tail = Number(u64[layout.audio_tail / 8]);
      for (; head < target; head++) {
        if (head - tail >= layout.audio_capacity) {
          result.overflow += target - head;
          head = target;
          break;
        }
        const sample = Math.round(8000 * Math.sin((2 * Math.PI * 440 * head) / RATE));
        ring[head % layout.audio_capacity] = (sample & 0xffff) | (sample << 16);
      }
      u64[layout.audio_head / 8] = BigInt(head);
      nextChunk += chunk;
    }
    worklet.process([], [[left, right]]);
    const played = Number(u64[layout.played / 8]);
    if (t >= warmup) {
      const latency = (head - Number(u64[layout.audio_tail / 8])) / RATE;
      result.maxLatency = Math.max(result.maxLatency, latency);
      if (Number(u64[layout.underruns / 8]) > underruns) result.underruns += 1;
      if (n % 375 === 0 && n > 0) {
        const ratio = (played - lastPlayed) / (375 * QUANTUM);
        if (t >= warmup + 3) {
          result.minRatio = Math.min(result.minRatio, ratio);
          result.maxRatio = Math.max(result.maxRatio, ratio);
        }
      }
    }
    if (n % 375 === 0) lastPlayed = played;
    underruns = Number(u64[layout.underruns / 8]);
  }
  result.dropped = Number(u64[layout.playback_dropped / 8]);
  return result;
}

const at_ = (speed) => (t) => t * speed * RATE;
// Piecewise speeds: [[until seconds, speed], ...].
const segments = (parts) => (t) => {
  let frames = 0;
  let from = 0;
  for (const [until, speed] of parts) {
    frames += (Math.min(t, until) - from) * speed * RATE;
    if (t <= until) return frames;
    from = until;
  }
  return frames;
};

const scenarios = [
  { name: 'wall speed', seconds: 30, produced: at_(1) },
  { name: 'startup sound, 44.8k frames/s', seconds: 30, produced: at_(44800 / RATE) },
  { name: 'startup sound, 45.3k frames/s', seconds: 30, produced: at_(45300 / RATE) },
  { name: '102% catch-up', seconds: 60, produced: at_(1.02) },
  { name: '15 s at 93%, then 102%', seconds: 60, produced: segments([[15, 0.93], [Infinity, 1.02]]) },
  { name: '30 ms chunks', seconds: 30, produced: at_(1), chunk: 0.03 },
  // Paused for 5 s: one underrun when it runs dry, none after it resumes.
  { name: 'paused for 5 s', seconds: 30, produced: segments([[10, 1], [15, 0], [Infinity, 1]]), underruns: 1 },
];
if (traceFile) {
  const rows = fs.readFileSync(traceFile, 'utf8').trim().split('\n').slice(1)
    .map((line) => line.split(',').map(Number));
  const [wall, frames] = rows.at(-1);
  if (frames < (wall / 1000) * RATE * 0.5) {
    console.error(`${traceFile}: ${frames} frames in ${wall / 1000} s; record it with boot.mjs --audio-trace, which drains the ring`);
    process.exit(2);
  }
  // The board's head at wall time t, linear between samples.
  const produced = (t) => {
    const ms = t * 1000;
    let lo = 0;
    let hi = rows.length - 1;
    if (ms >= rows[hi][0]) return rows[hi][1];
    while (hi - lo > 1) {
      const mid = (lo + hi) >> 1;
      if (rows[mid][0] <= ms) lo = mid; else hi = mid;
    }
    const [w0, f0] = rows[lo];
    const [w1, f1] = rows[hi];
    return ms <= w0 ? f0 : f0 + ((f1 - f0) * (ms - w0)) / (w1 - w0);
  };
  // Boot (first seconds) is silent and slower than 90%: judge from 4 s on.
  scenarios.push({ name: `recorded ${path.basename(traceFile)}`, seconds: rows.at(-1)[0] / 1000, produced, chunk: 0.001, warmup: 4 });
}

let failed = false;
for (const { name, underruns = 0, ...scenario } of scenarios) {
  const r = simulate(scenario);
  const ok = r.underruns <= underruns && r.dropped === 0 && r.overflow === 0 && r.maxLatency < 0.5;
  failed ||= !ok;
  const ratio = r.maxRatio ? `playback ${(r.minRatio * 100).toFixed(1)}-${(r.maxRatio * 100).toFixed(1)}%` : '';
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name.padEnd(34)} underruns ${r.underruns}  dropped ${r.dropped}  ` +
    `ring overflow ${r.overflow}  max latency ${(r.maxLatency * 1000).toFixed(0)} ms  ${ratio}`);
}
process.exit(failed ? 1 : 0);
