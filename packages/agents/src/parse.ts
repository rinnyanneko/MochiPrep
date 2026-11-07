import { AiDecisionSchema, type AiDecisionData } from '@mochiprep/shared';

export type ParseResult = { ok: true; decision: AiDecisionData } | { ok: false; error: string };

/** Accepts bare JSON or one fenced block. Everything else about the output must be exact. */
export function parseDecision(text: string): ParseResult {
  const trimmed = text.trim();
  const fenced = /^```(?:json)?\s*([\s\S]*?)\s*```$/i.exec(trimmed);
  let value: unknown;
  try {
    value = JSON.parse(fenced ? fenced[1]! : trimmed);
  } catch {
    return { ok: false, error: 'Response is not valid JSON' };
  }
  const result = AiDecisionSchema.safeParse(value);
  if (result.success) return { ok: true, decision: result.data };
  const issues = result.error.issues.map((i) => `${i.path.join('.') || 'root'}: ${i.message}`);
  return { ok: false, error: issues.join('; ') };
}
