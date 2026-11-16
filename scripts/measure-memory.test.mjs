import assert from 'node:assert/strict';
import { test } from 'node:test';
import { gateFailures, parseFootprint, parseProcStatus, parseWorkingSet } from './measure-memory.mjs';

test('parseFootprint reads the phys_footprint line in KB and MB', () => {
  const kb = 'Footprint: 14 MB\n\n  Dirty  Clean\n\nmochi-server [123]: 64-bit    Footprint: 14 MB (16384 bytes per page)\n';
  assert.equal(parseFootprint(kb), 14 * 1024 * 1024);
  assert.equal(parseFootprint('phys_footprint: 8192 KB\n'), 8192 * 1024);
  assert.equal(parseFootprint('phys_footprint: 12.5 MB\n'), 12.5 * 1024 * 1024);
  assert.throws(() => parseFootprint('nothing here'));
});

test('parseProcStatus reads VmRSS in kB', () => {
  const sample = 'Name:\tmochi-server\nVmPeak:\t  99999 kB\nVmRSS:\t  12345 kB\nThreads:\t4\n';
  assert.equal(parseProcStatus(sample), 12345 * 1024);
  assert.throws(() => parseProcStatus('Name:\tx\n'));
});

test('parseWorkingSet reads the PowerShell integer', () => {
  assert.equal(parseWorkingSet('19333120\r\n'), 19333120);
  assert.throws(() => parseWorkingSet('Get-Process : Cannot find a process'));
});

test('gateFailures', () => {
  assert.deepEqual(gateFailures({ metric: 'workingSet', mib: 18.4, liveFeed: 'connected' }), []);
  const over = gateFailures({ metric: 'workingSet', mib: 21, liveFeed: 'connected' });
  assert.equal(over.length, 1);
  assert.match(over[0], /20 MiB/);
  const feed = gateFailures({ metric: 'workingSet', mib: 18.4, liveFeed: 'degraded' });
  assert.equal(feed.length, 1);
  assert.match(feed[0], /degraded/);
  // Other OSes only report.
  assert.deepEqual(gateFailures({ metric: 'rss', mib: 90, liveFeed: 'connected' }), []);
});
