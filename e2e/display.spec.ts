import { expect, test } from '@playwright/test';

const rootVar = (name: string) => `getComputedStyle(document.documentElement).getPropertyValue('${name}').trim()`;

test('time zone and color flip apply across the UI and survive a reload', async ({ page }) => {
  await page.goto('/#settings');
  const clock = page.getByTestId('replay-time');
  await expect(clock).toBeVisible();

  await page.getByTestId('time-zone').selectOption('UTC');
  await expect(clock).toHaveText(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}Z$/);
  await page.getByTestId('time-zone').selectOption('Asia/Taipei');
  await expect(clock).toHaveText(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}\+08:00$/);

  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--up')));
  await page.getByTestId('flip-colors').check();
  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--down')));
  expect(await page.evaluate(rootVar('--fall'))).toBe(await page.evaluate(rootVar('--up')));

  await page.reload();
  await expect(page.getByTestId('time-zone')).toHaveValue('Asia/Taipei');
  await expect(page.getByTestId('flip-colors')).toBeChecked();
  await expect(clock).toHaveText(/\+08:00$/);
  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--down')));

  // The other theme defines its own palette, and the flip must follow it.
  await page.getByRole('button', { name: 'Toggle theme' }).click();
  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--down')));

  await page.getByTestId('flip-colors').uncheck();
  await page.getByTestId('time-zone').selectOption('local');
  await page.reload();
  await expect(page.getByTestId('flip-colors')).not.toBeChecked();
  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--up')));
});
