import assert from 'node:assert/strict';
import { test } from 'node:test';
import { gateFailures, latestFeedState, liveFeedOf, parseArgs, pickBackendPid, parseFootprint, parseProcStatus, parseWorkingSet, watchLiveFeed } from './measure-memory.mjs';

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
  assert.deepEqual(gateFailures({ ...win, mib: 30.2, liveFeed: 'connected' }), []);
  const over = gateFailures({ ...win, mib: 33, liveFeed: 'connected' });
  assert.equal(over.length, 1);
  assert.match(over[0], /32 MiB/);
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

// A stand-in for mochi-server's /api/session and /api/events: it reports the LIVE feed as
// connected, then either keeps the stream open or ends it, as a dropped network would.
async function fakeServer({ endStream }) {
  const { createServer } = await import('node:http');
  const server = createServer((req, res) => {
    if (req.url === '/api/session') {
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({ token: 't' }));
      return;
    }
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.write(`data: ${JSON.stringify({ type: 'feed', status: { source: 'LIVE', state: 'CONNECTED' } })}\n\n`);
    if (endStream) setTimeout(() => res.end(), 50);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return server;
}

test('watchLiveFeed keeps connected while the stream stays open', async () => {
  const server = await fakeServer({ endStream: false });
  const feed = watchLiveFeed(server.address().port);
  await feed.settled;
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(feed.state(), 'connected');
  feed.stop();
  server.closeAllConnections();
  server.close();
});

test('watchLiveFeed reports unknown once the stream ends before stop()', async () => {
  const server = await fakeServer({ endStream: true });
  const feed = watchLiveFeed(server.address().port);
  await feed.settled;
  await new Promise((resolve) => setTimeout(resolve, 150));
  // The last message said connected, but nothing watched the feed at sampling time.
  assert.equal(feed.state(), 'unknown');
  assert.ok(gateFailures({ target: 'win32-x64', metric: 'workingSet', mib: 19, liveFeed: feed.state() }).length > 0);
  feed.stop();
  server.close();
});

test('parseArgs refuses --binary with --pid and needs the launcher port with --pid', () => {
  assert.throws(() => parseArgs(['--binary', 'x', '--pid', '12', '--port', '8787']), /cannot be combined/);
  assert.throws(() => parseArgs(['--pid', '12']), /--port/);
  assert.equal(parseArgs(['--pid', '12', '--port', '8787']).port, 8787);
  assert.equal(parseArgs(['--binary', 'x']).port, 8987);
  assert.throws(() => parseArgs(['--binary', 'x', '--idle', 'abc']), /whole number/);
});
