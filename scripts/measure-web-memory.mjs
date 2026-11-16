#!/usr/bin/env node
// Measures the memory of the web interface while it is open, so "the UI uses less memory" can be
// checked against a number instead of a feeling. It drives the installed Chrome (the launcher's
// WebView2 is Chromium too, so the same heap and DOM counters apply) against the release backend,
// which serves the built UI itself, exactly as the launcher shows it.
//
//   pnpm build && cargo build --release --locked -p mochi-server
//   node scripts/measure-web-memory.mjs [--offline] [--source LIVE|REPLAY] [--soak 10] [--cycles 20]
//        [--port 8989] [--settle 3] [--sample-every 60] [--dwell 1] [--demo] [--snapshot file]
//        [--cycle-pages markets,agents]
//        [--binary path] [--out report.json]
//
// Scenario: sample after load on the Dashboard; visit every page, sampling each once settled; stay
// on Markets for --soak minutes, sampling every --sample-every seconds; cycle through every page
// --cycles times (or only the --cycle-pages listed), sampling every five cycles and at the end.
// Every sample forces a garbage collection first, so it counts what is retained, not what is
// merely waiting to be collected.
//
// --offline sets MOCHIPREP_OFFLINE=1 (replay only, no OKX or news) and defaults the source to
// REPLAY; otherwise the source defaults to LIVE. --demo presses "Try Demo Mode" after load so the
// pages have accounts, agents and charts to draw rather than empty states. --snapshot writes a V8
// heap snapshot at the start of the soak (as <name>-soak-start) and another at its end, so what
// the steady state holds, and what the soak adds, can be read.
//
// On Windows the working set and private bytes of the Chrome processes are read too (renderer,
// GPU, browser, utilities), since canvases and GPU textures do not show up in the JS heap.
//
// Exit codes: 0 measured, 1 the backend or browser did not start or a sample failed.
import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
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
  const valued = { '--source': 'source', '--soak': 'soak', '--cycles': 'cycles', '--port': 'port', '--settle': 'settle', '--sample-every': 'sampleEvery', '--dwell': 'dwell', '--out': 'out', '--binary': 'binary', '--snapshot': 'snapshot', '--cycle-pages': 'cyclePages' };
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

/** Where the soak-start snapshot goes: beside the end one, `x.heapsnapshot` -> `x-soak-start.heapsnapshot`. */
export function soakStartSnapshot(file) {
  const dot = file.lastIndexOf('.');
  const slash = Math.max(file.lastIndexOf('/'), file.lastIndexOf('\\'));
  return dot > slash ? `${file.slice(0, dot)}-soak-start${file.slice(dot)}` : `${file}-soak-start`;
}

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

  // Loaded only here, so the pure helpers above can be tested without a browser installed.
  const { chromium } = await import('@playwright/test');
  const dataDir = mkdtempSync(join(tmpdir(), 'mochi-web-memory-'));
  let child;
  let browser;
  try {
    const started = startBackend(binary, options.port, dataDir, options.offline);
    child = started.child;
    await started.ready;

    // Precise memory info, or performance.memory is rounded and bucketed to hide it from pages.
    browser = await chromium.launch({ channel: 'chrome', args: ['--enable-precise-memory-info'] });
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    // The source is read from localStorage once, at store creation, so it must be there before load.
    await context.addInitScript((source) => {
      try {
        localStorage.setItem('mochi.source', source);
      } catch {
        /* A refused storage write leaves the default source, which the report then shows. */
      }
    }, options.source);
    const page = await context.newPage();
    const session = await context.newCDPSession(page);
    await session.send('Performance.enable');
    await session.send('HeapProfiler.enable');
    const browserSession = await browser.newBrowserCDPSession();

    const started_at = Date.now();
    const samples = [];
    const sample = async (label, extra = {}) => {
      await session.send('HeapProfiler.collectGarbage');
      const { metrics } = await session.send('Performance.getMetrics');
      const perfMemory = await page.evaluate(() => {
        const m = performance.memory;
        return m ? { used: m.usedJSHeapSize, total: m.totalJSHeapSize } : null;
      });
      const entry = {
        label,
        page: await page.evaluate(() => location.hash.slice(1) || 'dashboard'),
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
    const go = async (name) => {
      // The app routes on hashchange; a reload would throw away exactly the state being measured.
      await page.evaluate((hash) => {
        location.hash = hash;
      }, name);
    };

    await page.goto(`http://127.0.0.1:${options.port}/#dashboard`, { waitUntil: 'load' });
    await sleep(options.settle * 1000);
    if (options.demo) {
      await page.getByTestId('try-demo').click({ timeout: 15_000 });
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
      }
    }

    const soak = samples.filter((s) => s.phase === 'soak');
    const cycles = samples.filter((s) => s.phase === 'cycles');
    const rendererOf = (s) => s.processes?.renderer?.workingSet;
    const withRenderer = (list) => list.map((s) => ({ ...s, rendererWorkingSet: rendererOf(s) }));
    const metrics = ['JSHeapUsedSize', 'Nodes', 'JSEventListeners', 'Documents', 'LayoutObjects', 'rendererWorkingSet'];
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
    if (options.out) writeFileSync(options.out, `${JSON.stringify(report, null, 2)}\n`);
    console.log('');
    console.log(formatTable(samples));
    const heapSlope = report.summary.soakPerMinute.JSHeapUsedSize?.slope;
    const nodeSlope = report.summary.cyclesPerCycle.Nodes?.slope;
    const listenerSlope = report.summary.cyclesPerCycle.JSEventListeners?.slope;
    console.log('');
    console.log(`soak heap slope: ${heapSlope === null || heapSlope === undefined ? '-' : `${Math.round(heapSlope / 1024)} KiB/min`}`);
    console.log(`cycle slopes: ${nodeSlope?.toFixed(1) ?? '-'} nodes/cycle, ${listenerSlope?.toFixed(1) ?? '-'} listeners/cycle`);
    console.log(`max documents: ${report.summary.maxDocuments}`);
    return 0;
  } finally {
    await browser?.close().catch(() => {});
    child?.kill();
    // Windows can hold SQLite files for a moment after the process dies.
    await sleep(300);
    rmSync(dataDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
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
