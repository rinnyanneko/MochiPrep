import { expect, test } from '@playwright/test';

test('log retention survives navigating away from Settings', async ({ page, request }) => {
  const session = await request.get('/api/session');
  const { token } = await session.json();
  const reset = await request.put('/api/settings/operation-logs', {
    headers: { Authorization: `Bearer ${token}` },
    data: { retention: 10 },
  });
  expect(reset.ok()).toBe(true);
  await page.goto('/');
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  const retention = page.getByLabel('Dated logs to keep');
  await expect(retention).toHaveValue('10');
  await retention.fill('0');
  await page.getByRole('button', { name: 'Save log retention' }).click();
  await expect(page.getByText('Log retention saved')).toBeVisible();
  await page.getByRole('button', { name: 'Dashboard', exact: true }).click();
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await expect(retention).toHaveValue('0');
});

const rootVar = (name: string) => `getComputedStyle(document.documentElement).getPropertyValue('${name}').trim()`;

test('time zone and direction colors apply across the UI and survive a reload', async ({ page }, testInfo) => {
  await page.goto('/#settings');
  const clock = page.getByTestId('replay-time');
  await expect(clock).toBeVisible();

  await page.getByTestId('time-zone').selectOption('UTC');
  await expect(clock).toHaveText(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}Z$/);
  await page.getByTestId('time-zone').selectOption('Asia/Taipei');
  await expect(clock).toHaveText(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}\+08:00$/);

  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--up')));
  await page.getByRole('combobox', { name: 'Up and down color preset' }).selectOption('red-green');
  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--down')));
  expect(await page.evaluate(rootVar('--fall'))).toBe(await page.evaluate(rootVar('--up')));

  await page.reload();
  await expect(page.getByTestId('time-zone')).toHaveValue('Asia/Taipei');
  await expect(page.getByRole('combobox', { name: 'Up and down color preset' })).toHaveValue('red-green');
  await expect(clock).toHaveText(/\+08:00$/);
  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--down')));

  // The other theme defines its own palette, and presets must follow it.
  await page.getByRole('button', { name: 'Toggle theme' }).click();
  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--down')));

  await page.getByRole('combobox', { name: 'Up and down color preset' }).selectOption('red-blue');
  expect(await page.evaluate(rootVar('--fall'))).toBe('#2563eb');
  await page.getByLabel('Up color', { exact: true }).fill('#aa22cc');
  await page.reload();
  await expect(page.getByLabel('Up color', { exact: true })).toHaveValue('#aa22cc');
  expect(await page.evaluate(rootVar('--rise'))).toBe('#aa22cc');
  await page.getByTestId('display-card').screenshot({ path: testInfo.outputPath('display-light.png') });
  await page.getByRole('button', { name: 'Toggle theme' }).click();
  expect(await page.evaluate(rootVar('--rise'))).toBe('#aa22cc');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByTestId('display-card').screenshot({ path: testInfo.outputPath('display-dark-mobile.png') });
  await page.getByRole('combobox', { name: 'Up and down color preset' }).selectOption('green-red');
  await page.getByTestId('time-zone').selectOption('local');
  await page.reload();
  await expect(page.getByRole('combobox', { name: 'Up and down color preset' })).toHaveValue('green-red');
  expect(await page.evaluate(rootVar('--rise'))).toBe(await page.evaluate(rootVar('--up')));
});
