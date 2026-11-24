/**
 * Something started that may be called off before its answer arrives: a request whose dialog
 * was cancelled, or whose inputs changed. `cancel()` stops `live` from running; it does not
 * undo what the server already did.
 */
export interface Cancellable {
  cancel(): void;
  /** Wraps `run` so it does nothing once cancelled. */
  live<A extends unknown[]>(run: (...args: A) => void): (...args: A) => void;
}

export function cancellable(): Cancellable {
  let cancelled = false;
  return {
    cancel() {
      cancelled = true;
    },
    live(run) {
      return (...args) => {
        if (!cancelled) run(...args);
      };
    },
  };
}
