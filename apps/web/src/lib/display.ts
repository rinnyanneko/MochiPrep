/**
 * Display preferences kept per browser. They change how times and directions look, never the
 * data: the API and the store keep Unix milliseconds and signed numbers.
 */

/** 'local' follows the browser, so a laptop that travels keeps showing its own clock. */
export type TimeZonePref = 'local' | 'UTC' | (string & {});
export type TickKind = 'year' | 'month' | 'day' | 'time' | 'seconds';

const formatters = new Map<string, Intl.DateTimeFormat>();

function formatter(zone: string | undefined): Intl.DateTimeFormat {
  const key = zone ?? '';
  let found = formatters.get(key);
  if (!found) {
    found = new Intl.DateTimeFormat('en-US', {
      timeZone: zone,
      hourCycle: 'h23',
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    });
    formatters.set(key, found);
  }
  return found;
}

const isKnownZone = (zone: string) => {
  try {
    formatter(zone);
    return true;
  } catch {
    return false;
  }
};

/** A stored value can come from an older browser or a hand edit, so anything unknown means local. */
export function parseTimeZone(raw: string | null): TimeZonePref {
  if (!raw || raw === 'local') return 'local';
  return isKnownZone(raw) ? raw : 'local';
}

export const parseColorFlip = (raw: string | null) => raw === '1';

export const resolveTimeZone = (pref: TimeZonePref) => (pref === 'local' ? Intl.DateTimeFormat().resolvedOptions().timeZone : pref);

/** Every zone this browser knows, for the Settings picker. */
export function listTimeZones(): string[] {
  try {
    return Intl.supportedValuesOf('timeZone');
  } catch {
    return [];
  }
}

interface Wall {
  year: string;
  month: string;
  day: string;
  hour: string;
  minute: string;
  second: string;
  offsetMinutes: number;
}

function wallClock(ts: number, pref: TimeZonePref): Wall {
  const parts: Record<string, string> = {};
  for (const part of formatter(pref === 'local' ? undefined : pref).formatToParts(ts)) parts[part.type] = part.value;
  const wall = { year: parts.year!, month: parts.month!, day: parts.day!, hour: parts.hour!, minute: parts.minute!, second: parts.second! };
  // The offset is whatever separates the zone's wall clock from UTC at this instant, which tracks DST.
  const asUtc = Date.UTC(Number(wall.year), Number(wall.month) - 1, Number(wall.day), Number(wall.hour), Number(wall.minute), Number(wall.second));
  const offsetMinutes = Math.round((asUtc - Math.floor(ts / 1000) * 1000) / 60_000);
  return { ...wall, offsetMinutes };
}

function offsetText(minutes: number, pref: TimeZonePref): string {
  // Z only for UTC itself: London in winter is +00:00, which says the offset can change.
  if (pref === 'UTC') return 'Z';
  const sign = minutes < 0 ? '-' : '+';
  const abs = Math.abs(minutes);
  return `${sign}${String(Math.floor(abs / 60)).padStart(2, '0')}:${String(abs % 60).padStart(2, '0')}`;
}

/** ISO 8601 to the minute with the zone offset, so a screenshot says which clock it shows. */
export function formatDateTime(ts: number | null | undefined, pref: TimeZonePref): string {
  if (!ts) return '–';
  const w = wallClock(ts, pref);
  return `${w.year}-${w.month}-${w.day}T${w.hour}:${w.minute}${offsetText(w.offsetMinutes, pref)}`;
}

/**
 * The chart picks tick positions on UTC boundaries. Off UTC, a "day" tick is not local midnight,
 * so it carries its time rather than a bare date that would read as the start of the local day.
 */
export function chartTickLabel(ts: number, kind: TickKind, pref: TimeZonePref): string {
  const w = wallClock(ts, pref);
  const hm = `${w.hour}:${w.minute}`;
  if (kind === 'time') return hm;
  if (kind === 'seconds') return `${hm}:${w.second}`;
  const midnight = hm === '00:00';
  if (kind === 'day' && midnight) return `${w.month}-${w.day}`;
  if (kind === 'month' && midnight && w.day === '01') return `${w.year}-${w.month}`;
  if (kind === 'year' && midnight && w.day === '01' && w.month === '01') return w.year;
  return kind === 'year' ? `${w.year}-${w.month}-${w.day}` : `${w.month}-${w.day} ${hm}`;
}
