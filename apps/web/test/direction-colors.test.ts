import { describe, expect, it } from 'vitest';
import { parseDirectionColors, directionPalette, directionTextColor } from '../src/lib/direction-colors';

describe('direction color preferences', () => {
  it('migrates the old flip setting only when the new preference is absent', () => {
    expect(parseDirectionColors(null, '1')).toEqual({ preset: 'red-green' });
    expect(parseDirectionColors(null, '0')).toEqual({ preset: 'green-red' });
    expect(parseDirectionColors('{"preset":"blue-green"}', '1')).toEqual({ preset: 'blue-green' });
  });
  it('rejects malformed storage and CSS values', () => {
    for (const raw of ['null', '[]', '{', '{"preset":"wat"}', '{"preset":"custom","rise":"red","fall":"#00ff00"}']) {
      expect(parseDirectionColors(raw, null)).toEqual({ preset: 'green-red' });
    }
    expect(parseDirectionColors('{"preset":"custom","rise":"#123abc","fall":"#Ff0000"}', null))
      .toEqual({ preset: 'custom', rise: '#123abc', fall: '#Ff0000' });
  });
  it('keeps custom colors across themes and adapts presets', () => {
    const custom = parseDirectionColors('{"preset":"custom","rise":"#123abc","fall":"#ff0000"}', null);
    expect(directionPalette(custom, 'light')).toEqual(directionPalette(custom, 'dark'));
    expect(directionPalette({ preset: 'red-green' }, 'dark').rise).toBe('#f58a9e');
    expect(directionPalette({ preset: 'red-blue' }, 'light').fall).toBe('#2563eb');
    expect(directionPalette({ preset: 'blue-green' }, 'dark').fall).toBe('#6fd6ae');
  });
});

it('picks contrasting button text for a custom color in either theme', () => {
  expect(directionTextColor('#6fd6ae')).toBe('#000000');
  expect(directionTextColor('#f58a9e')).toBe('#000000');
  expect(directionTextColor('#29203d')).toBe('#ffffff');
});
