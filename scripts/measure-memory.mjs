#!/usr/bin/env node
// Measures the resident memory of the Rust backend with the interface closed, so the release
// gate (<= 20 MiB working set on Windows x86_64, live OKX feed connected) is a number rather
// than a belief. No dependencies: it runs on any machine that can run the release.
//
//   node scripts/measure-memory.mjs --binary <path> [--port 8987] [--idle 12] [--out report.json]
//                                   [--pid <launcher pid>] [--gate]
//
// With --pid the backend was started by the launcher (not by this script): the script reads
// the token and feed from --port, reports the launcher's mochi-server child as the backend and
// adds the whole launcher process tree as `launcherTree`.
//
// The report's `target` is Node's `<platform>-<arch>` (win32-x64, linux-arm64, darwin-arm64...).
//
// Exit codes: 0 a number was read, 1 the backend did not start or nothing could be read,
// 2 (only with --gate) the win32-x64 gate failed.
import { execFileSync, spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { isMain } from './is-main.mjs';

export const GATE_MIB = 20;
// The 20 MiB budget was set and measured on Windows x86_64 only; Windows ARM64 is reported.
export const GATED_TARGET = 'win32-x64';
const READY_PREFIX = 'MochiPrep backend on http://127.0.0.1:';
const MIB = 1024 * 1024;

// ---- Parsers (pure, tested) -------------------------------------------------------------

/** `footprint <pid>` on macOS: the `phys_footprint` line, or the `Footprint:` summary line. */
export function parseFootprint(text) {
  const pattern = (label) => new RegExp(`${label}:\\s*([\\d.]+)\\s*(KB|MB|GB|bytes?)`, 'i');
  const match = pattern('phys_footprint').exec(text) ?? pattern('Footprint').exec(text);
  if (!match) throw new Error('no footprint line in output');
  const unit = match[2].toUpperCase();
  const scale = unit === 'KB' ? 1024 : unit === 'MB' ? MIB : unit === 'GB' ? 1024 * MIB : 1;
  return Math.round(Number(match[1]) * scale);
}

/** `/proc/<pid>/status` on Linux: `VmRSS:  12345 kB`. */
export function parseProcStatus(text) {
  const match = /^VmRSS:\s*(\d+)\s*kB/im.exec(text);
  if (!match) throw new Error('no VmRSS line in /proc status');
  return Number(match[1]) * 1024;
}

/** PowerShell `(Get-Process -Id N).WorkingSet64`: a bare integer. */
export function parseWorkingSet(text) {
  const trimmed = text.trim();
  if (!/^\d+$/.test(trimmed)) throw new Error(`not a working set integer: ${trimmed.slice(0, 80)}`);
  return Number(trimmed);
}

/** Reasons the win32-x64 gate fails; empty for every other target, where the number is only reported. */
export function gateFailures(report) {
  if (report.target !== GATED_TARGET) return [];
  const failures = [];
  if (report.mib > GATE_MIB) failures.push(`working set ${report.mib} MiB is over the ${GATE_MIB} MiB limit`);
  if (report.liveFeed !== 'connected') failures.push(`live OKX feed was ${report.liveFeed}, not connected`);
  return failures;
}

/** The OKX feed state in one SSE message ("data: {...}"), or undefined for anything else, malformed JSON included. */
export function liveFeedOf(message) {
  if (!message.startsWith('data: ')) return undefined;
  try {
    const event = JSON.parse(message.slice(6));
    if (event.type !== 'feed' || event.status?.source !== 'LIVE') return undefined;
    return String(event.status.state).toLowerCase();
  } catch {
    return undefined;
  }
}

/** The launcher's mochi-server descendant. Never the launcher itself: that would report its memory as the backend's. */
export function pickBackendPid(members, launcherPid, isBackend) {
  const found = members.find((member) => member !== launcherPid && isBackend(member));
  if (found === undefined) throw new Error("no mochi-server process under pid " + launcherPid);
  return found;
}

// ---- Sampling ---------------------------------------------------------------------------

const round = (value) => Math.round(value * 100) / 100;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function metricName() {
  return process.platform === 'win32' ? 'workingSet' : process.platform === 'darwin' ? 'footprint' : 'rss';
}

function powershell(command) {
  return execFileSync('powershell', ['-NoProfile', '-Command', command], { encoding: 'utf8' });
}

function sampleBytes(pid) {
  switch (process.platform) {
    case 'win32':
      return parseWorkingSet(powershell(`(Get-Process -Id ${pid}).WorkingSet64`));
    case 'darwin':
      return parseFootprint(execFileSync('footprint', [String(pid)], { encoding: 'utf8' }));
    default:
      return parseProcStatus(readFileSync(`/proc/${pid}/status`, 'utf8'));
  }
}

/** Direct children of every pid, as `Map<parent, pid[]>`. */
function childMap() {
  const map = new Map();
  const add = (parent, pid) => map.set(parent, [...(map.get(parent) ?? []), pid]);
  if (process.platform === 'win32') {
    const rows = JSON.parse(
      powershell('Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId | ConvertTo-Json -Compress'),
    );
    for (const row of [rows].flat()) add(row.ParentProcessId, row.ProcessId);
  } else if (process.platform === 'darwin') {
    const table = execFileSync('ps', ['-axo', 'pid=,ppid='], { encoding: 'utf8' });
    for (const line of table.split('\n')) {
      const [pid, ppid] = line.trim().split(/\s+/).map(Number);
      if (pid) add(ppid, pid);
    }
  } else {
    for (const entry of readdirSync('/proc')) {
      if (!/^\d+$/.test(entry)) continue;
      try {
        // The command name is parenthesised and may hold spaces, so split after the last ")".
        const stat = readFileSync(`/proc/${entry}/stat`, 'utf8');
        add(Number(stat.slice(stat.lastIndexOf(')') + 2).split(' ')[1]), Number(entry));
      } catch {
        // The process exited between listing and reading.
      }
    }
  }
  return map;
}

function treePids(root) {
  const children = childMap();
  const out = [];
  const queue = [root];
  while (queue.length) {
    const pid = queue.shift();
    out.push(pid);
    queue.push(...(children.get(pid) ?? []));
  }
  return out;
}

function launcherTree(pids) {
  let bytes = 0;
  let processes = 0;
  for (const member of pids) {
    try {
      bytes += sampleBytes(member);
      processes += 1;
    } catch {
      // A short-lived helper that exited mid-measurement does not count.
    }
  }
  return { bytes, mib: round(bytes / MIB), processes };
}

function isMochiServer(pid) {
  try {
    const name =
      process.platform === 'win32'
        ? powershell(`(Get-Process -Id ${pid}).ProcessName`)
        : execFileSync('ps', ['-o', 'comm=', '-p', String(pid)], { encoding: 'utf8' });
    return /mochi-server/i.test(name);
  } catch {
    return false;
  }
}

// ---- Driving the backend ----------------------------------------------------------------

function parseArgs(argv) {
  const options = { port: 8987, idle: 12, gate: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--gate') options.gate = true;
    else if (['--binary', '--port', '--idle', '--out', '--pid'].includes(arg)) options[arg.slice(2)] = argv[(i += 1)];
    else throw new Error(`unknown argument ${arg}`);
  }
  options.port = Number(options.port);
  options.idle = Number(options.idle);
  if (!options.binary && !options.pid) throw new Error('--binary <path> or --pid <launcher pid> is required');
  return options;
}

function startBackend(binary, port, dataDir) {
  // Inherited MOCHIPREP_* (a developer's offline or origin settings) would change what is measured.
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('MOCHIPREP_')));
  env.MOCHIPREP_PORT = String(port);
  env.MOCHIPREP_DATA_DIR = dataDir;
  const child = spawn(binary, [], { env, stdio: ['pipe', 'pipe', 'inherit'] });
  const ready = new Promise((resolve, reject) => {
    let buffer = '';
    child.stdout.on('data', (chunk) => {
      buffer += chunk;
      if (buffer.includes(READY_PREFIX)) resolve();
    });
    child.once('exit', (code) => reject(new Error(`backend exited with code ${code} before it was ready`)));
    setTimeout(() => reject(new Error('backend not ready after 30 s')), 30_000).unref();
  });
  return { child, ready };
}

/** Folds event-stream messages into the newest live-feed state; messages that say nothing keep it. */
export function latestFeedState(messages, previous = 'unknown') {
  let last = previous;
  for (const message of messages) last = liveFeedOf(message) ?? last;
  return last;
}

/**
 * Follows the event stream until `stop()`. `state()` is the newest OKX (LIVE) feed state seen, so a
 * drop during the idle wait reaches the report; `settled` resolves once the feed is connected, the
 * stream ends or 30 s pass. One deadline covers the session request and the stream.
 */
function watchLiveFeed(port) {
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), 30_000);
  let last = 'unknown';
  let markSettled;
  const settled = new Promise((resolve) => {
    markSettled = resolve;
  });
  const run = async () => {
    try {
      // No Origin header: Node's fetch sends none, which is what /api/session accepts.
      const session = await fetch(`http://127.0.0.1:${port}/api/session`, { signal: abort.signal });
      const { token } = await session.json();
      const response = await fetch(`http://127.0.0.1:${port}/api/events?token=${encodeURIComponent(token)}`, {
        signal: abort.signal,
      });
      const decoder = new TextDecoder();
      let buffer = '';
      for await (const chunk of response.body) {
        buffer += decoder.decode(chunk, { stream: true });
        const messages = [];
        let end;
        while ((end = buffer.indexOf('\n\n')) >= 0) {
          messages.push(buffer.slice(0, end));
          buffer = buffer.slice(end + 2);
        }
        // The first status is usually "connecting"; the report keeps the last one seen.
        last = latestFeedState(messages, last);
        if (last === 'connected') markSettled();
      }
    } catch {
      // A reset stream is a missing data point for the feed only; the backend is still measurable.
    } finally {
      clearTimeout(timer);
      markSettled();
    }
  };
  run();
  return {
    settled,
    state: () => last,
    stop: () => {
      clearTimeout(timer);
      abort.abort();
    },
  };
}

function gitCommit() {
  try {
    return execFileSync('git', ['rev-parse', '--short', 'HEAD'], { encoding: 'utf8' }).trim();
  } catch {
    return 'unknown';
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  let child;
  let dataDir;
  try {
    let pid = Number(options.pid);
    if (options.binary) {
      dataDir = mkdtempSync(join(tmpdir(), 'mochi-memory-'));
      const started = startBackend(options.binary, options.port, dataDir);
      child = started.child;
      await started.ready;
      pid = child.pid;
    }
    // Idle is measured with the interface closed: this script never opens a browser.
    // The stream stays open through the idle wait, so the report carries the state at the end.
    const feed = watchLiveFeed(options.port);
    await feed.settled;
    await sleep(options.idle * 1000);
    const liveFeed = feed.state();
    feed.stop();

    let backendPid = pid;
    let tree;
    if (options.pid) {
      const members = treePids(pid);
      tree = launcherTree(members);
      backendPid = pickBackendPid(members, pid, isMochiServer);
    }
    const bytes = sampleBytes(backendPid);
    const report = {
      target: `${process.platform}-${process.arch}`,
      metric: metricName(),
      bytes,
      mib: round(bytes / MIB),
      liveFeed,
      idleSeconds: options.idle,
      measuredAt: new Date().toISOString(),
      commit: gitCommit(),
      ...(tree ? { launcherTree: tree } : {}),
    };
    if (options.out) writeFileSync(options.out, `${JSON.stringify(report, null, 2)}\n`);
    console.log(`${report.target} ${report.metric}: ${report.mib} MiB, live feed ${report.liveFeed}, idle ${report.idleSeconds}s`);
    const failures = gateFailures(report);
    for (const failure of failures) console.error(`GATE FAIL: ${failure}`);
    return options.gate && failures.length ? 2 : 0;
  } finally {
    child?.kill();
    if (dataDir) {
      // Windows can hold SQLite files for a moment after the process dies.
      await sleep(300);
      rmSync(dataDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    }
  }
}

if (isMain(import.meta.url)) {
  main().then(
    (code) => process.exit(code),
    (error) => {
      console.error(`measure-memory: ${error.message}`);
      process.exit(1);
    },
  );
}
