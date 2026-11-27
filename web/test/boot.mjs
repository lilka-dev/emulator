// Boots a firmware headless in Node and reports how fast guest time runs.
//
//   node web/test/boot.mjs FIRMWARE.bin [WALL_SECONDS] [--threads single|multi]
//                          [--frame out.ppm] [--app APP.bin] [--sd FOLDER]
//                          [--audio out.pcm]   (s16le 48 kHz stereo, as drained)
//                          [--keys MS:PIN,...] (press GPIO PIN at guest millisecond MS)
//
// Uses dist/web (python3 web/build.py), or LILKA_WEB_DIST. Exit status 0 means
// the display showed something other than black before the time ran out.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const dist = path.resolve(process.env.LILKA_WEB_DIST ?? path.join(here, '../../dist/web'));
const args = process.argv.slice(2);
const option = (name, fallback) => {
  const at = args.indexOf(name);
  if (at < 0) return fallback;
  const [, value] = args.splice(at, 2);
  return value;
};
const threads = option('--threads', 'multi');
const frameOut = option('--frame');
const app = option('--app');
const sdFolder = option('--sd');
const audioOut = option('--audio');
const keys = (option('--keys') ?? '').split(',').filter(Boolean)
  .map((key) => key.split(':').map(Number)).sort((a, b) => a[0] - b[0]);
const [firmware, seconds = '60'] = args;
if (!firmware) {
  console.error('usage: node web/test/boot.mjs FIRMWARE.bin [WALL_SECONDS] [--threads single|multi]');
  process.exit(2);
}

const { default: createLilkaEmulator } = await import(pathToFileURL(path.join(dist, 'qemu-system-xtensa.js')));
const uart = [];
const module = await createLilkaEmulator({
  print: (line) => uart.push(line),
  printErr: (line) => console.error('[qemu]', line),
});
const { FS } = module;
FS.mkdirTree('/lilka/rom');
FS.mkdirTree('/lilka/sd');
FS.writeFile('/lilka/rom/esp32s3_rev0_rom.bin', fs.readFileSync(path.join(dist, 'rom/esp32s3_rev0_rom.bin')));
FS.writeFile('/lilka/firmware.bin', fs.readFileSync(firmware));
const argv = ['--firmware', '/lilka/firmware.bin', '--threads', threads];
if (app) {
  FS.writeFile('/lilka/app.bin', fs.readFileSync(app));
  argv.push('--app', '/lilka/app.bin');
}
if (sdFolder) {
  for (const name of fs.readdirSync(sdFolder)) {
    const source = path.join(sdFolder, name);
    if (fs.statSync(source).isFile()) FS.writeFile('/lilka/sd/' + name, fs.readFileSync(source));
  }
}

const layout = JSON.parse(module.UTF8ToString(module._lilka_web_layout()));
const base = module._lilka_web_live();
const memory = module.HEAPU8.buffer;
const words = new BigUint64Array(memory, base, layout.size / 8);
const u32 = new Uint32Array(memory, base, layout.size / 4);
const read64 = (offset) => Number(Atomics.load(words, offset / 8));

const started = performance.now();
module.callMain(argv);

// Drain the PCM ring like the page does, every 10 ms.
const audio = [];
const ring = new Uint32Array(memory, base + layout.audio, layout.audio_capacity);
const drain = () => {
  const head = Atomics.load(words, layout.audio_head / 8);
  const tail = Atomics.load(words, layout.audio_tail / 8);
  const count = Number(head - tail);
  if (!count) return;
  const chunk = new Uint32Array(count);
  for (let i = 0; i < count; i++) chunk[i] = ring[Number((tail + BigInt(i)) % BigInt(layout.audio_capacity))];
  audio.push(chunk);
  Atomics.store(words, layout.audio_tail / 8, head);
};
const drainer = audioOut ? setInterval(drain, 10) : null;

// Scripted presses: active-low pins, each held for 120 guest milliseconds.
const input = new Uint32Array(memory, base + layout.input, 2);
const setPin = (pin, high) => (high ? Atomics.or : Atomics.and)(input, pin >> 5, high ? 1 << (pin & 31) : ~(1 << (pin & 31)));
const presser = keys.length ? setInterval(() => {
  const ms = read64(layout.time) / 240e3;
  for (const key of keys) {
    if (!key.down && ms >= key[0]) { setPin(key[1], false); key.down = true; }
    if (key.down && !key.up && ms >= key[0] + 120) { setPin(key[1], true); key.up = true; }
  }
  if (keys.every((key) => key.up)) clearInterval(presser);
}, 5) : null;

// When the screen first shows something other than black.
let firstPicture = null;
const watcher = setInterval(() => {
  const pixels = new Uint32Array(memory, base + layout.pixels, layout.width * layout.height);
  if (pixels.some((pixel) => (pixel & 0xffffff) !== 0)) {
    firstPicture = { wall: (performance.now() - started) / 1000, guest: read64(layout.time) / 240e6 };
    console.log(`first picture: wall ${firstPicture.wall.toFixed(1)}s guest ${firstPicture.guest.toFixed(2)}s`);
    clearInterval(watcher);
  }
}, 100);

let lit = false;
let last = { wall: started, guest: 0 };
const deadline = started + Number(seconds) * 1000;
while (performance.now() < deadline) {
  await new Promise((resolve) => setTimeout(resolve, 2000));
  const now = performance.now();
  const guest = read64(layout.time) / 240e6;
  const status = Atomics.load(u32, layout.status / 4);
  const frames = read64(layout.frame) / 2;
  const pixels = new Uint32Array(memory, base + layout.pixels, layout.width * layout.height);
  lit ||= pixels.some((pixel) => (pixel & 0xffffff) !== 0);
  const speed = (guest - last.guest) / ((now - last.wall) / 1000);
  console.log(`wall ${((now - started) / 1000).toFixed(1)}s  guest ${guest.toFixed(3)}s  ` +
    `speed ${(speed * 100).toFixed(1)}%  frames ${frames}  status ${status}  lit ${lit}`);
  last = { wall: now, guest };
  if (status >= 3) break;
}

if (audioOut) {
  clearInterval(drainer);
  drain();
  fs.writeFileSync(audioOut, Buffer.concat(audio.map((chunk) => Buffer.from(chunk.buffer))));
  console.log('audio:', audioOut, audio.reduce((sum, chunk) => sum + chunk.length, 0), 'frames');
}
if (frameOut) {
  const { width, height } = layout;
  const rgba = new Uint8Array(memory, base + layout.pixels, width * height * 4);
  const ppm = Buffer.alloc(width * height * 3);
  for (let i = 0; i < width * height; i++) rgba.slice(i * 4, i * 4 + 3).forEach((v, c) => (ppm[i * 3 + c] = v));
  fs.writeFileSync(frameOut, Buffer.concat([Buffer.from(`P6\n${width} ${height}\n255\n`), ppm]));
  console.log('frame:', frameOut);
}
try {
  const log = FS.readFile('/tmp/lilka-uart.log', { encoding: 'utf8' });
  console.log('--- UART (last 30 lines) ---\n' + log.split('\n').slice(-30).join('\n'));
} catch {
  console.log('--- no UART log ---');
}
process.exit(lit ? 0 : 1);
