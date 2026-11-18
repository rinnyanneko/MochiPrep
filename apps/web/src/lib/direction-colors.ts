export const COLOR_PRESETS = ['red-green', 'green-red', 'red-blue', 'blue-green'] as const;
export type ColorPreset = typeof COLOR_PRESETS[number];
export type DirectionColors = { preset: ColorPreset } | { preset: 'custom'; rise: string; fall: string };
const hex = (value: unknown): value is string => typeof value === 'string' && /^#[0-9a-f]{6}$/i.test(value);

export function parseDirectionColors(raw: string | null, legacyFlip: string | null): DirectionColors {
  if (raw === null) return { preset: legacyFlip === '1' ? 'red-green' : 'green-red' };
  try {
    const value: unknown = JSON.parse(raw);
    if (value && typeof value === 'object' && 'preset' in value) {
      if (COLOR_PRESETS.includes(value.preset as ColorPreset)) return { preset: value.preset as ColorPreset };
      if (value.preset === 'custom' && 'rise' in value && 'fall' in value && hex(value.rise) && hex(value.fall)) {
        return { preset: 'custom', rise: value.rise, fall: value.fall };
      }
    }
  } catch { /* Older or hand-edited preferences fall back to the default. */ }
  return { preset: 'green-red' };
}

export function directionPalette(colors: DirectionColors, theme: 'light' | 'dark'): { rise: string; fall: string } {
  if (colors.preset === 'custom') return { rise: colors.rise, fall: colors.fall };
  const palette = theme === 'dark'
    ? { red: '#f58a9e', green: '#6fd6ae', blue: '#82b1ff' }
    : { red: '#c8324f', green: '#0d8a63', blue: '#2563eb' };
  const [rise, fall] = colors.preset.split('-') as [keyof typeof palette, keyof typeof palette];
  return { rise: palette[rise], fall: palette[fall] };
}

/** Pick the stronger WCAG contrast against the solid market color. */
export function directionTextColor(color: string): '#000000' | '#ffffff' {
  const [r, g, b] = [1, 3, 5].map((start) => {
    const channel = Number.parseInt(color.slice(start, start + 2), 16) / 255;
    return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  }) as [number, number, number];
  const luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  return (luminance + 0.05) / 0.05 >= 1.05 / (luminance + 0.05) ? '#000000' : '#ffffff';
}

export function applyDirectionColors(colors: DirectionColors, theme: 'light' | 'dark') {
  const root = document.documentElement;
  delete root.dataset.colors;
  const palette = directionPalette(colors, theme);
  for (const direction of ['rise', 'fall'] as const) {
    root.style.setProperty(`--${direction}`, palette[direction]);
    root.style.setProperty(`--${direction}-ink`, directionTextColor(palette[direction]));
    root.style.setProperty(`--${direction}-soft`, `color-mix(in srgb, ${palette[direction]} 16%, var(--surface-2))`);
  }
}
