import type { DecisionRecord } from '@mochiprep/shared';

/** The backend's limit on one message, in characters. */
export const MESSAGE_MAX = 1000;

export interface MessageReply {
  /** True when a cycle was already running: the message waits for the next one. */
  queued: boolean;
  record: DecisionRecord | null;
}

/** What happened to a sent message, in words. */
export function messageNote(reply: MessageReply): string {
  if (reply.queued || !reply.record) return 'A cycle is already running. Your message goes with the next one, together with any others still waiting.';
  if (reply.record.ownerMessage === undefined) {
    return `This cycle stopped before asking the model (${reply.record.outcome.toLowerCase().replaceAll('_', ' ')}). Your message goes with the next one.`;
  }
  return 'Sent. The decision it led to is in the list below.';
}
