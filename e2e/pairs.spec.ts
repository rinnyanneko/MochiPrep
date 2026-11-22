import { expect, test } from '@playwright/test';
import { seed } from './seed';

// Sorts after agents.spec, since creating an agent here also creates a paper account.
test('an agent picks several pairs and one cycle decides each of them', async ({ page, request }) => {
  await seed(request);
  await page.goto('/');
  await page.getByRole('button', { name: 'AI Agents' }).click();
  await page.getByRole('button', { name: 'New agent' }).click();
  const form = page.getByRole('dialog');
  await form.getByPlaceholder('Careful Kinako').fill('Pair Pudding');
  await form.getByLabel('AI provider').selectOption({ label: 'E2E model' });
  const pairs = form.getByRole('group', { name: 'Pairs' });
  await expect(pairs.getByRole('button', { name: 'BTC', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await pairs.getByRole('button', { name: 'ETH', exact: true }).click();
  await expect(form.getByText('Pairs (2 of up to 10)')).toBeVisible();
  await form.getByRole('button', { name: 'Create agent' }).click();
  await expect(form).toHaveCount(0);

  const card = page.getByTestId('agent-card').filter({ hasText: 'Pair Pudding' });
  await expect(card).toContainText('BTCUSDT, ETHUSDT');
  await card.getByRole('button', { name: 'Run one cycle' }).click();
  // One model call, one decision per pair.
  const rows = page.getByTestId('decision-row');
  await expect(rows).toHaveCount(2);
  await expect(rows.filter({ hasText: 'BTCUSDT' })).toHaveCount(1);
  await expect(rows.filter({ hasText: 'ETHUSDT' })).toHaveCount(1);

  await page.getByRole('button', { name: 'Trading Arena' }).click();
  await page.getByRole('group', { name: 'Arena market' }).getByRole('button', { name: 'OKX', exact: true }).click();
  await expect(page.getByText('No agents on OKX accounts yet.')).toBeVisible();
});
