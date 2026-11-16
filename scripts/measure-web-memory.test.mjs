import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DEFAULT_PORT, formatTable, groupProcesses, growth, mib, parseArgs, parseProcessJson, pickMetrics, slope, soakStartSnapshot } from './measure-web-memory.mjs';

test('parseArgs defaults: live source, port 8989, 10 minute soak, 20 cycles', () => {
  const options = parseArgs([]);
  assert.equal(options.source, 'LIVE');
  assert.equal(options.port, DEFAULT_PORT);
  assert.equal(options.soak, 10);
  assert.equal(options.cycles, 20);
  assert.equal(options.offline, false);
});

test('parseArgs: offline defaults to replay, an explicit source wins', () => {
  assert.equal(parseArgs(['--offline']).source, 'REPLAY');
  assert.equal(parseArgs(['--offline', '--source', 'live']).source, 'LIVE');
  assert.equal(parseArgs(['--soak', '0.5', '--cycles', '3']).soak, 0.5);
});

test('parseArgs refuses reserved ports and bad values', () => {
  assert.throws(() => parseArgs(['--port', '8787']), /launcher/);
  assert.throws(() => parseArgs(['--port', '8877']), /launcher/);
  assert.throws(() => parseArgs(['--source', 'OKX']), /LIVE or REPLAY/);
  assert.throws(() => parseArgs(['--cycles', '2.5']), /whole/);
  assert.throws(() => parseArgs(['--soak', '-1']), /non-negative/);
  assert.throws(() => parseArgs(['--sample-every', '0']), /above zero/);
  assert.throws(() => parseArgs(['--out']), /needs a value/);
  assert.throws(() => parseArgs(['--nope']), /unknown/);
});

test('slope is the least-squares fit and null without two distinct x', () => {
  assert.equal(slope([{ x: 0, y: 1 }, { x: 1, y: 3 }, { x: 2, y: 5 }]), 2);
  assert.equal(slope([{ x: 0, y: 4 }, { x: 1, y: 4 }]), 0);
  // Noise around a flat line fits to (almost) zero rather than to the last minus the first.
  assert.ok(Math.abs(slope([{ x: 0, y: 10 }, { x: 1, y: 12 }, { x: 2, y: 8 }, { x: 3, y: 10 }])) < 1);
  assert.equal(slope([{ x: 1, y: 1 }]), null);
  assert.equal(slope([{ x: 1, y: 1 }, { x: 1, y: 2 }]), null);
});

test('growth reports first, last, delta and slope, skipping samples without the metric', () => {
  const samples = [{ m: 0, Nodes: 100 }, { m: 1, Nodes: 110 }, { m: 2 }, { m: 3, Nodes: 130 }];
  const result = growth(samples, 'Nodes', (s) => s.m);
  assert.deepEqual({ ...result, slope: Math.round(result.slope) }, { first: 100, last: 130, delta: 30, slope: 10 });
  assert.equal(growth([{ m: 0 }], 'Nodes', (s) => s.m), null);
});

test('parseProcessJson reads one object, an array, or nothing', () => {
  assert.equal(parseProcessJson('').size, 0);
  const one = parseProcessJson('{"Id":12,"WorkingSet64":2048,"PrivateMemorySize64":1024}');
  assert.deepEqual(one.get(12), { workingSet: 2048, privateBytes: 1024 });
  const many = parseProcessJson('[{"Id":1,"WorkingSet64":1,"PrivateMemorySize64":2},{"Id":2,"WorkingSet64":3,"PrivateMemorySize64":4}]\r\n');
  assert.equal(many.size, 2);
});

test('groupProcesses sums by type and leaves out processes with no reading', () => {
  const info = [
    { id: 1, type: 'browser' },
    { id: 2, type: 'renderer' },
    { id: 3, type: 'renderer' },
    { id: 4, type: 'gpu' },
    { id: 5, type: 'utility' },
  ];
  const memory = new Map([
    [1, { workingSet: 10, privateBytes: 5 }],
    [2, { workingSet: 20, privateBytes: 6 }],
    [3, { workingSet: 30, privateBytes: 7 }],
    [4, { workingSet: 40, privateBytes: 8 }],
  ]);
  const groups = groupProcesses(info, memory);
  assert.deepEqual(groups.renderer, { count: 2, workingSet: 50, privateBytes: 13, pids: [2, 3] });
  assert.equal(groups.utility, undefined);
  assert.equal(groups.gpu.workingSet, 40);
});

test('pickMetrics keeps only the reported CDP metrics', () => {
  const picked = pickMetrics([{ name: 'Nodes', value: 5 }, { name: 'TaskDuration', value: 1 }, { name: 'JSHeapUsedSize', value: 9 }]);
  assert.deepEqual(picked, { Nodes: 5, JSHeapUsedSize: 9 });
});

test('mib rounds to two decimals and passes missing values through as null', () => {
  assert.equal(mib(1024 * 1024 * 1.234), 1.23);
  assert.equal(mib(undefined), null);
});

test('formatTable prints a header and one row per sample, dashes for missing values', () => {
  const table = formatTable([{ label: 'load:dashboard', JSHeapUsedSize: 2 * 1024 * 1024, Nodes: 900, processes: {} }]);
  const lines = table.split('\n');
  assert.equal(lines.length, 2);
  assert.match(lines[0], /^label/);
  assert.match(lines[1], /^load:dashboard\s+2\s+-\s+900/);
});

test('parseArgs --cycle-pages takes a subset of the pages and refuses unknown ones', () => {
  assert.deepEqual(parseArgs(['--cycle-pages', 'markets, agents']).cyclePages, ['markets', 'agents']);
  assert.equal(parseArgs([]).cyclePages.length, 7);
  assert.throws(() => parseArgs(['--cycle-pages', 'markets,nope']), /comma list/);
  assert.throws(() => parseArgs(['--cycle-pages', ',']), /comma list/);
});

test('soakStartSnapshot names the first snapshot beside the second', () => {
  assert.equal(soakStartSnapshot('out/live.heapsnapshot'), 'out/live-soak-start.heapsnapshot');
  assert.equal(soakStartSnapshot(String.raw`C:\a.b\live`), String.raw`C:\a.b\live-soak-start`);
});
