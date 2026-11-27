# Web UI restyle: a plain trading terminal

## Why

The owner finds the current web UI "AI-flavoured and ugly": one 18px-rounded card for everything,
pink on buttons, nav and badges beside green/red market colours, a purple-tinted dark theme, a beige
light theme, pill badges everywhere, black-weight headings over 11-13px text, and "Your cute little
AI trading companion" copy. They like how OKX looks, but not how slow OKX's web app feels. A sample
of Dashboard and Trading was approved on 2026-11-27.

## Goals

- Look like a professional trading tool: neutral graphite and white, colour only for market
  direction, warnings and status.
- Stay light and fast: nothing in the restyle may add rendering cost.
- Same features, data and flows. Only appearance and wording change.

## Non-goals and anti-goals

- No new features, no layout rewrites of working screens (Trading's chart and ticket stay).
- No blur, large shadows or decorative animation. Transitions are colour-only and at most 120ms.
- No references to TradingView: the owner has not used it.

## Approaches considered

1. **Plain trading terminal (picked).** Neutral palette, black/white primary, small radii,
   hairlines instead of a box per item, figures in strips. Fits real-money use.
2. Warm minimal: keep beige and mascot personality, remove pills. Rejected: still reads cute, not a
   trading tool.
3. Fix only the worst spots. Rejected: the card-template skeleton, the main source of the look,
   would remain.

## Design

- **Tokens** (`apps/web/src/index.css`): neutral greys for both themes; `--accent` is ink (the
  primary button is white on dark, black on light); new `--brand` pink used only for the wordmark and
  mascot; `--radius-card` 8px; focus ring uses `--info`.
- **Components** (`components/ui`): `rounded-md` buttons, fields and segmented controls; `medium`
  and `semibold` weights only; outlined secondary and danger buttons; 4px-radius badges; underline
  tabs in ink; sentence-case table headers with row hover; `Stat` is a plain cell and `StatStrip`
  groups figures in one panel with hairlines.
- **Figures**: `.num` uses the sans face with tabular figures; the mono face stays for code and chart
  axes.
- **Shell**: smaller wordmark and mascot, quiet nav highlight, "Paper only" as plain text, smaller
  page titles, square toasts.
- **Copy and empty states**: the welcome becomes a three-step setup list; empty pages say what to do
  next; repeated warnings are not stacked.
- **Performance**: chart price labels write styles only when their place changes and do nothing
  without labels; the mascot's per-second mood timer re-renders only the mascot, not the sidebar.
  Before/after frame rate and long tasks are measured with the browser pane visible, and reported as
  measured or not measured.

## Left to the builder

Per-page spacing and which empty states need a next-step link, following the rules above.
