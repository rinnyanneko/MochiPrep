import { expect, test } from '@playwright/test';

/**
 * Every stat cell, with a very large figure in it, at phone, tablet and desktop widths: the figure
 * must fit its cell and the cell its strip. A cut digit reads as a different number.
 */
for (const width of [375, 800, 1280, 1440]) {
  test(`stat figures are never cut at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    await page.goto('/#markets');
    await expect(page.getByTestId('ticker-last')).toBeVisible();
    await expect(page.getByTestId('stat-strip')).toHaveCount(1);

    const cut = await page.evaluate((figures) => {
      const bad: string[] = [];
      for (const figure of figures) {
        for (const strip of document.querySelectorAll('[data-testid="stat-strip"]')) {
          const box = strip.getBoundingClientRect();
          for (const cell of strip.children) {
            const value = cell.children[1] as HTMLElement;
            const before = value.innerHTML;
            value.textContent = figure;
            const style = getComputedStyle(cell);
            const room = (cell as HTMLElement).clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
            const right = cell.getBoundingClientRect().right;
            if (value.scrollWidth > room + 0.5 || right > box.right + 0.5) bad.push(`${cell.children[0]!.textContent} ${figure}`);
            value.innerHTML = before;
          }
        }
      }
      return bad;
    }, ['123,456,789.12', '1,234,567,890.12']);

    expect(cut).toEqual([]);
  });
}
