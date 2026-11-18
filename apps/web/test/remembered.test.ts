import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { oneOf, readRemembered, writeRemembered } from '../src/lib/remembered';

const timeframes = oneOf(['1m', '5m', '15m'] as const);

describe('remembered view options', () => {
  let stored: Map<string, string>;
  beforeEach(() => {
    stored = new Map();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => stored.get(key) ?? null,
      setItem: (key: string, value: string) => void stored.set(key, value),
    });
  });
  afterEach(() => vi.unstubAllGlobals());

  it('reads back what was written', () => {
    writeRemembered('markets.timeframe', '1m');
    expect(readRemembered('markets.timeframe', '15m', timeframes)).toBe('1m');
  });

  it('falls back when nothing, something invalid or unreadable JSON is stored', () => {
    expect(readRemembered('markets.timeframe', '15m', timeframes)).toBe('15m');
    writeRemembered('markets.timeframe', '3d');
    expect(readRemembered('markets.timeframe', '15m', timeframes)).toBe('15m');
    stored.set('mochiprep.view.markets.timeframe', '{');
    expect(readRemembered('markets.timeframe', '15m', timeframes)).toBe('15m');
  });

  it('works without storage, as in a private window', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
    });
    expect(() => writeRemembered('markets.timeframe', '1m')).not.toThrow();
    expect(readRemembered('markets.timeframe', '15m', timeframes)).toBe('15m');
  });
});
