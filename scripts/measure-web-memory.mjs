#!/usr/bin/env node
// Measures the memory of the web interface while it is open, so "the UI uses less memory" can be
// checked against a number instead of a feeling. It drives the installed Chrome (the launcher's
// WebView2 is Chromium too, so the same heap and DOM counters apply) against the release backend,
// which serves the built UI itself, exactly as the launcher shows it.
//
//   pnpm build && cargo build --release --locked -p mochi-server
//   node scripts/measure-web-memory.mjs [--offline] [--source LIVE|REPLAY] [--soak 10] [--cycles 20]
//        [--port 8989] [--settle 3] [--sample-every 60] [--dwell 1] [--demo] [--snapshot file]
//        [--cycle-pages markets,agents] [--chrome path]
//        [--binary path] [--out report.json]
//
// Chrome is driven over a bare DevTools protocol connection, not through Playwright. Playwright
// turns on the Network domain for every page, and with it the renderer keeps a copy of every
// response body (its default cap is far above anything a run fetches) in case a client asks for it. Every API answer the
// interface fetched was then held in the renderer and read as the interface's own growth: about
// half of the 0.29 MiB a full page tour seemed to add, none of which a user's browser holds.
// --chrome names the Chrome executable; by default the usual install locations are tried.
//
// Scenario: sample after load on the Dashboard; visit every page, sampling each once settled; stay
// on Markets for --soak minutes, sampling every --sample-every seconds; cycle through every page
// --cycles times (or only the --cycle-pages listed), sampling every five cycles and at the end.
// A page listed twice in a row (--cycle-pages agents,agents) stays put for the same wall time as
// a two-page switch, since setting the hash it already has fires no hashchange: that is the
// control which separates growth over time from growth per page switch.
// Every sample forces a garbage collection first, so it counts what is retained, not what is
// merely waiting to be collected.
//
// --offline sets MOCHIPREP_OFFLINE=1 (replay only, no OKX or news) and defaults the source to
// REPLAY; otherwise the source defaults to LIVE. --demo presses "Try Demo Mode" after load so the
// pages have accounts, agents and charts to draw rather than empty states. --snapshot writes a V8
// heap snapshot at the start of the soak (as <name>-soak-start) and another at its end, so what
// the steady state holds, and what the soak adds, can be read; around the page cycles it writes
// one after the warm-up cycles (<name>-cycles-10) and one at the end (<name>-cycles-end).
//
// On Windows the working set and private bytes of the Chrome processes are read too (renderer,
// GPU, browser, utilities), since canvases and GPU textures do not show up in the JS heap.
//
// Exit codes: 0 measured, 1 the backend or browser did not start or a sample failed.
import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { isMain } from './is-main.mjs';

export const PAGES = ['dashboard', 'markets', 'agents', 'paper', 'arena', 'history', 'settings'];
export const DEFAULT_PORT = 8989;
// 8787 is the launcher's backend and 8877 the e2e backend: measuring must never collide with
// (or worse, read the numbers of) a server someone else is running.
export const RESERVED_PORTS = [8787, 8877];
const READY_PREFIX = 'MochiPrep backend on http://127.0.0.1:';
const MIB = 1024 * 1024;
const CDP_METRICS = ['JSHeapUsedSize', 'JSHeapTotalSize', 'Nodes', 'JSEventListeners', 'Documents', 'LayoutObjects', 'Frames'];

// ---- Pure helpers (tested) --------------------------------------------------------------

export const mib = (bytes) => (bytes === undefined || bytes === null ? null : Math.round((bytes / MIB) * 100) / 100);

export function parseArgs(argv) {
  const options = { offline: false, demo: false, soak: 10, cycles: 20, port: DEFAULT_PORT, settle: 3, sampleEvery: 60, dwell: 1 };
  const valued = { '--source': 'source', '--soak': 'soak', '--cycles': 'cycles', '--port': 'port', '--settle': 'settle', '--sample-every': 'sampleEvery', '--dwell': 'dwell', '--out': 'out', '--binary': 'binary', '--snapshot': 'snapshot', '--cycle-pages': 'cyclePages', '--chrome': 'chrome' };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--offline') options.offline = true;
    else if (arg === '--demo') options.demo = true;
    else if (valued[arg]) {
      i += 1;
      if (argv[i] === undefined) throw new Error(`${arg} needs a value`);
      options[valued[arg]] = argv[i];
    } else throw new Error(`unknown argument ${arg}`);
  }
  for (const name of ['soak', 'cycles', 'port', 'settle', 'sampleEvery', 'dwell']) {
    const value = Number(options[name]);
    if (!Number.isFinite(value) || value < 0) throw new Error(`--${name} must be a non-negative number`);
    options[name] = value;
  }
  if (!Number.isInteger(options.port) || options.port < 1 || options.port > 65535) throw new Error('--port must be a port number');
  if (RESERVED_PORTS.includes(options.port)) throw new Error(`--port ${options.port} belongs to the launcher or the e2e backend`);
  if (!Number.isInteger(options.cycles)) throw new Error('--cycles must be a whole number');
  if (options.sampleEvery <= 0) throw new Error('--sample-every must be above zero');
  // Cycling a subset (say markets,agents against agents,settings) tells which page's mount and
  // unmount is behind a growth the full cycle shows.
  options.cyclePages = options.cyclePages === undefined ? [...PAGES] : String(options.cyclePages).split(',').map((p) => p.trim()).filter(Boolean);
  const unknown = options.cyclePages.filter((p) => !PAGES.includes(p));
  if (unknown.length || options.cyclePages.length === 0) throw new Error(`--cycle-pages takes a comma list of ${PAGES.join(', ')}`);
  // Offline has no live feed, so a LIVE source there would measure an empty chart.
  options.source = String(options.source ?? (options.offline ? 'REPLAY' : 'LIVE')).toUpperCase();
  if (!['LIVE', 'REPLAY'].includes(options.source)) throw new Error('--source must be LIVE or REPLAY');
  return options;
}

/** Least-squares slope of y over x; null with fewer than two distinct x. */
export function slope(points) {
  const usable = points.filter((p) => Number.isFinite(p.x) && Number.isFinite(p.y));
  if (usable.length < 2) return null;
  const n = usable.length;
  const mx = usable.reduce((s, p) => s + p.x, 0) / n;
  const my = usable.reduce((s, p) => s + p.y, 0) / n;
  let num = 0;
  let den = 0;
  for (const p of usable) {
    num += (p.x - mx) * (p.y - my);
    den += (p.x - mx) ** 2;
  }
  return den === 0 ? null : num / den;
}

/**
 * First, last, change and slope of one metric over a phase. `x` picks the axis: minutes for the
 * soak (growth per minute), cycle number for the page cycles (growth per cycle).
 */
export function growth(samples, metric, x) {
  const points = samples.map((s) => ({ x: x(s), y: s[metric] })).filter((p) => typeof p.y === 'number');
  if (points.length === 0) return null;
  const first = points[0].y;
  const last = points[points.length - 1].y;
  return { first, last, delta: last - first, slope: slope(points) };
}

/**
 * Chrome processes grouped by CDP type, with working set and private bytes added up. Processes
 * with no memory reading (exited between listing and reading) are left out, not counted as zero.
 */
export function groupProcesses(processInfo, memory) {
  const groups = {};
  for (const proc of processInfo) {
    const reading = memory.get(proc.id);
    if (!reading) continue;
    const group = (groups[proc.type] ??= { count: 0, workingSet: 0, privateBytes: 0, pids: [] });
    group.count += 1;
    group.workingSet += reading.workingSet;
    group.privateBytes += reading.privateBytes;
    group.pids.push(proc.id);
  }
  return groups;
}

/** `Get-Process ... | ConvertTo-Json`: one object, or an array of them, or nothing at all. */
export function parseProcessJson(text) {
  const trimmed = text.trim();
  if (!trimmed) return new Map();
  const rows = [JSON.parse(trimmed)].flat();
  return new Map(rows.map((row) => [Number(row.Id), { workingSet: Number(row.WorkingSet64), privateBytes: Number(row.PrivateMemorySize64) }]));
}

/** One CDP `Performance.getMetrics` answer as a plain object of the metrics this report keeps. */
export function pickMetrics(metrics) {
  const out = {};
  for (const { name, value } of metrics) if (CDP_METRICS.includes(name)) out[name] = value;
  return out;
}

const pad = (value, width) => String(value).padStart(width);

/** The printed table: one row per sample, sizes in MiB. */
export function formatTable(samples) {
  const header = ['label', 'heapUsed', 'heapTotal', 'nodes', 'listeners', 'docs', 'layout', 'renderer', 'rendPriv', 'gpu', 'chromeAll'];
  const widths = [26, 9, 9, 7, 9, 5, 7, 9, 9, 8, 10];
  const row = (cells) => cells.map((cell, i) => (i === 0 ? String(cell).padEnd(widths[i]) : pad(cell, widths[i]))).join(' ');
  const show = (value) => (value === null || value === undefined ? '-' : value);
  const lines = [row(header)];
  for (const s of samples) {
    lines.push(
      row([
        s.label.slice(0, widths[0]),
        show(mib(s.JSHeapUsedSize)),
        show(mib(s.JSHeapTotalSize)),
        show(s.Nodes),
        show(s.JSEventListeners),
        show(s.Documents),
        show(s.LayoutObjects),
        show(mib(s.processes?.renderer?.workingSet)),
        show(mib(s.processes?.renderer?.privateBytes)),
        show(mib(s.processes?.gpu?.workingSet)),
        show(mib(s.processes?.total?.workingSet)),
      ]),
    );
  }
  return lines.join('\n');
}

/** A further snapshot beside the --snapshot one: `x.heapsnapshot` with "cycles-end" -> `x-cycles-end.heapsnapshot`. */
export function siblingSnapshot(file, suffix) {
  const dot = file.lastIndexOf('.');
  const slash = Math.max(file.lastIndexOf('/'), file.lastIndexOf('\\'));
  return dot > slash ? `${file.slice(0, dot)}-${suffix}${file.slice(dot)}` : `${file}-${suffix}`;
}
export const soakStartSnapshot = (file) => siblingSnapshot(file, 'soak-start');

/** The cycle after which the warm-up (first chunk loads, JIT tier-up) is taken to be over. */
export const WARM_CYCLE = 10;

// ---- Processes --------------------------------------------------------------------------

// System tools are run by absolute path, not looked up on PATH, so a directory earlier on PATH
// cannot put another program in their place.
const SYSTEM_ROOT = process.env.SystemRoot || String.raw`C:\Windows`;
const POWERSHELL = join(SYSTEM_ROOT, 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');

function windowsMemory(pids) {
  if (pids.length === 0) return new Map();
  const list = pids.map((pid) => Number(pid)).filter(Number.isInteger).join(',');
  // Filtering the full list, not `Get-Process -Id`: that exits non-zero when any one pid has
  // exited, which a short-lived Chrome utility process often has by the time it is read.
  const command = `$ids = @(${list}); Get-Process | Where-Object { $ids -contains $_.Id } | Select-Object Id,WorkingSet64,PrivateMemorySize64 | ConvertTo-Json -Compress`;
  return parseProcessJson(execFileSync(POWERSHELL, ['-NoProfile', '-Command', command], { encoding: 'utf8' }));
}

/**
 * Chrome's own list of its processes (SystemInfo.getProcessInfo), read by working set. Only on
 * Windows, the platform the launcher's WebView2 runs on; elsewhere the report says it was skipped.
 * With one page open there is one renderer for it, though Chrome may keep a spare renderer warm:
 * the renderer figure is the largest, and `processes.renderer.count` shows whether a spare existed.
 */
async function sampleProcesses(browserSession, backendPid) {
  if (process.platform !== 'win32') return { skipped: 'process memory is read on Windows only' };
  const { processInfo } = await browserSession.send('SystemInfo.getProcessInfo');
  const memory = windowsMemory([...processInfo.map((p) => p.id), backendPid]);
  const groups = groupProcesses(processInfo, memory);
  const renderers = (groups.renderer?.pids ?? []).map((pid) => ({ pid, ...memory.get(pid) }));
  renderers.sort((a, b) => b.workingSet - a.workingSet);
  const total = Object.values(groups).reduce(
    (sum, g) => ({ count: sum.count + g.count, workingSet: sum.workingSet + g.workingSet, privateBytes: sum.privateBytes + g.privateBytes }),
    { count: 0, workingSet: 0, privateBytes: 0 },
  );
  return {
    renderer: renderers[0] ? { ...renderers[0], count: renderers.length } : undefined,
    renderers,
    gpu: groups.gpu ?? groups.GPU,
    browser: groups.browser,
    utility: groups.utility,
    total,
    byType: groups,
    backend: memory.get(backendPid),
  };
}

// ---- Driving the backend and the page ----------------------------------------------------

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function portIsFree(port) {
  return new Promise((resolve) => {
    const probe = createServer();
    probe.once('error', () => resolve(false));
    probe.listen(port, '127.0.0.1', () => probe.close(() => resolve(true)));
  });
}

function startBackend(binary, port, dataDir, offline) {
  // Inherited MOCHIPREP_* (a developer's offline or origin settings) would change what is measured.
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('MOCHIPREP_')));
  env.MOCHIPREP_PORT = String(port);
  env.MOCHIPREP_DATA_DIR = dataDir;
  if (offline) env.MOCHIPREP_OFFLINE = '1';
  const child = spawn(binary, [], { env, stdio: ['ignore', 'pipe', 'inherit'] });
  const ready = new Promise((resolve, reject) => {
    let buffer = '';
    child.stdout.on('data', (chunk) => {
      buffer += chunk;
      if (buffer.includes(READY_PREFIX)) resolve();
    });
    child.once('error', reject);
    child.once('exit', (code) => reject(new Error(`backend exited with code ${code} before it was ready`)));
    setTimeout(() => reject(new Error('backend not ready after 30 s')), 30_000).unref();
  });
  return { child, ready };
}

function gitCommit() {
  try {
    // git is a developer tool found on PATH by design; it only labels the report.
    return execFileSync('git', ['rev-parse', '--short', 'HEAD'], { encoding: 'utf8' }).trim(); // NOSONAR
  } catch {
    return 'unknown';
  }
}

/** Where Chrome is usually installed, most likely first. */
export function chromeCandidates(platform, env) {
  if (platform === 'win32') {
    const roots = [env.ProgramFiles, env['ProgramFiles(x86)'], env.LOCALAPPDATA].filter(Boolean);
    return roots.map((root) => join(root, 'Google', 'Chrome', 'Application', 'chrome.exe'));
  }
  if (platform === 'darwin') return ['/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'];
  return ['/usr/bin/google-chrome', '/usr/bin/google-chrome-stable', '/opt/google/chrome/chrome'];
}

/** Chrome's DevToolsActivePort file: the port on the first line, the browser target's path on the second. */
export function parseDevToolsActivePort(text) {
  const [port, path] = text.split(/\r?\n/);
  if (!/^\d+$/.test(port ?? '') || !path?.startsWith('/devtools/browser/')) return null;
  return `ws://127.0.0.1:${port}${path}`;
}

/**
 * The few DevTools protocol calls this script makes, over one WebSocket to the browser target.
 * Page commands go through a flattened session, so one connection carries both.
 */
class Cdp {
  #ws;
  #nextId = 0;
  #pending = new Map();
  #listeners = new Map();

  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((resolve, reject) => {
      ws.addEventListener('open', resolve, { once: true });
      ws.addEventListener('error', () => reject(new Error(`cannot connect to Chrome at ${url}`)), { once: true });
    });
    return new Cdp(ws);
  }

  constructor(ws) {
    this.#ws = ws;
    ws.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      if (message.id !== undefined) {
        const call = this.#pending.get(message.id);
        this.#pending.delete(message.id);
        if (message.error) call?.reject(new Error(`${call.method}: ${message.error.message}`));
        else call?.resolve(message.result);
        return;
      }
      for (const listener of this.#listeners.get(`${message.sessionId ?? ''} ${message.method}`) ?? []) listener(message.params);
    });
    ws.addEventListener('close', () => {
      for (const call of this.#pending.values()) call.reject(new Error(`${call.method}: Chrome closed the connection`));
      this.#pending.clear();
    });
  }

  /** A handle shaped like a Playwright CDPSession: send, on, off. No session id means the browser. */
  session(sessionId = '') {
    const key = (method) => `${sessionId} ${method}`;
    return {
      send: (method, params = {}) => {
        const id = (this.#nextId += 1);
        this.#ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
        return new Promise((resolve, reject) => this.#pending.set(id, { resolve, reject, method }));
      },
      on: (method, listener) => {
        if (!this.#listeners.has(key(method))) this.#listeners.set(key(method), new Set());
        this.#listeners.get(key(method)).add(listener);
      },
      off: (method, listener) => this.#listeners.get(key(method))?.delete(listener),
    };
  }

  close() {
    this.#ws.close();
  }
}

async function launchChrome(executable, profileDir) {
  // Playwright's own headless set, less --enable-automation: no background throttling, so timers
  // run as they would in a window someone is looking at.
  const args = [
    `--user-data-dir=${profileDir}`,
    '--remote-debugging-port=0',
    '--headless',
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-extensions',
    '--disable-component-update',
    '--disable-sync',
    '--disable-default-apps',
    '--disable-background-networking',
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--disable-renderer-backgrounding',
    '--mute-audio',
    '--hide-scrollbars',
    // Precise memory info, or performance.memory is rounded and bucketed to hide it from pages.
    '--enable-precise-memory-info',
    'about:blank',
  ];
  const chrome = spawn(executable, args, { stdio: 'ignore' });
  const exited = new Promise((resolve) => chrome.once('exit', resolve));
  chrome.once('error', () => {});
  const portFile = join(profileDir, 'DevToolsActivePort');
  for (let waited = 0; waited < 30_000; waited += 100) {
    if (chrome.exitCode !== null) break;
    const url = existsSync(portFile) ? parseDevToolsActivePort(readFileSync(portFile, 'utf8')) : null;
    if (url) return { chrome, exited, cdp: await Cdp.connect(url) };
    await sleep(100);
  }
  chrome.kill();
  throw new Error(`Chrome (${executable}) did not open its DevTools port`);
}

async function takeSnapshot(session, file) {
  const out = createWriteStream(file);
  // A write error (a missing folder, a full disk) must reject here, so main's cleanup still
  // stops the browser and backend, rather than crash the process as an unhandled stream error.
  let failure;
  out.on('error', (error) => {
    failure ??= error;
  });
  const onChunk = ({ chunk }) => out.write(chunk);
  session.on('HeapProfiler.addHeapSnapshotChunk', onChunk);
  try {
    await session.send('HeapProfiler.collectGarbage');
    await session.send('HeapProfiler.takeHeapSnapshot', { reportProgress: false });
  } finally {
    session.off('HeapProfiler.addHeapSnapshotChunk', onChunk);
    await new Promise((resolve) => (out.destroyed ? resolve() : out.end(resolve)));
  }
  if (failure) throw new Error(`could not write ${file}: ${failure.message}`);
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const binary = options.binary ?? join('target', 'release', process.platform === 'win32' ? 'mochi-server.exe' : 'mochi-server');
  if (!existsSync(binary)) throw new Error(`${binary} not found: run pnpm build, then cargo build --release --locked -p mochi-server`);
  if (!(await portIsFree(options.port))) throw new Error(`port ${options.port} is in use; pick another with --port`);

  const chromePath = options.chrome ?? chromeCandidates(process.platform, process.env).find((path) => existsSync(path));
  if (!chromePath || !existsSync(chromePath)) throw new Error('Chrome not found: install it or pass --chrome path');

  const dataDir = mkdtempSync(join(tmpdir(), 'mochi-web-memory-'));
  const profileDir = mkdtempSync(join(tmpdir(), 'mochi-web-memory-chrome-'));
  let child;
  let browser;
  try {
    const started = startBackend(binary, options.port, dataDir, options.offline);
    child = started.child;
    await started.ready;

    browser = await launchChrome(chromePath, profileDir);
    const browserSession = browser.cdp.session();
    const { targetInfos } = await browserSession.send('Target.getTargets');
    const blank = targetInfos.find((t) => t.type === 'page');
    const targetId = blank?.targetId ?? (await browserSession.send('Target.createTarget', { url: 'about:blank' })).targetId;
    const { sessionId } = await browserSession.send('Target.attachToTarget', { targetId, flatten: true });
    const session = browser.cdp.session(sessionId);
    await session.send('Page.enable');
    await session.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    // The source is read from localStorage once, at store creation, so it must be there before load.
    await session.send('Page.addScriptToEvaluateOnNewDocument', {
      source: `try { localStorage.setItem('mochi.source', ${JSON.stringify(options.source)}); } catch { /* a refused write leaves the default source, which the report shows */ }`,
    });
    await session.send('Performance.enable');
    await session.send('HeapProfiler.enable');
    const evaluate = async (expression) => {
      const { result, exceptionDetails } = await session.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
      if (exceptionDetails) throw new Error(`page script failed: ${exceptionDetails.exception?.description ?? exceptionDetails.text}`);
      return result.value;
    };

    const started_at = Date.now();
    const samples = [];
    const sample = async (label, extra = {}) => {
      await session.send('HeapProfiler.collectGarbage');
      const { metrics } = await session.send('Performance.getMetrics');
      const perfMemory = await evaluate('performance.memory ? { used: performance.memory.usedJSHeapSize, total: performance.memory.totalJSHeapSize } : null');
      const entry = {
        label,
        page: await evaluate("location.hash.slice(1) || 'dashboard'"),
        elapsedMinutes: Math.round(((Date.now() - started_at) / 60_000) * 100) / 100,
        ...extra,
        ...pickMetrics(metrics),
        performanceMemory: perfMemory,
        processes: await sampleProcesses(browserSession, child.pid),
      };
      samples.push(entry);
      console.log(`${label}: heap ${mib(entry.JSHeapUsedSize)} MiB, ${entry.Nodes} nodes, ${entry.JSEventListeners} listeners, renderer ${mib(entry.processes.renderer?.workingSet) ?? '-'} MiB`);
      return entry;
    };
    // The app routes on hashchange; a reload would throw away exactly the state being measured.
    const go = (name) => evaluate(`location.hash = ${JSON.stringify(name)}`);

    const loaded = new Promise((resolve) => session.on('Page.loadEventFired', resolve));
    await session.send('Page.navigate', { url: `http://127.0.0.1:${options.port}/#dashboard` });
    await loaded;
    await sleep(options.settle * 1000);
    if (options.demo) {
      // Waits for the button, as the Welcome card renders only once the accounts are fetched.
      const clicked = await evaluate(`new Promise((resolve) => {
        const until = Date.now() + 15000;
        const tryClick = () => {
          const button = document.querySelector('[data-testid="try-demo"]');
          if (button) { button.click(); resolve(true); } else if (Date.now() > until) resolve(false); else setTimeout(tryClick, 100);
        };
        tryClick();
      })`);
      if (!clicked) throw new Error('the Try Demo Mode button did not appear');
      await sleep(options.settle * 1000);
    }
    await sample('load:dashboard', { phase: 'load' });

    for (const name of PAGES) {
      await go(name);
      await sleep(options.settle * 1000);
      await sample(`page:${name}`, { phase: 'pages' });
    }

    await go('markets');
    await sleep(options.settle * 1000);
    const soakStart = Date.now();
    const soakEnd = soakStart + options.soak * 60_000;
    await sample('soak:0', { phase: 'soak', soakMinutes: 0 });
    // A second snapshot at the start of the soak, so what the soak adds can be diffed.
    if (options.snapshot) await takeSnapshot(session, soakStartSnapshot(options.snapshot));
    while (Date.now() < soakEnd) {
      await sleep(Math.min(options.sampleEvery * 1000, soakEnd - Date.now()));
      const minutes = Math.round(((Date.now() - soakStart) / 60_000) * 100) / 100;
      await sample(`soak:${minutes}`, { phase: 'soak', soakMinutes: minutes });
    }
    // Taken at the end of the soak, on Markets with the feed running: the steady state the
    // interface spends most of its time in.
    if (options.snapshot) {
      await takeSnapshot(session, options.snapshot);
      console.log(`heap snapshot written to ${options.snapshot}`);
    }

    // Every cycle ends on the last page, so the baseline is taken there too: pages differ by
    // thousands of nodes (Settings lists every time zone), and a baseline on Markets would read
    // that difference as growth.
    await go(options.cyclePages[options.cyclePages.length - 1]);
    await sleep(options.settle * 1000);
    await sample('cycles:0', { phase: 'cycles', cycle: 0 });
    for (let cycle = 1; cycle <= options.cycles; cycle += 1) {
      for (const name of options.cyclePages) {
        await go(name);
        await sleep(options.dwell * 1000);
      }
      if (cycle % 5 === 0 || cycle === options.cycles) {
        await sleep(options.settle * 1000);
        await sample(`cycles:${cycle}`, { phase: 'cycles', cycle });
        // A pair of snapshots around the post-warm-up cycles shows what each page switch leaves behind.
        if (options.snapshot && cycle === WARM_CYCLE && options.cycles > WARM_CYCLE) await takeSnapshot(session, siblingSnapshot(options.snapshot, `cycles-${cycle}`));
        if (options.snapshot && cycle === options.cycles) await takeSnapshot(session, siblingSnapshot(options.snapshot, 'cycles-end'));
      }
    }

    const soak = samples.filter((s) => s.phase === 'soak');
    const cycles = samples.filter((s) => s.phase === 'cycles');
    // Private bytes as well as the working set: Windows trims and refills the working set on its
    // own schedule, while private bytes only move when the renderer allocates or frees.
    const withRenderer = (list) =>
      list.map((s) => ({ ...s, rendererWorkingSet: s.processes?.renderer?.workingSet, rendererPrivateBytes: s.processes?.renderer?.privateBytes }));
    const metrics = ['JSHeapUsedSize', 'Nodes', 'JSEventListeners', 'Documents', 'LayoutObjects', 'rendererWorkingSet', 'rendererPrivateBytes'];
    const summarize = (list, x) => Object.fromEntries(metrics.map((m) => [m, growth(withRenderer(list), m, x)]));
    const report = {
      target: `${process.platform}-${process.arch}`,
      commit: gitCommit(),
      measuredAt: new Date().toISOString(),
      options: { ...options, binary },
      summary: {
        soakPerMinute: summarize(soak, (s) => s.soakMinutes),
        cyclesPerCycle: summarize(cycles, (s) => s.cycle),
        maxDocuments: Math.max(...samples.map((s) => s.Documents ?? 0)),
      },
      samples,
    };
    console.log('');
    console.log(formatTable(samples));
    const heapSlope = report.summary.soakPerMinute.JSHeapUsedSize?.slope;
    const nodeSlope = report.summary.cyclesPerCycle.Nodes?.slope;
    const listenerSlope = report.summary.cyclesPerCycle.JSEventListeners?.slope;
    const privateSlope = report.summary.cyclesPerCycle.rendererPrivateBytes?.slope;
    const cycleHeapSlope = report.summary.cyclesPerCycle.JSHeapUsedSize?.slope;
    console.log('');
    console.log(`soak heap slope: ${heapSlope === null || heapSlope === undefined ? '-' : `${Math.round(heapSlope / 1024)} KiB/min`}`);
    console.log(`cycle slopes: ${nodeSlope?.toFixed(1) ?? '-'} nodes/cycle, ${listenerSlope?.toFixed(1) ?? '-'} listeners/cycle`);
    const perCycle = (bytes) => (bytes === null || bytes === undefined ? '-' : `${(bytes / 1024).toFixed(1)} KiB/cycle`);
    console.log(`cycle slopes: renderer private ${perCycle(privateSlope)}, JS heap ${perCycle(cycleHeapSlope)}`);
    console.log(`max documents: ${report.summary.maxDocuments}`);
    // Written after printing, so a bad --out path still leaves the results on screen.
    if (options.out) {
      try {
        writeFileSync(options.out, `${JSON.stringify(report, null, 2)}\n`);
      } catch (error) {
        console.error(`measure-web-memory: cannot write ${options.out}: ${error.message}`);
        return 1;
      }
    }
    return 0;
  } finally {
    if (browser) {
      await browser.cdp.session().send('Browser.close').catch(() => {});
      browser.cdp.close();
      // Chrome normally exits on Browser.close; a hung one must not outlive the run.
      await Promise.race([browser.exited, sleep(5000)]);
      browser.chrome.kill();
    }
    child?.kill();
    // Windows can hold SQLite files for a moment after the process dies.
    await sleep(300);
    rmSync(dataDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    rmSync(profileDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  }
}

if (isMain(import.meta.url)) {
  try {
    process.exit(await main());
  } catch (error) {
    console.error(`measure-web-memory: ${error.message}`);
    process.exit(1);
  }
}
