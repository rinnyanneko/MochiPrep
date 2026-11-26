import { describe, expect, it } from 'vitest';
import type { DecisionRecord } from '@mochiprep/shared';
import { messageNote } from '../src/lib/agent-message';

const record = (fields: Partial<DecisionRecord>) => ({ outcome: 'SIGNAL', ...fields }) as DecisionRecord;

describe('messageNote', () => {
  it('says the message waits while a cycle runs', () => {
    expect(messageNote({ queued: true, record: null })).toMatch(/already running/);
  });
  it('says the message waits when the cycle stopped before the model', () => {
    expect(messageNote({ queued: false, record: record({ outcome: 'DATA_ERROR' }) })).toBe(
      'This cycle stopped before asking the model (data error). Your message goes with the next one.',
    );
  });
  it('says it was sent when the record carries it', () => {
    expect(messageNote({ queued: false, record: record({ ownerMessage: 'stay flat' }) })).toMatch(/^Sent/);
  });
});
