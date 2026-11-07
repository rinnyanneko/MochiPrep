import { expect, test } from '@playwright/test';

const usdt = (text: string | null) => Number((text ?? '').replace(/[^\d.-]/g, ''));

test('market data to agent decision to risk to paper order to position to PnL in the UI', async ({ page }) => {
  await page.goto('/');

  await expect(page.getByTestId('dyor-notice')).toContainText('DYOR');

  await page.getByTestId('try-demo').click();
  await expect(page.getByTestId('demo-banner')).toBeVisible();
  await expect(page.getByTestId('total-equity')).toHaveText('40,000.00');

  await page.getByRole('button', { name: 'Markets' }).click();
  await expect(page.getByTestId('market-chart').locator('canvas').first()).toBeVisible();
  await expect.poll(async () => usdt(await page.getByTestId('ticker-last').textContent())).toBeGreaterThan(0);

  await page.getByRole('button', { name: 'AI Agents' }).click();
  await expect(page.getByTestId('agent-card')).toHaveCount(3);
  for (const start of await page.getByRole('button', { name: 'Start', exact: true }).all().then((b) => b.reverse())) {
    await start.click();
  }
  await expect(page.getByTestId('agent-status').filter({ hasText: 'Waiting' })).toHaveCount(3);

  await page.getByLabel('Replay speed').selectOption('240');
  await page.getByRole('button', { name: 'Play replay' }).click();

  await page.getByRole('button', { name: 'Trading Arena' }).click();
  await expect
    .poll(async () => (await page.getByTestId('arena-row').allInnerTexts()).join().includes('−'), { timeout: 45_000 })
    .toBe(true);
  await expect(page.getByTestId('arena-row')).toHaveCount(3);
  // A fixed tape time makes everything after this point deterministic.
  await expect.poll(() => page.getByTestId('replay-time').textContent(), { timeout: 45_000 }).toMatch(/2026-01-0[6-9]/);

  await page.getByRole('button', { name: /^Stop all agents/ }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Stop all' }).click();
  await expect(page.getByRole('button', { name: 'Play replay' })).toBeVisible();

  await page.getByRole('button', { name: 'Trade History' }).click();
  await page.getByLabel('Filter by outcome').selectOption('EXECUTED');
  await page.getByTestId('decision-row').first().click();
  await expect(page.getByTestId('risk-checks')).toContainText('Risk engine: approved');
  await expect(page.getByTestId('decision-detail')).toContainText('Filled');
  await page.getByLabel('Filter by outcome').selectOption('RISK_REJECTED');
  await page.getByTestId('decision-row').first().click();
  await expect(page.getByTestId('risk-checks')).toContainText('Risk engine: rejected');
  await expect(page.getByTestId('ledger-audit')).toHaveText('Ledger balances match');

  await page.getByRole('button', { name: 'Paper Trading' }).click();
  await page.getByLabel('Paper account').selectOption({ label: 'Demo terminal' });
  await expect(page.getByTestId('account-equity')).toHaveText('10,000.00');
  await page.getByRole('button', { name: 'Review long' }).click();
  await page.getByRole('button', { name: 'Place order' }).click();
  await expect(page.getByTestId('position-row')).toHaveCount(1);
  await expect(page.getByTestId('position-row')).toContainText('LONG');
  // Spread, slippage and the entry fee mean a fresh long is immediately below starting equity.
  await expect.poll(async () => usdt(await page.getByTestId('account-equity').textContent())).toBeLessThan(10_000);

  await page.getByRole('button', { name: 'Close', exact: true }).click();
  await expect(page.getByTestId('position-row')).toHaveCount(0);

  await page.getByRole('button', { name: 'Settings' }).click();
  await page.getByRole('button', { name: 'Reset demo' }).click();
  await page.getByTestId('confirm-demo-action').click();
  await page.getByRole('button', { name: 'Dashboard' }).click();
  await expect(page.getByTestId('total-equity')).toHaveText('40,000.00');
});
