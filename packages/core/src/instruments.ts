import { INSTRUMENTS, type InstrumentSpecification, type SymbolId } from '@mochiprep/shared';

export function getInstrument(symbol: SymbolId): InstrumentSpecification {
  const spec = INSTRUMENTS[symbol];
  if (!spec) throw new Error(`Unknown instrument ${symbol}`);
  return spec;
}
