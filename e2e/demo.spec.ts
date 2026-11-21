import { expect, test } from '@playwright/test';

const usdt = (text: string | null) => Number((text ?? '').replace(/[^\d.-]/g, ''));

test('market data to agent decision to risk to paper order to position to PnL in the UI', async ({ page, request }) => {
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

  await page.getByRole('button', { name: 'Trading', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Paper Trading' })).toBeVisible();
  await page.getByLabel('Paper account').selectOption({ label: 'Reckless account' });
  await expect(page.getByTestId('chart-trade-summary')).toContainText(/[1-9]\d* fills?/);
  await page.getByLabel('Paper account').selectOption({ label: 'Demo terminal' });
  await expect(page.getByTestId('account-equity')).toHaveText('10,000.00');
  await page.getByRole('button', { name: 'Review long' }).click();
  await page.getByRole('button', { name: 'Place order' }).click();
  await expect(page.getByTestId('position-row')).toHaveCount(1);
  await expect(page.getByTestId('position-row')).toContainText('LONG');
  await expect(page.getByTestId('chart-trade-summary')).toContainText('Open');
  // Labels sit at the left, clear of the newest candles at the right edge.
  const label = page.getByTestId('chart-line-label').filter({ hasText: 'LONG' });
  await expect(label).toBeVisible();
  const [box, chart] = [await label.boundingBox(), await page.getByTestId('market-chart').boundingBox()];
  expect(box!.x + box!.width).toBeLessThan(chart!.x + chart!.width / 2);
  // Spread, slippage and the entry fee mean a fresh long is immediately below starting equity.
  await expect.poll(async () => usdt(await page.getByTestId('account-equity').textContent())).toBeLessThan(10_000);

  const accountId = await page.getByLabel('Paper account').inputValue();
  const session = await request.get('/api/session');
  const { token } = await session.json();
  const positions = await request.get(`/api/accounts/${encodeURIComponent(accountId)}/positions`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  expect(positions.ok()).toBe(true);
  const [position] = await positions.json();
  expect(position.id).toEqual(expect.any(String));
  const closing = page.waitForRequest((request) => request.method() === 'POST' && request.url().endsWith('/close'));
  await page.getByRole('button', { name: 'Close', exact: true }).click();
  expect((await closing).postDataJSON().positionId).toBe(position.id);
  await expect(page.getByTestId('position-row')).toHaveCount(0);

  await page.getByRole('button', { name: 'Settings' }).click();
  await page.getByRole('button', { name: 'Reset demo' }).click();
  await page.getByTestId('confirm-demo-action').click();
  await page.getByRole('button', { name: 'Dashboard' }).click();
  await expect(page.getByTestId('total-equity')).toHaveText('40,000.00');
});

test('an agent in suggestion mode waits for the user, then the suggestion becomes a position', async ({ page }) => {
  await page.goto('/');
  // The first test may have left Demo Mode on, in the same throwaway backend.
  const demo = page.getByTestId('try-demo');
  await expect(demo.or(page.getByTestId('total-equity'))).toBeVisible();
  if (await demo.isVisible()) await demo.click();

  await page.getByRole('button', { name: 'AI Agents' }).click();
  const card = page.getByTestId('agent-card').filter({ hasText: 'Reckless Rice Cake' });
  await card.getByRole('button', { name: 'Edit Reckless Rice Cake' }).click();
  await page.getByRole('button', { name: 'Suggest to me' }).click();
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(card.getByTestId('agent-mode')).toHaveText('Suggest');
  await card.getByRole('button', { name: 'Start', exact: true }).click();

  await page.getByLabel('Replay speed').selectOption('60');
  await page.getByRole('button', { name: 'Play replay' }).click();

  await page.getByRole('button', { name: /^Dashboard/ }).click();
  const row = page.getByTestId('signal-row').first();
  await expect(row).toBeVisible({ timeout: 30_000 });
  // The tape stops at a suggestion, and nothing has traded yet.
  await expect(page.getByRole('button', { name: 'Play replay' })).toBeVisible();
  await expect(page.getByTestId('signal-count')).toHaveText('1');

  await row.getByRole('button', { name: 'Place order' }).click();
  await expect(page.getByTestId('signals-card')).toBeHidden();
  await expect(page.getByTestId('position-row')).toHaveCount(1);
});

test('a 1% trailing stop set in the protection dialog shows its trigger price in the position row', async ({ page }) => {
  await page.goto('/');
  // Earlier tests may have left Demo Mode on, in the same throwaway backend.
  const demo = page.getByTestId('try-demo');
  await expect(demo.or(page.getByTestId('total-equity'))).toBeVisible();
  if (await demo.isVisible()) await demo.click();
  await page.getByRole('button', { name: 'Trading', exact: true }).click();
  await page.getByLabel('Paper account').selectOption({ label: 'Demo terminal' });
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

  // A long's trigger sits below the last price: 1% under the highest price seen.
  const trigger = page.getByTestId('trailing-trigger');
  await expect(trigger).toContainText(/Trail [\d,]+\.\d+/);
  const mark = usdt(await page.getByTestId('position-row').locator("td").nth(4).textContent());
  const triggerPrice = usdt(await trigger.textContent());
  expect(triggerPrice).toBeLessThan(mark);
  expect(triggerPrice).toBeGreaterThan(mark * 0.97);

  await page.getByRole('button', { name: /^Edit stop and target for/ }).click();
  await dialog.getByLabel('Stop loss', { exact: true }).fill((mark * 0.96).toFixed(2));
  const saving = page.waitForRequest((req) => req.method() === 'PUT' && req.url().endsWith('/protection'));
  await dialog.getByRole('button', { name: 'Save' }).click();
  const body = (await saving).postDataJSON();
  expect(body.keepTakeProfit).toBe(true);
  expect(body).not.toHaveProperty('trailingStop');
  await expect(dialog).toHaveCount(0);
  await expect(trigger).toBeVisible();
});

test('separate OKX take profits show the size of each order', async ({ page }) => {
  await page.route('**/api/accounts', async (route) => {
    const response = await route.fetch();
    const accounts = await response.json() as { id: string; name: string; venue: string }[];
    await route.fulfill({
      response,
      json: accounts.map((account) => account.name === 'Demo terminal'
        ? { ...account, venue: 'OKX_DEMO' }
        : account),
    });
  });
  await page.route('**/api/accounts/*/orders?**', (route) => route.fulfill({
    json: [
      { id: 'first-target', quantity: '0.0005', triggerPrice: '78000' },
      { id: 'second-target', quantity: '0.001', triggerPrice: '79000' },
      { id: 'small-target', quantity: '0.0000003', triggerPrice: '80000' },
    ].map((target) => ({
      ...target,
      symbol: 'BTCUSDT',
      side: 'SELL',
      type: 'TAKE_PROFIT_MARKET',
      status: 'OPEN',
      closePosition: true,
      reduceOnly: true,
      createdAt: Date.UTC(2026, 0, 1),
    })),
  }));
  await page.goto('/');
  const demo = page.getByTestId('try-demo');
  await expect(demo.or(page.getByTestId('total-equity'))).toBeVisible();
  if (await demo.isVisible()) await demo.click();
  await page.getByRole('button', { name: 'Trading', exact: true }).click();
  await page.getByLabel('Paper account').selectOption({ label: 'Demo terminal (OKX demo)' });
  await expect(page.getByRole('heading', { name: 'OKX Trading' })).toBeVisible();
  await page.getByRole('tab', { name: 'Pending orders (3)' }).click();
  const table = page.getByRole('table');
  await expect(table.getByRole('cell', { name: '0.0005', exact: true })).toBeVisible();
  await expect(table.getByRole('cell', { name: '0.001', exact: true })).toBeVisible();
  await expect(table.getByRole('cell', { name: '0.0000003', exact: true })).toBeVisible();
});

test('OKX market and limit reductions send the selected isolated position', async ({ page: firstPage, context }) => {
  let page = firstPage;
  await context.route('**/api/instruments', async (route) => {
    const response = await route.fetch();
    const instruments = await response.json() as { symbol: string; priceTick: string }[];
    await route.fulfill({
      response,
      json: instruments.map((instrument) => instrument.symbol === 'BTCUSDT'
        ? { ...instrument, priceTick: '1e-1' }
        : instrument),
    });
  });
  await context.route('**/api/accounts', async (route) => {
    const response = await route.fetch();
    const accounts = await response.json() as { id: string; name: string; venue: string }[];
    await route.fulfill({
      response,
      json: accounts.map((account) => account.name === 'Demo terminal'
        ? { ...account, venue: 'OKX_DEMO' }
        : account),
    });
  });
  const positionId = 'okx:BTC-USDT-SWAP:isolated:LONG:test:1';
  await context.route('**/api/accounts/*/positions', (route) => {
    const path = new URL(route.request().url()).pathname.split('/');
    const accountId = path.at(-2);
    const position = {
      accountId, symbol: 'BTCUSDT', side: 'LONG', quantity: '0.001',
      entryPrice: '75000', markPrice: '75500', liquidationPrice: '52000',
      margin: '25', leverage: 3, unrealizedPnl: '0.5', openedAt: Date.UTC(2026, 0, 1),
      externalProtection: [],
    };
    return route.fulfill({ json: [
      { ...position, id: 'cross-position', marginMode: 'cross', closeSupported: false },
      { ...position, id: positionId, marginMode: 'isolated', closeSupported: true },
    ] });
  });
  let unconfirmedOrder = false;
  let orderReadback: Record<string, unknown> | undefined;
  let submittedOrders = 0;
  await context.route('**/api/orders', (route) => {
    submittedOrders += 1;
    if (unconfirmedOrder) {
      unconfirmedOrder = false;
      const order = route.request().postDataJSON();
      orderReadback = { ...order, id: 'pending-okx-reduction', status: 'OPEN', exchangeOrderId: 'fake-okx-order' };
      return route.fulfill({ json: { order: { ...orderReadback, exchangeOrderId: null }, fills: [], duplicate: false } });
    }
    const order = route.request().postDataJSON();
    return route.fulfill({ json: { order: { ...order, status: 'OPEN', exchangeOrderId: 'fake-okx-order' }, fills: [], duplicate: false } });
  });
  await context.route('**/api/orders/pending-okx-reduction', (route) => route.fulfill({ json: orderReadback }));
  await page.goto('/');
  const demo = page.getByTestId('try-demo');
  await expect(demo.or(page.getByTestId('total-equity'))).toBeVisible();
  if (await demo.isVisible()) await demo.click();
  await page.getByRole('button', { name: 'Trading', exact: true }).click();
  await page.getByLabel('Paper account').selectOption({ label: 'Demo terminal (OKX demo)' });
  await expect(page.getByTestId('position-row')).toHaveCount(2);
  await page.getByRole('checkbox', { name: 'Reduce only' }).check();
  const selected = page.getByLabel('Position to reduce');
  await expect(selected).toHaveValue(positionId);
  await expect(selected.locator('option[value="cross-position"]')).toHaveCount(0);

  const size = page.getByLabel('Order size');
  await size.fill('0.002');
  const review = page.getByRole('button', { name: 'Review reduction' });
  await expect(review).toBeDisabled();
  await size.fill('0.00019');
  await expect(review).toBeDisabled();
  await size.fill('0.0004');

  // Simulate both browser stores refusing the reduction reference.
  await page.evaluate(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key.startsWith('mochiprep.pending-reduction.')) {
        throw new DOMException('Storage is full', 'QuotaExceededError');
      }
      original.call(this, key, value);
    };
    window.addEventListener('restore-test-storage', () => {
      Storage.prototype.setItem = original;
    }, { once: true });
  });
  await review.click();
  await page.getByRole('button', { name: 'Place order', exact: true }).click();
  await expect(page.getByText('Browser storage is required to confirm this reduction.')).toBeVisible();
  expect(submittedOrders).toBe(0);
  await page.getByRole('dialog').getByRole('button', { name: 'Close', exact: true }).click();
  await page.evaluate(() => window.dispatchEvent(new Event('restore-test-storage')));

  // Session storage alone ends with its tab, so it cannot hold the reduction reference.
  await page.evaluate(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (this === window.localStorage && key.startsWith('mochiprep.pending-reduction.')) {
        throw new DOMException('Storage is full', 'QuotaExceededError');
      }
      original.call(this, key, value);
    };
    window.addEventListener('restore-test-storage', () => {
      Storage.prototype.setItem = original;
    }, { once: true });
  });
  // Wait for the first refusal's toast to expire, so only a new one can match.
  const storageToast = page.getByText('Browser storage is required to confirm this reduction.');
  await expect(storageToast).toHaveCount(0, { timeout: 15_000 });
  await review.click();
  await page.getByRole('button', { name: 'Place order', exact: true }).click();
  await expect(storageToast).toBeVisible();
  expect(submittedOrders).toBe(0);
  expect(await page.evaluate(() => Object.keys(sessionStorage)
    .filter((key) => key.startsWith('mochiprep.pending-reduction.')))).toEqual([]);
  await page.getByRole('dialog').getByRole('button', { name: 'Close', exact: true }).click();
  await page.evaluate(() => window.dispatchEvent(new Event('restore-test-storage')));

  let marketClientOrderId: string | undefined;
  for (const [type, limit] of [['Market', null], ['Limit', '76000.1']] as const) {
    await page.getByRole('button', { name: type, exact: true }).click();
    if (limit) {
      const price = page.getByRole('textbox', { name: 'Limit price' });
      await price.fill('76000.04');
      await expect(review).toBeDisabled();
      await expect(page.getByText(/price.*multiple of 1e-1/i)).toBeVisible();
      await price.fill(limit);
    }
    await review.click();
    if (limit) {
      await expect(page.getByRole('dialog').getByText(limit, { exact: true })).toBeVisible();
      unconfirmedOrder = true;
    }
    const request = page.waitForRequest((req) => req.method() === 'POST' && req.url().endsWith('/api/orders'));
    await page.getByRole('button', { name: 'Place order', exact: true }).click();
    const body = (await request).postDataJSON();
    expect(body).toMatchObject({
      symbol: 'BTCUSDT', positionId, side: 'SELL', type: type.toUpperCase(),
      quantity: '0.0004', price: limit, reduceOnly: true,
      clientOrderId: expect.stringMatching(/^ui:/),
    });
    if (limit) {
      expect(body.clientOrderId).not.toBe(marketClientOrderId);
      const dialog = page.getByRole('dialog');
      await expect(dialog.getByRole('button', { name: 'Check order status' })).toBeEnabled();
      await dialog.getByRole('button', { name: 'Close', exact: true }).click();
      await page.evaluate((request) => {
        const other = { ...request, clientOrderId: 'ui:other-window', quantity: '0.0001' };
        localStorage.setItem(`mochiprep.pending-reduction.${other.accountId}/${other.clientOrderId}`, JSON.stringify({ request: other }));
      }, body);
      const url = page.url();
      await page.close();
      page = await context.newPage();
      await page.goto(url);
      await page.getByLabel('Paper account').selectOption({ label: 'Demo terminal (OKX demo)' });
      await page.getByRole('button', { name: 'Review pending reduction' }).click();
      const reopenedDialog = page.getByRole('dialog');
      await reopenedDialog.getByLabel('Pending reduction').selectOption(body.clientOrderId);
      await expect(reopenedDialog.getByText(limit, { exact: true })).toBeVisible();
      const readback = page.waitForRequest((req) => req.method() === 'GET' && req.url().endsWith('/api/orders/pending-okx-reduction'));
      await reopenedDialog.getByRole('button', { name: 'Check order status' }).click();
      await readback;
      await expect(reopenedDialog).toHaveCount(0);
      await page.getByRole('button', { name: 'Review pending reduction' }).click();
      await expect(reopenedDialog.getByText('0.0001 BTC', { exact: true })).toBeVisible();
      await reopenedDialog.getByRole('button', { name: 'Discard retry' }).click();
    } else {
      marketClientOrderId = body.clientOrderId;
    }
    await expect(page.getByRole('dialog')).toHaveCount(0);
  }
  await expect(page.getByRole('button', { name: 'Review pending reduction' })).toHaveCount(0);
});
