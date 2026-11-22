import { describe, expect, it } from 'vitest';
import { DEFAULT_RISK_SETTINGS, type AgentTemplate } from '@mochiprep/shared';
import { copyName, duplicateTemplate, intervalOptions, settingsFromTemplate, templateBody } from '../src/lib/templates';

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
