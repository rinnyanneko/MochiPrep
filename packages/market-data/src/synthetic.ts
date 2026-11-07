import { INSTRUMENTS, type Candle, type SymbolId } from '@mochiprep/shared';

/** 2026-01-01T00:00:00Z. The tape is synthetic, so this date means nothing about real markets. */
export const TAPE_START = Date.UTC(2026, 0, 1);
export const TAPE_MINUTES = 18 * 1440;
/** History shown before the first playable bar, so indicators are warm. */
export const WARMUP_MINUTES = 4 * 1440;

const SEEDS: Record<SymbolId, { seed: number; price: number; volume: number }> = {
  BTCUSDT: { seed: 0x6d6f6368, price: 68_000, volume: 42 },
  ETHUSDT: { seed: 0x69707265, price: 3_400, volume: 520 },
  SOLUSDT: { seed: 0x70303031, price: 150, volume: 6_400 },
};

/** Integer-only PRNG, so the tape is identical on every machine. */
function mulberry32(seed: number) {
  let a = seed;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Bundled demo data: a seeded random walk through trend, chop and crash regimes. */
export function generateTape(symbol: SymbolId, minutes = TAPE_MINUTES): Candle[] {
  const { seed, price: startPrice, volume: baseVolume } = SEEDS[symbol];
  const tick = Number(INSTRUMENTS[symbol].priceTick);
  const decimals = INSTRUMENTS[symbol].priceTick.split('.')[1]?.length ?? 0;
  const roundTick = (v: number) => Number((Math.round(v / tick) * tick).toFixed(decimals));
  const random = mulberry32(seed);
  const gaussian = () => random() + random() + random() + random() + random() + random() - 3;

  const candles: Candle[] = [];
  let price = startPrice;
  let drift = 0;
  let vol = 0.0005;
  let regimeLeft = 0;
  for (let i = 0; i < minutes; i++) {
    if (regimeLeft-- <= 0) {
      const pick = random();
      regimeLeft = 180 + Math.floor(random() * 900);
      if (pick < 0.3) [drift, vol] = [0.00006, 0.0006];
      else if (pick < 0.6) [drift, vol] = [-0.00006, 0.0007];
      else if (pick < 0.92) [drift, vol] = [0, 0.0004];
      else [drift, vol, regimeLeft] = [-0.00035, 0.0016, 120];
    }
    const open = price;
    let high = open;
    let low = open;
    for (let s = 0; s < 4; s++) {
      price *= 1 + drift / 4 + (gaussian() * vol) / 2;
      high = Math.max(high, price);
      low = Math.min(low, price);
    }
    const volume = baseVolume * (0.6 + random()) * (1 + (Math.abs(price - open) / open) * 900);
    candles.push({
      time: TAPE_START + i * 60_000,
      open: roundTick(open),
      high: roundTick(high),
      low: roundTick(low),
      close: roundTick(price),
      volume: Number(volume.toFixed(3)),
      closed: true,
    });
  }
  return candles;
}
