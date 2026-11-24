import { describe, expect, it } from 'vitest';
import { cancellable } from '../src/lib/cancellable';

describe('cancellable', () => {
  it('runs an answer that arrives before cancelling', () => {
    const seen: number[] = [];
    const request = cancellable();
    request.live((n: number) => seen.push(n))(1);
    expect(seen).toEqual([1]);
  });

  it('drops an answer that arrives after cancelling', async () => {
    // As a dialog closed while its request is in flight: the answer comes later.
    const seen: string[] = [];
    const request = cancellable();
    const answer = Promise.resolve('armed').then(request.live((v: string) => seen.push(v)));
    request.cancel();
    await answer;
    expect(seen).toEqual([]);
  });

  it('keeps each request apart, so only the current one answers', async () => {
    const seen: string[] = [];
    const older = cancellable();
    const newer = cancellable();
    const first = Promise.resolve('old range').then(older.live((v: string) => seen.push(v)));
    older.cancel();
    const second = Promise.resolve('new range').then(newer.live((v: string) => seen.push(v)));
    await Promise.all([first, second]);
    expect(seen).toEqual(['new range']);
  });
});
