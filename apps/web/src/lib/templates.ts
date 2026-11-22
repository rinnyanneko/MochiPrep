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

/** Identifies an exported template file, so an unrelated JSON file is refused with a clear message. */
export const TEMPLATE_FILE_FORMAT = 'mochiprep.agent-templates';
export const TEMPLATE_FILE_VERSION = 1;
/** Keeps a pasted or picked file from flooding the backend with requests. */
export const IMPORT_MAX_TEMPLATES = 100;
export const IMPORT_MAX_BYTES = 1_000_000;

export interface TemplateFile {
  format: typeof TEMPLATE_FILE_FORMAT;
  version: typeof TEMPLATE_FILE_VERSION;
  templates: AgentTemplateFields[];
}

/** The export document: only the saved settings, never ids, timestamps or the built-in flag. */
export function exportTemplates(templates: readonly AgentTemplate[]): string {
  const file: TemplateFile = { format: TEMPLATE_FILE_FORMAT, version: TEMPLATE_FILE_VERSION, templates: templates.map(templateBody) };
  return `${JSON.stringify(file, null, 2)}\n`;
}

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null && !Array.isArray(value);

/**
 * Reads an export document. The checks here only catch a wrong or damaged file early: the file is
 * untrusted, and the backend's strict template schema still validates every field of every template.
 * Unknown keys are dropped, and a missing description becomes empty, so a file from a newer version
 * that adds fields still imports.
 */
export function parseTemplateFile(text: string): AgentTemplateFields[] {
  if (text.length > IMPORT_MAX_BYTES) throw new Error('This file is too large to be a template export.');
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    throw new Error('This is not valid JSON.');
  }
  if (!isRecord(data) || data.format !== TEMPLATE_FILE_FORMAT) throw new Error('This is not a MochiPrep template export.');
  if (data.version !== TEMPLATE_FILE_VERSION) throw new Error(`Unsupported template export version: ${String(data.version)}.`);
  if (!Array.isArray(data.templates)) throw new Error('The export has no templates list.');
  if (data.templates.length === 0) throw new Error('The export contains no templates.');
  if (data.templates.length > IMPORT_MAX_TEMPLATES) throw new Error(`At most ${IMPORT_MAX_TEMPLATES} templates can be imported at once.`);
  return data.templates.map((item, index) => {
    if (!isRecord(item) || typeof item.name !== 'string' || !isRecord(item.risk)) throw new Error(`Template ${index + 1} is damaged.`);
    const fields = { ...item, description: typeof item.description === 'string' ? item.description : '' } as unknown as AgentTemplateFields;
    return templateBody(fields);
  });
}

/** Imported names get a copy suffix when they clash with an existing template or an earlier one in the file. */
export function importNames(incoming: readonly AgentTemplateFields[], existing: readonly AgentTemplate[]): AgentTemplateFields[] {
  const taken = existing.map((t) => t.name);
  return incoming.map((fields) => {
    const name = taken.includes(fields.name) ? copyName(fields.name, taken) : fields.name;
    taken.push(name);
    return { ...fields, name };
  });
}
