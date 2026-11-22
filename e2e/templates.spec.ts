import { expect, test } from '@playwright/test';

// Sorts after demo.spec, since creating an agent here also creates a paper account.
test('a duplicated template pre-fills a new agent, which keeps its own limits', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByTestId('replay-time')).toBeVisible();
  await page.getByRole('button', { name: 'AI Agents' }).click();

  await page.getByRole('button', { name: 'Templates' }).click();
  const manager = page.getByRole('dialog');
  await expect(manager.getByTestId('template-row').filter({ hasText: 'Balanced' })).toBeVisible();
  // Built-ins are read-only: duplicate is offered, edit is not.
  await expect(manager.getByRole('button', { name: 'Edit Balanced' })).toHaveCount(0);
  await manager.getByRole('button', { name: 'Duplicate Balanced' }).click();

  const editor = page.getByRole('dialog');
  await editor.getByLabel('Template name').fill('E2E careful');
  await editor.getByLabel('Strategy instructions').fill('Only trade the E2E setup.');
  await editor.getByLabel('Max leverage (x)').fill('2');
  await editor.getByRole('button', { name: 'Create template' }).click();

  const list = page.getByRole('dialog');
  const row = list.getByTestId('template-row').filter({ hasText: 'E2E careful' });
  await expect(row).toBeVisible();
  await expect(row).toContainText('max 2x');
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).toHaveCount(0);

  await page.getByRole('button', { name: 'New agent' }).click();
  const form = page.getByRole('dialog');
  await form.getByLabel('Start from template').selectOption({ label: 'E2E careful' });
  await expect(form.getByLabel('Strategy instructions')).toHaveValue('Only trade the E2E setup.');
  await expect(form.getByLabel('Max leverage (x)')).toHaveValue('2');
  await expect(form.getByLabel('Timeframe')).toHaveValue('15m');
  await form.getByLabel('Name').fill('Template Tofu');
  await form.getByRole('button', { name: 'Create agent' }).click();
  await expect(page.getByRole('dialog')).toHaveCount(0);

  const card = page.getByTestId('agent-card').filter({ hasText: 'Template Tofu' });
  await expect(card).toBeVisible();
  await expect(card.getByTestId('agent-mode')).toHaveText('Suggest');

  // Deleting the template leaves the agent as it was.
  await page.getByRole('button', { name: 'Templates' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Delete E2E careful' }).click();
  await expect(page.getByRole('dialog').getByTestId('template-row').filter({ hasText: 'E2E careful' })).toHaveCount(0);
  await page.keyboard.press('Escape');
  await card.getByRole('button', { name: 'Edit Template Tofu' }).click();
  const saved = page.getByRole('dialog');
  await expect(saved.getByLabel('Strategy instructions')).toHaveValue('Only trade the E2E setup.');
  await expect(saved.getByLabel('Max leverage (x)')).toHaveValue('2');
});

test('a failed template load says so and retries, instead of showing an empty list', async ({ page }) => {
  let fail = true;
  await page.route('**/api/agent-templates', async (route) => {
    if (fail && route.request().method() === 'GET') {
      await route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: 'Store unavailable' }) });
    } else {
      await route.fallback();
    }
  });
  await page.goto('/');
  await expect(page.getByTestId('replay-time')).toBeVisible();
  await page.getByRole('button', { name: 'AI Agents' }).click();
  await page.getByRole('button', { name: 'Templates' }).click();

  const manager = page.getByRole('dialog');
  await expect(manager.getByRole('alert')).toContainText('Templates could not be loaded');
  await expect(manager.getByText('No templates.')).toHaveCount(0);

  fail = false;
  await manager.getByRole('button', { name: 'Retry' }).click();
  await expect(manager.getByTestId('template-row').filter({ hasText: 'Balanced' })).toBeVisible();
  await expect(manager.getByRole('alert')).toHaveCount(0);

  // A refresh that fails after a good load keeps the list but still says it may be stale.
  fail = true;
  await manager.getByRole('button', { name: 'Restore built-ins' }).click();
  await expect(manager.getByRole('alert')).toContainText('Templates could not be loaded');
  await expect(manager.getByTestId('template-row').filter({ hasText: 'Balanced' })).toBeVisible();
  fail = false;
  await manager.getByRole('button', { name: 'Retry' }).click();
  await expect(manager.getByRole('alert')).toHaveCount(0);
});
