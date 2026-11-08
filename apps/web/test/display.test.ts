import { describe, expect, it } from 'vitest';
import { chartTickLabel, formatDateTime, parseColorFlip, parseTimeZone, resolveTimeZone } from '../src/lib/display';

// 2026-03-08 06:30 UTC, an hour before New York's spring-forward instant (07:00 UTC).
const TS = Date.UTC(2026, 2, 8, 6, 30);

describe('formatDateTime', () => {
  it('writes UTC as ISO 8601 with a Z', () => {
    expect(formatDateTime(TS, 'UTC')).toBe('2026-03-08T06:30Z');
  });

  it('writes a named zone with its offset', () => {
    expect(formatDateTime(TS, 'Asia/Taipei')).toBe('2026-03-08T14:30+08:00');
    expect(formatDateTime(TS, 'Asia/Kolkata')).toBe('2026-03-08T12:00+05:30');
  });

  it('follows daylight saving per timestamp', () => {
    expect(formatDateTime(TS, 'America/New_York')).toBe('2026-03-08T01:30-05:00');
    expect(formatDateTime(TS + 3_600_000, 'America/New_York')).toBe('2026-03-08T03:30-04:00');
  });

  it('writes +00:00 for a zone that is only at UTC for now', () => {
    expect(formatDateTime(Date.UTC(2026, 0, 15, 9, 5), 'Europe/London')).toBe('2026-01-15T09:05+00:00');
  });

  it('shows midnight as 00, not 24', () => {
    expect(formatDateTime(Date.UTC(2026, 0, 1), 'UTC')).toBe('2026-01-01T00:00Z');
  });

  it('uses the browser zone for local', () => {
    const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    expect(formatDateTime(TS, 'local')).toBe(formatDateTime(TS, zone === 'UTC' ? 'UTC' : zone).replace(/Z$/, '+00:00'));
  });

  it('shows a dash for a missing time', () => {
    expect(formatDateTime(null, 'UTC')).toBe('–');
    expect(formatDateTime(undefined, 'UTC')).toBe('–');
    expect(formatDateTime(0, 'UTC')).toBe('–');
  });
});

describe('chartTickLabel', () => {
  it('labels time ticks with the wall clock of the zone', () => {
    expect(chartTickLabel(TS, 'time', 'Asia/Taipei')).toBe('14:30');
    expect(chartTickLabel(TS + 15_000, 'seconds', 'UTC')).toBe('06:30:15');
  });

  it('labels a day tick by date when it falls on local midnight', () => {
    expect(chartTickLabel(Date.UTC(2026, 2, 8), 'day', 'UTC')).toBe('03-08');
  });

  it('adds the time when the UTC day boundary is not local midnight', () => {
    expect(chartTickLabel(Date.UTC(2026, 2, 8), 'day', 'Asia/Taipei')).toBe('03-08 08:00');
    expect(chartTickLabel(Date.UTC(2026, 2, 1), 'month', 'America/New_York')).toBe('02-28 19:00');
  });

  it('labels month and year ticks at local midnight', () => {
    expect(chartTickLabel(Date.UTC(2026, 2, 1), 'month', 'UTC')).toBe('2026-03');
    expect(chartTickLabel(Date.UTC(2026, 0, 1), 'year', 'UTC')).toBe('2026');
  });
});

describe('preferences', () => {
  it('accepts local, UTC and real IANA zones', () => {
    expect(parseTimeZone(null)).toBe('local');
    expect(parseTimeZone('local')).toBe('local');
    expect(parseTimeZone('UTC')).toBe('UTC');
    expect(parseTimeZone('Asia/Taipei')).toBe('Asia/Taipei');
  });

  it('falls back to local for a zone this browser does not know', () => {
    expect(parseTimeZone('Mars/Olympus_Mons')).toBe('local');
    expect(parseTimeZone('')).toBe('local');
  });

  it('resolves local to the browser zone', () => {
    expect(resolveTimeZone('local')).toBe(Intl.DateTimeFormat().resolvedOptions().timeZone);
    expect(resolveTimeZone('Asia/Tokyo')).toBe('Asia/Tokyo');
  });

  it('flips colors only when explicitly stored as on', () => {
    expect(parseColorFlip('1')).toBe(true);
    expect(parseColorFlip('0')).toBe(false);
    expect(parseColorFlip(null)).toBe(false);
    expect(parseColorFlip('yes')).toBe(false);
  });
});
