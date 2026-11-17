import type { IChartApi } from 'lightweight-charts';

/**
 * A chart that lives for the whole session instead of one page visit.
 *
 * Chromium does not give back the native memory (Oilpan, partition_alloc, a GPU transfer buffer)
 * behind a chart's canvases when the chart is destroyed: each create-and-remove left 0.1 to 0.2
 * MiB of renderer memory behind for good, so touring the pages grew the renderer without bound.
 * So each chart role (the Markets chart, the Trading chart, the equity chart) has one instance,
 * drawn into a `div` of its own that a mounted component borrows: the div goes into the
 * component's host on mount and comes out again on unmount. The chart is never `remove()`d.
 *
 * While the div is detached the chart's autoSize observer sees a zero size and shrinks its
 * canvases to nothing, so a hidden chart holds no backing stores. Borrowers clear the data on
 * return, so the next mount starts as empty as a newly created chart would.
 */
export interface Lease<Slot> {
  slot: Slot;
  release: () => void;
}

export class ChartRole<Slot extends { chart: IChartApi }> {
  private kept: { element: HTMLDivElement; slot: Slot } | null = null;
  private busy = false;

  /** `build` creates the chart and its series in the element it is given; it runs once. */
  constructor(private readonly build: (element: HTMLDivElement) => Slot) {}

  /** Puts this role's chart into `host`, creating it on first use. */
  take(host: HTMLElement): Lease<Slot> {
    if (this.busy) {
      // Two charts of one role on screen at once: the second gets a chart of its own, destroyed
      // with it. No page does this today; it keeps such a page correct rather than lean.
      const spare = this.create(host);
      return {
        slot: spare.slot,
        release: () => {
          spare.slot.chart.remove();
          spare.element.remove();
        },
      };
    }
    if (this.kept) host.appendChild(this.kept.element);
    else this.kept = this.create(host);
    const kept = this.kept;
    this.busy = true;
    let released = false;
    return {
      slot: kept.slot,
      release: () => {
        if (released) return;
        released = true;
        kept.element.remove();
        this.busy = false;
      },
    };
  }

  private create(host: HTMLElement) {
    const element = document.createElement('div');
    // The host is sized by the page; the chart fills it, as it did when drawn into the host itself.
    element.style.width = '100%';
    element.style.height = '100%';
    // Attached before the chart is built, so its first layout sees the real size.
    host.appendChild(element);
    return { element, slot: this.build(element) };
  }
}
