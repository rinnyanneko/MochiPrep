import { expect, test } from '@playwright/test';

const usdt = (text: string | null) => Number((text ?? '').replace(/[^\d.-]/g, ''));

test('a replay paper account trades end to end: order, trailing stop, close, ledger audit', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByTestId('replay-time')).toBeVisible();
  await page.getByRole('button', { name: 'Trading', exact: true }).click();

  await page.getByRole('button', { name: 'New account' }).click();
  const create = page.getByRole('dialog');
  await create.getByPlaceholder('My paper account').fill('E2E paper');
  await create.getByRole('button', { name: 'Create' }).click();
  await expect(create).toHaveCount(0);
  await expect(page.getByLabel('Paper account')).toContainText('E2E paper');

  // Step the tape so there is an observed quote to fill against.
  const before = await page.getByTestId('replay-time').textContent();
  await page.getByRole('button', { name: 'Step 15 minutes' }).click();
  await expect(page.getByTestId('replay-time')).not.toHaveText(before ?? '');

  await page.getByRole('button', { name: 'Review long' }).click();
  await page.getByRole('button', { name: 'Place order' }).click();
  await expect(page.getByTestId('position-row')).toHaveCount(1);
  await expect(page.getByTestId('trailing-trigger')).toHaveCount(0);

  await page.getByRole('button', { name: /^Edit stop and target for/ }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('button', { name: 'Percent' }).click();
  await dialog.getByLabel('Trailing callback').fill('1');
  await dialog.getByRole('button', { name: 'Save' }).click();
  await expect(dialog).toHaveCount(0);

  const trigger = page.getByTestId('trailing-trigger');
  await expect(trigger).toContainText(/Trail [\d,]+\.\d+/);
  // A 1% callback trails the long's best mark, so the trigger sits near mark x 0.99. The replay
  // may move between the two reads (and the peak can sit above the mark), so allow 0.2% of the
  // mark and re-read both until they agree.
  await expect(async () => {
    const mark = usdt(await page.getByTestId('position-row').locator('td').nth(4).textContent());
    const triggerPrice = usdt(await trigger.textContent());
    expect(triggerPrice).toBeLessThan(mark);
    expect(Math.abs(triggerPrice - mark * 0.99)).toBeLessThanOrEqual(mark * 0.002);
  }).toPass({ timeout: 10_000 });

  await page.getByRole('button', { name: 'Close', exact: true }).click();
  await expect(page.getByTestId('position-row')).toHaveCount(0);

  await page.getByRole('button', { name: 'Trade History' }).click();
  await expect(page.getByTestId('ledger-audit')).toHaveText('Ledger balances match');
});
