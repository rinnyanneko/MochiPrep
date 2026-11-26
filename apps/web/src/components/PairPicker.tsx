import { Check } from 'lucide-react';
import type { SymbolId } from '@mochiprep/shared';
import { cn } from '@/lib/utils';

/** The backend refuses more: every pair adds its candles to the one prompt the agent sends. */
export const MAX_AGENT_PAIRS = 10;

/** Toggles pairs on and off, keeping the order they were picked in; the first is the agent's main pair. */
export function PairPicker({ options, value, onChange }: Readonly<{ options: SymbolId[]; value: SymbolId[]; onChange(next: SymbolId[]): void }>) {
  // A saved pair that is not offered here (an added pair while viewing replay) still shows, so it can be removed.
  const shown = [...options, ...value.filter((s) => !options.includes(s))];
  const full = value.length >= MAX_AGENT_PAIRS;
  return (
    <fieldset className="flex flex-wrap gap-1.5" data-testid="pair-picker">
      <legend className="sr-only">Pairs</legend>
      {shown.map((symbol) => {
        const on = value.includes(symbol);
        return (
          <button
            key={symbol}
            type="button"
            aria-pressed={on}
            disabled={!on && full}
            onClick={() => onChange(on ? value.filter((s) => s !== symbol) : [...value, symbol])}
            className={cn(
              'num inline-flex items-center gap-1 rounded-full border px-2.5 py-1 text-xs font-semibold transition-colors disabled:opacity-40',
              on ? 'border-accent bg-accent-soft text-accent' : 'border-line bg-surface-2 text-ink-2 hover:border-ink-3',
            )}
          >
            {on && <Check className="size-3" />}
            {symbol.replace(/USDT$/, '')}
          </button>
        );
      })}
    </fieldset>
  );
}
