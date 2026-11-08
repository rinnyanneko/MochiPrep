import { INSTRUMENTS, type SymbolId } from '@mochiprep/shared';

type Num = string | number | null | undefined;

const toNumber = (v: Num) => (v === null || v === undefined || v === '' ? NaN : Number(v));

export function money(v: Num, digits = 2): string {
  const n = toNumber(v);
  if (Number.isNaN(n)) return '–';
  return n.toLocaleString('en-US', { minimumFractionDigits: digits, maximumFractionDigits: digits });
}

export function signedMoney(v: Num, digits = 2): string {
  const n = toNumber(v);
  if (Number.isNaN(n)) return '–';
  return `${n > 0 ? '+' : n < 0 ? '−' : ''}${money(Math.abs(n), digits)}`;
}

export function pct(v: Num, digits = 2): string {
  const n = toNumber(v);
  if (Number.isNaN(n)) return '–';
  return `${n > 0 ? '+' : n < 0 ? '−' : ''}${Math.abs(n).toFixed(digits)}%`;
}

export function priceDigits(symbol: SymbolId): number {
  return INSTRUMENTS[symbol]?.priceTick.split('.')[1]?.length ?? 2;
}

export function price(v: Num, symbol: SymbolId): string {
  return money(v, priceDigits(symbol));
}

export function qty(v: Num): string {
  const n = toNumber(v);
  if (Number.isNaN(n)) return '–';
  return n.toLocaleString('en-US', { maximumFractionDigits: 6 });
}

export const shortId = (id: string | null | undefined) => (id ? id.replace(/^[a-z]+_/, '').slice(0, 8) : '–');

export const pnlClass = (v: Num) => {
  const n = toNumber(v);
  return n > 0 ? 'text-rise' : n < 0 ? 'text-fall' : 'text-ink-2';
};
