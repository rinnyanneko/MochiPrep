# Web UI Restyle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the approved "plain trading terminal" restyle across every page, keep it fast, and ship it as one PR to `dev`.

**Architecture:** Shared tokens (`apps/web/src/index.css`) and components (`apps/web/src/components/ui/index.tsx`) carry most of the look; pages only lose leftover pastel and heavy classes and gain plain empty states. The sample (tokens, components, shell, Dashboard welcome, `StatStrip`, chart label loop, mood timer) already sits uncommitted on `claude/ui-ux-polish`.

**Tech Stack:** React 19, Tailwind 4, Radix, lightweight-charts 5, vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-11-27-ui-restyle-design.md`

## Global Constraints

- Colour only for market direction (`rise`/`fall`), warnings, errors and status; `--brand` pink only on the wordmark and mascot.
- Radii: `rounded-md` for controls, `--radius-card` 8px for panels, `rounded` for badges. Weights: `font-medium` and `font-semibold` only.
- No blur, no shadows beyond a 1px ring, no decorative animation; transitions colour-only, `duration-100`.
- Features, data, flows, `data-testid`s and e2e-visible texts stay unless a task changes them and updates the spec that reads them.
- Copy is plain English, sentence case, no "cute", no "AI companion".
- PR base is `dev`; CodeRabbit runs against `dev` first; the publishing login is `rinnyanneko`.

## Review Focus

- Light theme: `--ink-3` (#7b7f87) text on `--surface` white must stay readable for labels (≥ 4.0:1); check stat labels and table headers.
- Custom direction colours (Settings → Up and down colors) must still recolour buttons, badges and chart lines after the token change.
- Keyboard focus must stay visible on white and on graphite (`--info` ring).
- A narrow window (375px) must not scroll sideways because of `StatStrip`.
- e2e specs that click "Add my AI key" or read the old welcome text must still find what they need.

---

### Task 1: Commit the sample

**Files:** the sample's current diff (`index.css`, `components/ui/index.tsx`, `Shell.tsx`, `Dashboard.tsx`, `Paper.tsx`, `Markets.tsx`, `charts.tsx`, plus the radius/weight sweep), `e2e/*.spec.ts` if a text moved.

- [ ] **Step 1:** `grep -rn "Add my AI key\|cute little\|Paper only" e2e apps/web/test` — update any spec that reads removed text; `data-testid="add-ai-key"` is kept on the first setup step.
- [ ] **Step 2:** `pnpm typecheck` and `pnpm test` — both pass.
- [ ] **Step 3:** Commit "Restyle the web UI as a plain trading terminal".

### Task 2: Remaining pages

**Files:** `pages/Agents.tsx`, `pages/Arena.tsx`, `pages/History.tsx`, `pages/Settings.tsx`, `pages/Markets.tsx`, `components/{AgentTemplates,AgentMessage,ExchangeCard,VaultCard,PairsCard,PairPicker,ReplayData,SignalsCard,NewsPanel,LauncherControls,trading}.tsx`.

- [ ] **Step 1:** `grep -rn "text-accent\|bg-accent-soft\|bg-info-soft\|text-\[15px\]\|uppercase" apps/web/src` — each hit is either a primary action (keep `accent`), a status (keep its tone), or decoration (→ `text-ink`/`bg-surface-2`).
- [ ] **Step 2:** Empty states on Agents, Trading, Arena and History each name the next step with a button to the page that does it (Agents → "New agent"; Trading → "New account"; Arena → "Create an agent"; History → "Open trading").
- [ ] **Step 3:** Info banners: one per page at most; Arena's long disclaimer becomes one line under the title.
- [ ] **Step 4:** Screenshot every page in both themes at 1440×900; fix what reads as boxed, pastel or heavy.
- [ ] **Step 5:** `pnpm typecheck`, `pnpm test`; commit "Restyle the remaining pages and empty states".

### Task 3: Dialogs and narrow screens

- [ ] **Step 1:** Open each dialog (new agent, templates, new account, replay dates, set password, stop all) and screenshot; fix radius/weight leftovers.
- [ ] **Step 2:** Resize to 375×812; every page has no horizontal page scroll (`document.documentElement.scrollWidth <= innerWidth`); `StatStrip` stacks.
- [ ] **Step 3:** Check the Review Focus items: label contrast in light mode, a custom colour preset, keyboard focus ring.
- [ ] **Step 4:** Commit if anything changed.

### Task 4: Performance check

- [ ] **Step 1:** With the browser pane visible, on Trading with one open position and live quotes, count `style` attribute mutations on chart labels per second (MutationObserver, 10 s) and long tasks, on this branch and with `charts.tsx` from `origin/dev`.
- [ ] **Step 2:** Report both numbers as measured, or say plainly that they could not be measured.

### Task 5: Ship

- [ ] **Step 1:** `pnpm build`, `pnpm test:e2e` (background), `pnpm typecheck`, `pnpm test` — all pass.
- [ ] **Step 2:** `coderabbit review --base dev`; fix valid findings; repeat until clean or only refuted findings remain.
- [ ] **Step 3:** Push, `gh pr create --base dev` with before/after notes; bind the PR.
