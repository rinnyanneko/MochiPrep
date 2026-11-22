import { describe, expect, it } from 'vitest';
import { DEFAULT_RISK_SETTINGS, type AgentTemplate } from '@mochiprep/shared';
import { copyName, duplicateTemplate, exportTemplates, importNames, IMPORT_MAX_TEMPLATES, intervalOptions, parseTemplateFile, settingsFromTemplate, templateBody, TEMPLATE_FILE_FORMAT } from '../src/lib/templates';

const template: AgentTemplate = {
  id: 'builtin_balanced',
  name: 'Balanced',
  description: 'Pullbacks',
  builtIn: true,
  strategy: 'Trade pullbacks',
  timeframe: '15m',
  intervalSec: 900,
  executionMode: 'SIGNAL',
  risk: { ...DEFAULT_RISK_SETTINGS, maxLeverage: 2 },
  useNews: false,
  allowTrailingStop: true,
  createdAt: null,
  updatedAt: null,
};

describe('agent templates', () => {
  it('pre-fills only the agent settings, with a copy of the risk limits', () => {
    const settings = settingsFromTemplate(template);
    expect(settings).toEqual({
      strategy: 'Trade pullbacks',
      timeframe: '15m',
      intervalSec: 900,
      executionMode: 'SIGNAL',
      risk: template.risk,
      useNews: false,
      allowTrailingStop: true,
    });
    expect(settings.risk).not.toBe(template.risk);
    expect(settings).not.toHaveProperty('name');
  });

  it('sends only the fields the strict schema accepts', () => {
    expect(Object.keys(templateBody(template)).sort()).toEqual(
      ['allowTrailingStop', 'description', 'executionMode', 'intervalSec', 'name', 'risk', 'strategy', 'timeframe', 'useNews'],
    );
  });

  it('names copies uniquely within the length limit', () => {
    expect(copyName('Balanced', ['Balanced'])).toBe('Balanced (copy)');
    expect(copyName('Balanced', ['Balanced (copy)'])).toBe('Balanced (copy 2)');
    const long = 'x'.repeat(60);
    expect(copyName(long, []).length).toBeLessThanOrEqual(60);
    // The server counts code points, and a cut must never leave half of a surrogate pair.
    const emoji = `${'x'.repeat(52)}😀😀`;
    const copied = copyName(emoji, []);
    expect(copied).toBe(`${'x'.repeat(52)}😀 (copy)`);
    expect([...copied].length).toBeLessThanOrEqual(60);
    expect(copied).not.toMatch(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/);
    expect(duplicateTemplate(template, [template]).name).toBe('Balanced (copy)');
  });

  it('keeps an unusual schedule selectable', () => {
    expect(intervalOptions(900).some((i) => i.value === 900)).toBe(true);
    const options = intervalOptions(120);
    expect(options.find((i) => i.value === 120)?.label).toBe('Every 2 minutes');
    expect(options.map((o) => o.value)).toEqual([...options.map((o) => o.value)].sort((a, b) => a - b));
  });
});

describe('template export and import', () => {
  const mine: AgentTemplate = { ...template, id: 'tpl_1', name: 'Mine', builtIn: false, createdAt: 1, updatedAt: 2 };

  it('round-trips settings without ids or timestamps', () => {
    const text = exportTemplates([template, mine]);
    expect(text).not.toContain('tpl_1');
    expect(text).not.toContain('builtIn');
    const parsed = parseTemplateFile(text);
    expect(parsed).toEqual([templateBody(template), templateBody(mine)]);
  });

  it('refuses files that are not template exports', () => {
    expect(() => parseTemplateFile('nope')).toThrow('not valid JSON');
    expect(() => parseTemplateFile('{"templates":[]}')).toThrow('not a MochiPrep template export');
    expect(() => parseTemplateFile(JSON.stringify({ format: TEMPLATE_FILE_FORMAT, version: 9, templates: [] }))).toThrow('Unsupported');
    expect(() => parseTemplateFile(JSON.stringify({ format: TEMPLATE_FILE_FORMAT, version: 1, templates: [] }))).toThrow('no templates');
    expect(() => parseTemplateFile(JSON.stringify({ format: TEMPLATE_FILE_FORMAT, version: 1, templates: [{ name: 1 }] }))).toThrow('Template 1 is damaged');
    const many = Array.from({ length: IMPORT_MAX_TEMPLATES + 1 }, () => templateBody(mine));
    expect(() => parseTemplateFile(JSON.stringify({ format: TEMPLATE_FILE_FORMAT, version: 1, templates: many }))).toThrow('At most');
  });

  it('drops unknown keys so the strict schema sees only template fields', () => {
    const text = JSON.stringify({ format: TEMPLATE_FILE_FORMAT, version: 1, templates: [{ ...templateBody(mine), providerId: 'mock', id: 'x', description: undefined }] });
    const [parsed] = parseTemplateFile(text);
    expect(parsed).not.toHaveProperty('providerId');
    expect(parsed).not.toHaveProperty('id');
    expect(parsed?.description).toBe('');
  });

  it('renames clashing imports', () => {
    const incoming = [templateBody(mine), templateBody(mine), { ...templateBody(mine), name: 'Fresh' }];
    expect(importNames(incoming, [mine]).map((t) => t.name)).toEqual(['Mine (copy)', 'Mine (copy 2)', 'Fresh']);
  });
});
