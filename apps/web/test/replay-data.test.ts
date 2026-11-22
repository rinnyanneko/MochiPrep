import { describe, expect, it } from 'vitest';
import { formatBytes, formatDuration } from '../src/lib/estimates';

describe('replay data estimates', () => {
  it('names sizes in the unit people read', () => {
    expect(formatBytes(57 * 1440)).toBe('80 KB');
    expect(formatBytes(57 * 1440 * 10 * 18)).toBe('14 MB');
    expect(formatBytes(3.2 * 1024 ** 2)).toBe('3.2 MB');
    expect(formatBytes(2.5 * 1024 ** 3)).toBe('2.5 GB');
  });

  it('rounds a download time to what is worth saying', () => {
    expect(formatDuration(20)).toBe('under a minute');
    expect(formatDuration(595)).toBe('about 10 min');
    expect(formatDuration(3 * 3600 + 25 * 60)).toBe('about 3 h 25 min');
  });
});
