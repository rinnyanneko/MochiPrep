import { useCallback, useState } from 'react';

const PREFIX = 'mochiprep.view.';

/** A stored value, or `fallback` when none is stored, it cannot be read, or `valid` rejects it. */
export function readRemembered<T>(key: string, fallback: T, valid: (value: unknown) => value is T): T {
  try {
    const text = localStorage.getItem(PREFIX + key);
    if (text === null) return fallback;
    const value: unknown = JSON.parse(text);
    return valid(value) ? value : fallback;
  } catch {
    return fallback;
  }
}

export function writeRemembered(key: string, value: unknown) {
  try {
    localStorage.setItem(PREFIX + key, JSON.stringify(value));
  } catch {
    /* A view option is a convenience; private windows may refuse to store it. */
  }
}

/**
 * `useState` for a view option (a pair, a timeframe, a tab, a filter) that should still be set
 * after leaving the page and coming back, or after a reload. `valid` guards against a stored
 * value an older or newer version wrote. Never use it for what is typed into a form.
 */
export function useRemembered<T>(key: string, fallback: T, valid: (value: unknown) => value is T): [T, (value: T) => void] {
  const [value, setValue] = useState(() => readRemembered(key, fallback, valid));
  const set = useCallback(
    (next: T) => {
      setValue(next);
      writeRemembered(key, next);
    },
    [key],
  );
  return [value, set];
}

/** A `valid` for a value that must be one of `options`. */
export const oneOf =
  <T extends string | number | boolean>(options: readonly T[]) =>
  (value: unknown): value is T =>
    options.includes(value as T);

export const isString = (value: unknown): value is string => typeof value === 'string';
export const isBoolean = (value: unknown): value is boolean => typeof value === 'boolean';
export const isInteger = (value: unknown): value is number => Number.isInteger(value);
