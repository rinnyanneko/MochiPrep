import type { AgentTemplate, AgentTemplateFields } from '@mochiprep/shared';

/** The agent settings a template carries: everything in the template except its own name and description. */
export type AgentSettings = Omit<AgentTemplateFields, 'name' | 'description'>;

export const INTERVALS = [
  { value: 30, label: 'Every 30 seconds (demo pace)' },
  { value: 60, label: 'Every minute' },
  { value: 300, label: 'Every 5 minutes' },
  { value: 900, label: 'Every 15 minutes (default)' },
  { value: 3600, label: 'Every hour' },
];

/** The schedule choices, plus the current value when it is not one of them (a template can hold any). */
export function intervalOptions(current: number) {
  if (INTERVALS.some((i) => i.value === current)) return INTERVALS;
  const label = current % 60 === 0 ? `Every ${current / 60} minutes` : `Every ${current} seconds`;
  return [...INTERVALS, { value: current, label }].sort((a, b) => a.value - b.value);
}

/** The template's settings, as a patch for the agent form. Copies risk so form edits never touch the template. */
export function settingsFromTemplate(template: AgentTemplate): AgentSettings {
  return {
    strategy: template.strategy,
    timeframe: template.timeframe,
    intervalSec: template.intervalSec,
    executionMode: template.executionMode,
    risk: { ...template.risk },
    useNews: template.useNews,
    allowTrailingStop: template.allowTrailingStop,
  };
}

/** Exactly the fields the template routes accept, so no agent-only key reaches the strict schema. */
export function templateBody(fields: AgentTemplateFields): AgentTemplateFields {
  return {
    name: fields.name,
    description: fields.description,
    ...settingsFromTemplate({ ...fields, id: '', builtIn: false, createdAt: null, updatedAt: null }),
  };
}

const NAME_MAX = 60;

/** "Balanced (copy)", then "Balanced (copy 2)" and so on, kept within the 60-character name limit. */
export function copyName(name: string, taken: readonly string[]): string {
  const used = new Set(taken);
  // Code points, as the server counts the limit: a UTF-16 slice could split a surrogate pair.
  const chars = Array.from(name);
  for (let n = 1; ; n++) {
    const suffix = n === 1 ? ' (copy)' : ` (copy ${n})`;
    const candidate = chars.slice(0, NAME_MAX - suffix.length).join('').trimEnd() + suffix;
    if (!used.has(candidate)) return candidate;
  }
}

/** An editable copy of any template, built-in or not. */
export function duplicateTemplate(template: AgentTemplate, all: readonly AgentTemplate[]): AgentTemplateFields {
  return {
    ...templateBody(template),
    name: copyName(template.name, all.map((t) => t.name)),
  };
}
