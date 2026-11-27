// Browser frontend for the Lilka emulator.
//
// qemu-system-xtensa.js is QEMU with the Lilka board and session code compiled
// to WebAssembly (web/build.py). QEMU runs on worker threads; this page shares
// the board's live protocol buffer with it in wasm memory, the same layout the
// desktop app maps from a file (crates/lilka-board/src/live.rs).
import createLilkaEmulator from './qemu-system-xtensa.js';

// Pin numbers from src/controls.rs; the inputs are active-low.
const BUTTONS = {
  up: [38, 'arrowup'], down: [41, 'arrowdown'], left: [39, 'arrowleft'], right: [40, 'arrowright'],
  a: [5, 'z'], b: [6, 'x'], c: [10, 'c'], d: [9, 'v'], select: [0, 'tab'], start: [4, 'enter'],
};
const KEYS = Object.fromEntries(Object.entries(BUTTONS).map(([name, [, key]]) => [key, name]));
const PERSIST = '/persist';
const FIRMWARE = PERSIST + '/firmware.bin';
const FIRMWARE_INFO = PERSIST + '/firmware.json';
const SESSION = PERSIST + '/session';
const SD = PERSIST + '/sd';
const OUTPUT = '/tmp/lilka-output';
const UART = '/tmp/lilka-uart.log';
const STATUS_RUNNING = 1, STATUS_PAUSED = 2, STATUS_FINISHED = 3, STATUS_FAILED = 4;

const $ = (id) => document.getElementById(id);
const ui = {
  status: $('status'), overlay: $('overlay'), canvas: $('screen'), run: $('run'), pause: $('pause'),
  reset: $('reset'), stop: $('stop'), setup: $('setup'), firmware: $('firmware'),
  firmwareName: $('firmware-name'), app: $('app'), sd: $('sd'), sdSummary: $('sd-summary'),
  sdExport: $('sd-export'), sdClear: $('sd-clear'), newSession: $('new-session'),
  latest: $('latest'), latestLanguage: $('latest-language'),
  threads: $('threads'), uart: $('uart'),
};

let emulator;
let FS;
let live;
let pendingApp = null;
let running = false;
const log = [];
const timers = [];
function remember(line) {
  log.push(line);
  if (log.length > 500) log.splice(0, log.length - 500);
}

function setStatus(text) {
  ui.status.textContent = text;
}
function showOverlay(text) {
  ui.overlay.textContent = text;
  ui.overlay.hidden = !text;
}
function exists(path) {
  return FS.analyzePath(path).exists;
}
function sync(populate) {
  return new Promise((resolve, reject) => FS.syncfs(populate, (error) => (error ? reject(error) : resolve())));
}
function walk(directory, visit, prefix = '') {
  for (const name of FS.readdir(directory)) {
    if (name === '.' || name === '..') continue;
    const path = directory + '/' + name;
    if (FS.isDir(FS.stat(path).mode)) walk(path, visit, prefix + name + '/');
    else visit(path, prefix + name);
  }
}
function removeTree(directory) {
  for (const name of FS.readdir(directory)) {
    if (name === '.' || name === '..') continue;
    const path = directory + '/' + name;
    if (FS.isDir(FS.stat(path).mode)) {
      removeTree(path);
      FS.rmdir(path);
    } else {
      FS.unlink(path);
    }
  }
}

/** Views on the live protocol buffer shared with the board. */
class Live {
  constructor(module) {
    this.layout = JSON.parse(module.UTF8ToString(module._lilka_web_layout()));
    this.base = module._lilka_web_live();
    this.memory = module.HEAPU8.buffer;
    this.u32 = new Uint32Array(this.memory, this.base, this.layout.size / 4);
    this.u64 = new BigUint64Array(this.memory, this.base, 16);
    this.pixels = new Uint8Array(this.memory, this.base + this.layout.pixels, this.layout.width * this.layout.height * 4);
  }
  word(offset) { return Atomics.load(this.u32, offset / 4); }
  wide(offset) { return Number(Atomics.load(this.u64, offset / 8)); }
  setPin(pin, high) {
    const index = this.layout.input / 4 + (pin >> 5);
    const bit = 1 << (pin & 31);
    if (high) Atomics.or(this.u32, index, bit);
    else Atomics.and(this.u32, index, ~bit);
  }
  releaseInputs() {
    Atomics.store(this.u64, this.layout.input / 8, (1n << BigInt(this.layout.input_pins)) - 1n);
  }
  set running(value) { Atomics.store(this.u32, this.layout.run / 4, value ? 1 : 0); }
  get running() { return this.word(this.layout.run) !== 0; }
  get status() { return this.word(this.layout.status); }
  get guestSeconds() { return this.wide(this.layout.time) / 240e6; }
}

async function boot() {
  if (!crossOriginIsolated) {
    setStatus('Потрібна ізоляція сторінки');
    showOverlay('Браузер не ввімкнув SharedArrayBuffer. Відкрийте сторінку через HTTPS або localhost ' +
      '(сервер має надсилати заголовки COOP/COEP; web/serve.py робить це), а потім оновіть її.');
    return;
  }
  emulator = await createLilkaEmulator({
    print: remember,
    printErr: (line) => {
      remember(line);
      if (!line.includes('unsupported syscall')) console.warn('[lilka]', line);
    },
  });
  FS = emulator.FS;
  FS.mkdirTree(PERSIST);
  FS.mount(emulator.IDBFS, {}, PERSIST);
  await sync(true);
  FS.mkdirTree(SD);
  FS.mkdirTree(SESSION);
  FS.mkdirTree('/lilka/rom');
  const rom = await fetch('rom/esp32s3_rev0_rom.bin');
  if (!rom.ok) throw new Error('ROM ESP32-S3 не знайдено (rom/esp32s3_rev0_rom.bin)');
  FS.writeFile('/lilka/rom/esp32s3_rev0_rom.bin', new Uint8Array(await rom.arrayBuffer()));
  // Two interpreter threads boot KeiraOS about 2.5x faster than one.
  ui.threads.value = localStorage.getItem('lilka-threads') ?? 'multi';
  ui.latestLanguage.value = localStorage.getItem('lilka-language') ?? 'LANG_UK';
  refreshSetup();
  setStatus('Готово');
}

function refreshSetup() {
  const haveFirmware = exists(FIRMWARE);
  if (haveFirmware) {
    const info = exists(FIRMWARE_INFO) ? JSON.parse(FS.readFile(FIRMWARE_INFO, { encoding: 'utf8' })) : {};
    const saved = exists(SESSION + '/flash.bin') && !pendingApp ? ' Збережений сеанс буде продовжено.' : '';
    ui.firmwareName.textContent = `${info.name ?? 'firmware.bin'} (${Math.round((info.size ?? 0) / 1024)} КіБ).${saved}`;
  } else {
    ui.firmwareName.textContent = 'Прошивку ще не обрано.';
  }
  let files = 0;
  let bytes = 0;
  walk(SD, (path) => {
    files += 1;
    bytes += FS.stat(path).size;
  });
  ui.sdSummary.textContent = `SD: ${files} файлів, ${(bytes / 1048576).toFixed(1)} МіБ.`;
  ui.run.disabled = running || !haveFirmware;
  if (!running) {
    showOverlay(haveFirmware ? 'Натисніть «Запустити»' : 'Завантажте останню KeiraOS або оберіть прошивку');
  }
}

// Latest KeiraOS release. The deploy bundles it next to the page
// (firmware/latest.json, same origin); a build without it uses the copy that
// flasher.lilka.dev bundles, which GitHub Pages serves with CORS.
const FLASHER = 'https://flasher.lilka.dev/';
async function latestRelease(language) {
  try {
    const response = await fetch('firmware/latest.json', { cache: 'no-cache' });
    if (response.ok) {
      const latest = await response.json();
      if (latest.files?.[language]) return { tag: latest.tag, url: latest.files[language] };
    }
  } catch { /* not bundled with this build */ }
  const response = await fetch(FLASHER + 'firmware/releases.json', { cache: 'no-cache' });
  if (!response.ok) throw new Error('Не вдалося отримати список версій KeiraOS');
  const name = `KeiraOS_merged_${language}.bin`;
  for (const release of await response.json()) {
    if (release.draft || release.prerelease) continue;
    const asset = release.assets?.find((item) => item.name === name && item.local_url);
    if (asset) return { tag: release.tag_name, url: FLASHER + asset.local_url };
  }
  throw new Error('Не знайдено збірки KeiraOS ' + language);
}
async function downloadLatest() {
  const language = ui.latestLanguage.value;
  localStorage.setItem('lilka-language', language);
  ui.latest.disabled = true;
  try {
    setStatus('Пошук останньої KeiraOS…');
    const { tag, url } = await latestRelease(language);
    setStatus(`Завантаження KeiraOS ${tag}…`);
    const response = await fetch(url);
    if (!response.ok) throw new Error(`KeiraOS ${tag}: HTTP ${response.status}`);
    const name = `KeiraOS_${tag}_${language}.bin`;
    await chooseFirmware(new File([await response.arrayBuffer()], name));
    setStatus(`KeiraOS ${tag} готова`);
  } catch (error) {
    setStatus('Помилка');
    showOverlay(String(error.message ?? error));
  } finally {
    ui.latest.disabled = running;
  }
}

async function chooseFirmware(file) {
  const bytes = new Uint8Array(await file.arrayBuffer());
  FS.writeFile(FIRMWARE, bytes);
  FS.writeFile(FIRMWARE_INFO, JSON.stringify({ name: file.name, size: bytes.length }));
  // A different base firmware starts a fresh flash image.
  if (exists(SESSION + '/flash.bin')) FS.unlink(SESSION + '/flash.bin');
  await sync(false);
  refreshSetup();
}

async function addToSd(files) {
  for (const file of files) {
    // webkitRelativePath starts with the chosen folder's own name.
    const relative = (file.webkitRelativePath || file.name).split('/').slice(1).join('/') || file.name;
    if (relative.split('/').some((part) => part.startsWith('.'))) continue;
    const path = SD + '/' + relative;
    FS.mkdirTree(path.slice(0, path.lastIndexOf('/')));
    FS.writeFile(path, new Uint8Array(await file.arrayBuffer()));
  }
  await sync(false);
  refreshSetup();
}

async function start() {
  ui.run.disabled = true;
  ui.setup.disabled = true;
  const args = ['--firmware', FIRMWARE, '--state', SESSION, '--sd', SD, '--output', OUTPUT,
    '--uart', UART, '--threads', ui.threads.value];
  if (pendingApp) {
    FS.writeFile('/lilka/app.bin', pendingApp);
  }
  if (pendingApp) args.push('--app', '/lilka/app.bin');
  else args.push('--resume');
  live = new Live(emulator);
  await startAudio();
  emulator.callMain(args);
  running = true;
  showOverlay('');
  ui.canvas.focus();
  for (const button of [ui.pause, ui.reset, ui.stop]) button.disabled = false;
  requestAnimationFrame(draw);
  timers.push(setInterval(report, 1000), setInterval(readUart, 1000));
  // Flash writes are file writes; keep IndexedDB close behind them.
  timers.push(setInterval(() => sync(false).catch(console.warn), 15000));
}

let shownFrame = 0;
let drawnFrames = 0;
const context = ui.canvas.getContext('2d');
const image = context.createImageData(280, 240);
function draw() {
  const frame = live.word(live.layout.frame);
  if (frame !== shownFrame && (frame & 1) === 0) {
    image.data.set(live.pixels);
    // The board writes a frame between two counter increments; retry a torn copy.
    if (live.word(live.layout.frame) === frame) {
      context.putImageData(image, 0, 0);
      shownFrame = frame;
      drawnFrames += 1;
    }
  }
  if (running) requestAnimationFrame(draw);
}

let last = { wall: performance.now(), guest: 0, frames: 0 };
function report() {
  const status = live.status;
  if (status === STATUS_FAILED || status === STATUS_FINISHED) {
    running = false;
    timers.forEach(clearInterval);
    readUart();
    sync(false).catch(console.warn);
    const reason = log.filter((line) => line.startsWith('Lilka:') || line.includes('rror')).slice(-3).join('\n');
    showOverlay(status === STATUS_FAILED ? 'Не вдалося запустити:\n' + reason : 'Зупинено');
    setStatus(status === STATUS_FAILED ? 'Помилка' : 'Зупинено');
    return;
  }
  const now = performance.now();
  const guest = live.guestSeconds;
  const speed = (guest - last.guest) / ((now - last.wall) / 1000);
  const fps = (drawnFrames - last.frames) / ((now - last.wall) / 1000);
  last = { wall: now, guest, frames: drawnFrames };
  const state = status === STATUS_PAUSED ? 'Пауза' : 'Працює';
  setStatus(`${state} · ${guest.toFixed(1)} с · ${Math.round(speed * 100)}% · ${fps.toFixed(0)} к/с`);
  ui.pause.textContent = live.running ? 'Пауза' : 'Продовжити';
}

let uartOffset = 0;
function readUart() {
  if (!exists(UART)) return;
  const size = FS.stat(UART).size;
  if (size <= uartOffset) return;
  const stream = FS.open(UART, 'r');
  const bytes = new Uint8Array(size - uartOffset);
  FS.read(stream, bytes, 0, bytes.length, uartOffset);
  FS.close(stream);
  uartOffset = size;
  const atBottom = ui.uart.scrollTop + ui.uart.clientHeight >= ui.uart.scrollHeight - 4;
  ui.uart.textContent = (ui.uart.textContent + new TextDecoder().decode(bytes)).slice(-60000);
  if (atBottom) ui.uart.scrollTop = ui.uart.scrollHeight;
}

// The interpreted CPU polls buttons less often than real hardware: hold every
// press long enough that a quick tap is not missed.
const MIN_HOLD_MS = 120;
const pressedAt = {};
function press(name, down) {
  if (!running || !BUTTONS[name]) return;
  if (down) {
    pressedAt[name] = performance.now();
  } else {
    const held = performance.now() - (pressedAt[name] ?? 0);
    if (held < MIN_HOLD_MS) {
      setTimeout(() => press(name, false), MIN_HOLD_MS - held);
      return;
    }
  }
  live.setPin(BUTTONS[name][0], !down);
  document.querySelector(`[data-button="${name}"]`)?.classList.toggle('held', down);
}
function togglePause() {
  if (running) live.running = !live.running;
}
function reset() {
  if (!running) return;
  live.releaseInputs();
  live.running = true;
  emulator._lilka_web_request(1);
}
async function stop() {
  if (!running) return;
  ui.stop.disabled = true;
  setStatus('Збереження…');
  live.running = true;
  emulator._lilka_web_request(2);
  // The board writes SD changes back to the folder last, then leaves a marker.
  const deadline = performance.now() + 15000;
  while (!exists(OUTPUT + '/sd-folder-flushed') && performance.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  await sync(false);
  // QEMU initializes once per process, like the desktop app: start over.
  location.reload();
}

addEventListener('keydown', (event) => {
  if (!running || event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement) return;
  const key = event.key.toLowerCase();
  if (KEYS[key]) {
    event.preventDefault();
    if (!event.repeat) press(KEYS[key], true);
  } else if (key === 'p' && !event.repeat) {
    togglePause();
  } else if (key === 'f5') {
    event.preventDefault();
    reset();
  }
});
addEventListener('keyup', (event) => {
  const name = KEYS[event.key.toLowerCase()];
  if (name) press(name, false);
});
addEventListener('blur', () => Object.keys(pressedAt).forEach((name) => press(name, false)));
for (const button of document.querySelectorAll('[data-button]')) {
  const name = button.dataset.button;
  button.addEventListener('pointerdown', (event) => {
    button.setPointerCapture(event.pointerId);
    press(name, true);
  });
  for (const type of ['pointerup', 'pointercancel', 'lostpointercapture']) {
    button.addEventListener(type, () => press(name, false));
  }
}

let audio;
async function startAudio() {
  try {
    audio = new AudioContext({ sampleRate: 48000, latencyHint: 'interactive' });
    await audio.audioWorklet.addModule('audio-worklet.js');
    const node = new AudioWorkletNode(audio, 'lilka-audio', {
      numberOfInputs: 0,
      outputChannelCount: [2],
      processorOptions: { memory: live.memory, base: live.base, layout: live.layout },
    });
    node.connect(audio.destination);
    await audio.resume();
  } catch (error) {
    console.warn('Audio unavailable:', error);
  }
}

// A store-only ZIP of the SD folder.
function zip(entries) {
  const table = new Uint32Array(256).map((_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c;
  });
  const crc32 = (bytes) => {
    let c = ~0;
    for (const byte of bytes) c = table[(c ^ byte) & 0xff] ^ (c >>> 8);
    return ~c >>> 0;
  };
  const encoder = new TextEncoder();
  const parts = [];
  const central = [];
  let offset = 0;
  for (const [name, data] of entries) {
    const path = encoder.encode(name);
    const crc = crc32(data);
    const header = (signature, extra) => {
      const view = new DataView(new ArrayBuffer(extra ? 46 : 30));
      view.setUint32(0, signature, true);
      let at = 4;
      if (extra) view.setUint16((at += 2) - 2, 20, true);
      view.setUint16(at, 20, true);
      view.setUint16(at + 2, 0x0800, true);
      view.setUint32(at + 10, crc, true);
      view.setUint32(at + 14, data.length, true);
      view.setUint32(at + 18, data.length, true);
      view.setUint16(at + 22, path.length, true);
      if (extra) view.setUint32(42, offset, true);
      return new Uint8Array(view.buffer);
    };
    const local = header(0x04034b50, false);
    parts.push(local, path, data);
    central.push(header(0x02014b50, true), path);
    offset += local.length + path.length + data.length;
  }
  const size = central.reduce((sum, part) => sum + part.length, 0);
  const end = new DataView(new ArrayBuffer(22));
  end.setUint32(0, 0x06054b50, true);
  end.setUint16(8, entries.length, true);
  end.setUint16(10, entries.length, true);
  end.setUint32(12, size, true);
  end.setUint32(16, offset, true);
  return new Blob([...parts, ...central, new Uint8Array(end.buffer)], { type: 'application/zip' });
}

ui.firmware.addEventListener('change', () => ui.firmware.files[0] && chooseFirmware(ui.firmware.files[0]));
ui.latest.addEventListener('click', downloadLatest);
ui.app.addEventListener('change', async () => {
  const file = ui.app.files[0];
  pendingApp = file ? new Uint8Array(await file.arrayBuffer()) : null;
  refreshSetup();
});
ui.sd.addEventListener('change', () => addToSd([...ui.sd.files]));
ui.sdExport.addEventListener('click', () => {
  const entries = [];
  walk(SD, (path, name) => entries.push([name, FS.readFile(path)]));
  const link = document.createElement('a');
  link.href = URL.createObjectURL(zip(entries));
  link.download = 'lilka-sd.zip';
  link.click();
  setTimeout(() => URL.revokeObjectURL(link.href), 10000);
});
ui.sdClear.addEventListener('click', async () => {
  if (!confirm('Видалити всі файли з SD-карти в цьому браузері?')) return;
  removeTree(SD);
  await sync(false);
  refreshSetup();
});
ui.newSession.addEventListener('click', async () => {
  if (!exists(SESSION + '/flash.bin') || !confirm('Скинути збережений стан флеш-пам\'яті (налаштування, NVS)?')) return;
  FS.unlink(SESSION + '/flash.bin');
  await sync(false);
  refreshSetup();
});
ui.threads.addEventListener('change', () => localStorage.setItem('lilka-threads', ui.threads.value));
ui.run.addEventListener('click', () => start().catch((error) => {
  setStatus('Помилка');
  showOverlay(String(error));
}));
ui.pause.addEventListener('click', togglePause);
ui.reset.addEventListener('click', reset);
ui.stop.addEventListener('click', stop);

boot().catch((error) => {
  console.error(error);
  setStatus('Помилка');
  showOverlay(String(error.message ?? error));
});
