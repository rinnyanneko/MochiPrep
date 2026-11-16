import assert from 'node:assert/strict';
import { test } from 'node:test';
import { gateFailures, latestFeedState, liveFeedOf, pickBackendPid, parseFootprint, parseProcStatus, parseWorkingSet } from './measure-memory.mjs';

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
  const win = { target: 'win32-x64', metric: 'workingSet' };
  assert.deepEqual(gateFailures({ ...win, mib: 18.4, liveFeed: 'connected' }), []);
  const over = gateFailures({ ...win, mib: 21, liveFeed: 'connected' });
  assert.equal(over.length, 1);
  assert.match(over[0], /20 MiB/);
  const feed = gateFailures({ ...win, mib: 18.4, liveFeed: 'degraded' });
  assert.equal(feed.length, 1);
  assert.match(feed[0], /degraded/);
  // Every other target only reports, Windows ARM64 included.
  assert.deepEqual(gateFailures({ target: 'win32-arm64', metric: 'workingSet', mib: 30, liveFeed: 'degraded' }), []);
  assert.deepEqual(gateFailures({ target: 'linux-x64', metric: 'rss', mib: 90, liveFeed: 'connected' }), []);
});

test('liveFeedOf ignores malformed and unrelated messages', () => {
  const feed = (source, state) => `data: ${JSON.stringify({ type: 'feed', status: { source, state } })}`;
  assert.equal(liveFeedOf(feed('LIVE', 'CONNECTED')), 'connected');
  assert.equal(liveFeedOf(feed('LIVE', 'DEGRADED')), 'degraded');
  assert.equal(liveFeedOf(feed('REPLAY', 'OFFLINE_REPLAY')), undefined);
  assert.equal(liveFeedOf('data: {not json'), undefined);
  assert.equal(liveFeedOf(': heartbeat'), undefined);
});

test('pickBackendPid refuses to fall back to the launcher', () => {
  assert.equal(pickBackendPid([10, 11, 12], 10, (pid) => pid === 12), 12);
  assert.throws(() => pickBackendPid([10, 11], 10, () => false), /no mochi-server/);
});

test('latestFeedState reports the last state, so a later drop wins', () => {
  const feed = (status) => `data: ${JSON.stringify({ type: 'feed', status: { source: 'LIVE', state: status } })}`;
  assert.equal(latestFeedState([feed('CONNECTING'), feed('CONNECTED')]), 'connected');
  assert.equal(latestFeedState([feed('CONNECTED'), ': heartbeat', feed('DEGRADED')]), 'degraded');
  assert.equal(latestFeedState([': heartbeat'], 'connected'), 'connected');
  assert.equal(latestFeedState([]), 'unknown');
});
