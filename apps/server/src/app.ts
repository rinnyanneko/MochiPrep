import fastify, { type FastifyInstance, type FastifyReply, type FastifyRequest } from 'fastify';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import fastifyStatic from '@fastify/static';
import { auditLedger } from '@mochiprep/core';
import {
  AddInstrumentSchema,
  AgentInputSchema,
  ArmSchema,
  ClosePositionSchema,
  CreateAccountSchema,
  INSTRUMENTS,
  NewsSourceInputSchema,
  NewsSourceToggleSchema,
  OkxConnectSchema,
  PlaceOrderSchema,
  ProtectionSchema,
  ProviderInputSchema,
  ProviderKeySchema,
  ReplayControlSchema,
  SymbolSchema,
  TIMEFRAMES,
  VaultChangeSchema,
  VaultCreateSchema,
  VaultPasswordSchema,
  VaultResetSchema,
  type ServerEvent,
} from '@mochiprep/shared';
import { randomUUID } from 'node:crypto';
import { z, ZodError } from 'zod';
import { REPO_ROOT, type ServerConfig } from './config';
import { MOCK_PROVIDER_ID, type Runtime } from './runtime';
import { WrongPasswordError } from './vault';

const CandleQuery = z.object({
  source: z.enum(['REPLAY', 'LIVE']),
  symbol: SymbolSchema,
  timeframe: z.enum(TIMEFRAMES),
  limit: z.coerce.number().int().min(1).max(1000).default(300),
});
const SourceQuery = z.object({ source: z.enum(['REPLAY', 'LIVE']) });
const LimitQuery = z.object({ limit: z.coerce.number().int().min(1).max(2000).default(200) });
const DecisionQuery = LimitQuery.extend({ agentId: z.string().optional() });

type IdParams = { id: string };
type PositionParams = { accountId: string; symbol: string };

function sameToken(a: string, b: string) {
  const left = Buffer.from(a);
  const right = Buffer.from(b);
  return left.length === right.length && timingSafeEqual(left, right);
}

export function buildApp(runtime: Runtime, config: ServerConfig): { app: FastifyInstance; token: string } {
  const app = fastify({ logger: { level: 'warn' }, bodyLimit: 256 * 1024 });
  /** New on every start. Browsers can only obtain it from an allowed origin. */
  const token = randomBytes(32).toString('hex');
  const allowedHosts = new Set(config.allowedOrigins.map((origin) => new URL(origin).host));

  const streams = new Set<NodeJS.WritableStream & { end(): void }>();
  // Open event streams would otherwise hold app.close() forever.
  app.addHook('preClose', async () => {
    for (const stream of streams) stream.end();
  });

  const hostAllowed = (request: FastifyRequest) => allowedHosts.has(request.headers.host ?? '');

  app.addHook('onRequest', async (request, reply) => {
    // A rebinding page reaches us with its own Host; a foreign page with its own Origin.
    if (!hostAllowed(request)) return reply.code(403).send({ error: 'Host not allowed' });
    const origin = request.headers.origin;
    if (origin && !config.allowedOrigins.includes(origin)) return reply.code(403).send({ error: 'Origin not allowed' });

    const path = request.url.split('?')[0]!;
    if (!path.startsWith('/api/') || path === '/api/session') return;
    const header = request.headers.authorization?.replace(/^Bearer /, '');
    const supplied = header ?? (path === '/api/events' ? (request.query as { token?: string }).token : undefined);
    if (!supplied || !sameToken(supplied, token)) return reply.code(401).send({ error: 'Unauthorized' });
  });

  app.setErrorHandler((error: Error & { statusCode?: number }, _request, reply) => {
    if (error instanceof ZodError) {
      const message = error.issues.map((i) => `${i.path.join('.') || 'input'}: ${i.message}`).join('; ');
      return reply.code(400).send({ error: message });
    }
    // Not 401: the browser client reads 401 as an expired session token and retries.
    if (error instanceof WrongPasswordError) return reply.code(403).send({ error: error.message });
    const message = runtime.redact(error.message);
    if ((error.statusCode ?? 400) >= 500) app.log.error({ message });
    return reply.code(error.statusCode ?? 400).send({ error: message });
  });

  /** Only a same-origin page or an allowed Origin gets the token. curl can too; it is already local. */
  app.get('/api/session', async (request, reply) => {
    const site = request.headers['sec-fetch-site'];
    const sameOrigin = site === 'same-origin' || site === 'none' || site === undefined;
    if (!sameOrigin && !request.headers.origin) return reply.code(403).send({ error: 'Cross-site request refused' });
    return { token };
  });

  app.get('/api/health', async () => ({ ok: true }));

  app.get('/api/state', async () => ({
    demoActive: runtime.isDemoActive(),
    vault: runtime.vaultStatus(),
    replay: runtime.replayState(),
    feeds: runtime.feeds(),
    instruments: INSTRUMENTS,
  }));

  app.get('/api/events', (request, reply: FastifyReply) => {
    reply.hijack();
    const raw = reply.raw;
    raw.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache, no-transform',
      connection: 'keep-alive',
    });
    const send = (event: ServerEvent) => raw.write(`data: ${JSON.stringify(event)}\n\n`);
    const unsubscribe = runtime.subscribe(send);
    const heartbeat = setInterval(() => send({ type: 'heartbeat' }), 15_000);
    streams.add(raw);
    for (const status of runtime.feeds()) send({ type: 'feed', status });
    send({ type: 'replay', state: runtime.replayState() });
    request.raw.on('close', () => {
      clearInterval(heartbeat);
      unsubscribe();
      streams.delete(raw);
    });
  });

  // ---- market ---------------------------------------------------------------------------
  app.get('/api/market/candles', async (request) => {
    const { source, symbol, timeframe, limit } = CandleQuery.parse(request.query);
    runtime.assertTradable(symbol, source);
    return runtime.candles(source, symbol, timeframe, limit);
  });
  app.get('/api/market/quotes', async (request) => runtime.quotes(SourceQuery.parse(request.query).source));

  // ---- news -----------------------------------------------------------------------------
  app.get('/api/news', async (request) => {
    const { symbol } = z.object({ symbol: SymbolSchema }).parse(request.query);
    return runtime.newsFor(symbol, 12);
  });
  app.get('/api/news/sources', async () => runtime.newsSources());
  app.post('/api/news/sources', async (request) => {
    const { name, url } = NewsSourceInputSchema.parse(request.body);
    return runtime.addNewsSource(name, url);
  });
  app.put<{ Params: IdParams }>('/api/news/sources/:id', async (request) => {
    runtime.setNewsSourceEnabled(request.params.id, NewsSourceToggleSchema.parse(request.body).enabled);
    return { ok: true };
  });
  app.delete<{ Params: IdParams }>('/api/news/sources/:id', async (request) => {
    runtime.removeNewsSource(request.params.id);
    return { ok: true };
  });

  // ---- trading pairs --------------------------------------------------------------------
  app.get('/api/instruments', async () => Object.values(INSTRUMENTS));
  app.get('/api/instruments/available', async () => runtime.availableInstruments());
  app.post('/api/instruments', async (request) => runtime.addInstrument(AddInstrumentSchema.parse(request.body).symbol));
  app.delete<{ Params: { symbol: string } }>('/api/instruments/:symbol', async (request) => {
    await runtime.removeInstrument(SymbolSchema.parse(request.params.symbol));
    return { ok: true };
  });

  // ---- accounts -------------------------------------------------------------------------
  const accountView = (id: string) => ({ ...runtime.store.getAccount(id)!, balance: runtime.balanceOf(id) });
  const requireAccount = (id: string) => {
    if (!runtime.store.getAccount(id)) throw Object.assign(new Error('Unknown account'), { statusCode: 404 });
    return id;
  };

  app.get('/api/accounts', async () => runtime.store.listAccounts().map((a) => accountView(a.id)));
  app.post('/api/accounts', async (request) => {
    const account = runtime.engine.createAccount(CreateAccountSchema.parse(request.body));
    runtime.invalidate('accounts');
    return accountView(account.id);
  });
  app.post<{ Params: IdParams }>('/api/accounts/:id/reset', async (request) => {
    runtime.engine.resetAccount(requireAccount(request.params.id));
    runtime.invalidate('accounts', 'orders', 'positions', 'decisions', 'arena');
    return accountView(request.params.id);
  });
  app.delete<{ Params: IdParams }>('/api/accounts/:id', async (request) => {
    const id = requireAccount(request.params.id);
    if (runtime.store.listAgents().some((a) => a.accountId === id)) throw new Error('Delete the agents using this account first');
    runtime.disconnectExchange(id);
    runtime.store.transaction(() => runtime.store.deleteAccountData(id, false));
    runtime.invalidate('accounts', 'orders', 'positions');
    return { ok: true };
  });
  app.get<{ Params: IdParams }>('/api/accounts/:id/positions', async (r) => runtime.positionsOf(requireAccount(r.params.id)));
  app.get<{ Params: IdParams }>('/api/accounts/:id/closed-positions', async (r) => {
    return runtime.store.listClosedPositions(requireAccount(r.params.id), LimitQuery.parse(r.query).limit);
  });
  app.get<{ Params: IdParams }>('/api/accounts/:id/orders', async (r) => runtime.store.listOrders(requireAccount(r.params.id), LimitQuery.parse(r.query).limit));
  app.get<{ Params: IdParams }>('/api/accounts/:id/fills', async (r) => runtime.store.listFills(requireAccount(r.params.id), LimitQuery.parse(r.query).limit));
  app.get<{ Params: IdParams }>('/api/accounts/:id/ledger', async (r) => runtime.store.listLedger(requireAccount(r.params.id)).slice(-LimitQuery.parse(r.query).limit).reverse());
  app.get<{ Params: IdParams }>('/api/accounts/:id/equity', async (r) => runtime.store.listEquity(requireAccount(r.params.id), LimitQuery.parse(r.query).limit));
  app.get<{ Params: IdParams }>('/api/accounts/:id/audit', async (r) => {
    const id = requireAccount(r.params.id);
    // The exchange keeps the books for its accounts, so there is no local ledger to audit.
    if (runtime.store.getAccount(id)!.venue !== 'PAPER') return { consistent: true, issues: [], notApplicable: true };
    const issues = auditLedger(runtime.store.getAccount(id)!, runtime.store.listLedger(id));
    return { consistent: issues.length === 0, issues };
  });

  // ---- orders ---------------------------------------------------------------------------
  app.post('/api/orders', async (request) => {
    const input = PlaceOrderSchema.parse(request.body);
    runtime.assertTradable(input.symbol, runtime.store.getAccount(requireAccount(input.accountId))!.source);
    const result = await runtime.execution(input.accountId).placeOrder(input);
    runtime.invalidate('accounts', 'orders', 'positions', 'arena');
    return result;
  });
  app.delete<{ Params: IdParams }>('/api/orders/:id', async (request) => {
    const order = runtime.store.getOrder(request.params.id);
    if (!order) throw Object.assign(new Error('Unknown order'), { statusCode: 404 });
    await runtime.execution(order.accountId).cancelOrder(order.id);
    runtime.invalidate('accounts', 'orders');
    return { ok: true };
  });
  app.post<{ Params: PositionParams }>('/api/positions/:accountId/:symbol/close', async (request) => {
    const { fraction, clientOrderId } = ClosePositionSchema.parse(request.body);
    const symbol = SymbolSchema.parse(request.params.symbol);
    const accountId = requireAccount(request.params.accountId);
    const result = await runtime.execution(accountId).closePosition(accountId, symbol, fraction, clientOrderId);
    runtime.invalidate('accounts', 'orders', 'positions', 'arena');
    return result;
  });
  app.put<{ Params: PositionParams }>('/api/positions/:accountId/:symbol/protection', async (request) => {
    const { stopLoss, takeProfit, trailingStop } = ProtectionSchema.parse(request.body);
    const symbol = SymbolSchema.parse(request.params.symbol);
    const accountId = requireAccount(request.params.accountId);
    if (runtime.store.getAccount(accountId)!.venue !== 'PAPER') throw new Error('Change stops for exchange positions on OKX');
    // An absent trailingStop keeps the current one, so a client that only edits SL/TP cannot drop it.
    const kept = trailingStop === undefined ? runtime.engine.protectionOfSync(accountId, symbol).trailingStop : trailingStop;
    runtime.engine.setProtectionSync(accountId, symbol, { stopLoss, takeProfit, trailingStop: kept });
    runtime.invalidate('orders', 'positions');
    return { ok: true };
  });

  // ---- AI providers ---------------------------------------------------------------------
  const requireProvider = (id: string) => {
    const provider = runtime.store.getProvider(id);
    if (!provider) throw Object.assign(new Error('Unknown provider'), { statusCode: 404 });
    return provider;
  };
  const editableProvider = (id: string) => {
    if (id === MOCK_PROVIDER_ID) throw new Error('The mock provider is built in');
    return requireProvider(id);
  };

  app.get('/api/providers', async () => runtime.providerViews());
  app.post('/api/providers', async (request) => {
    const input = ProviderInputSchema.parse(request.body);
    const provider = { ...input, id: `prov_${randomUUID()}`, createdAt: Date.now() };
    runtime.store.insertProvider(provider);
    runtime.invalidate('providers');
    return runtime.providerViews().find((p) => p.id === provider.id);
  });
  app.put<{ Params: IdParams }>('/api/providers/:id', async (request) => {
    const current = editableProvider(request.params.id);
    const input = ProviderInputSchema.parse(request.body);
    const provider = { ...current, ...input, extraHeaders: input.extraHeaders ?? current.extraHeaders ?? {} };
    runtime.store.saveProvider(provider);
    runtime.invalidate('providers');
    return runtime.providerViews().find((p) => p.id === provider.id);
  });
  app.delete<{ Params: IdParams }>('/api/providers/:id', async (request) => {
    const { id } = editableProvider(request.params.id);
    if (runtime.store.listAgents().some((a) => a.providerId === id)) throw new Error('Agents still use this provider');
    runtime.clearKey(id);
    runtime.store.deleteProvider(id);
    runtime.invalidate('providers');
    return { ok: true };
  });
  app.put<{ Params: IdParams }>('/api/providers/:id/key', async (request) => {
    const { id } = editableProvider(request.params.id);
    runtime.setKey(id, ProviderKeySchema.parse(request.body).apiKey);
    return { ok: true };
  });
  app.delete<{ Params: IdParams }>('/api/providers/:id/key', async (request) => {
    runtime.clearKey(editableProvider(request.params.id).id);
    return { ok: true };
  });
  app.post<{ Params: IdParams; Body: { model?: string } }>('/api/providers/:id/test', async (request) => {
    const config = requireProvider(request.params.id);
    const model = z.object({ model: z.string().optional() }).parse(request.body ?? {}).model ?? config.defaultModel;
    const provider = runtime.providerFor(config);
    if (config.kind === 'MOCK') return { ok: true, sample: 'Mock provider needs no connection.' };
    const response = await provider.complete({
      model,
      // OpenAI and DeepSeek reject JSON mode unless the prompt itself mentions JSON.
      system: 'Reply with this JSON object and nothing else: {"ok":true}',
      user: 'Connection test. Answer in JSON.',
      context: undefined as never,
    });
    return { ok: true, sample: runtime.redact(response.text.slice(0, 120)) };
  });

  // ---- agents ---------------------------------------------------------------------------
  const requireAgent = (id: string) => {
    const agent = runtime.store.getAgent(id);
    if (!agent) throw Object.assign(new Error('Unknown agent'), { statusCode: 404 });
    return agent;
  };

  app.get('/api/agents', async () => runtime.store.listAgents());
  app.post('/api/agents', async (request) => runtime.createAgent(AgentInputSchema.parse(request.body)));
  app.put<{ Params: IdParams }>('/api/agents/:id', async (request) => {
    const input = AgentInputSchema.parse(request.body);
    runtime.assertAgentRefs(input.providerId, input.accountId, input.symbol);
    const agent = { ...requireAgent(request.params.id), ...input };
    runtime.store.saveAgent(agent);
    runtime.invalidate('agents', 'arena');
    return agent;
  });
  app.delete<{ Params: IdParams }>('/api/agents/:id', async (request) => {
    const agent = requireAgent(request.params.id);
    if (agent.running) throw new Error('Stop the agent first');
    runtime.store.deleteAgent(agent.id);
    runtime.invalidate('agents', 'decisions', 'arena');
    return { ok: true };
  });

  const lifecycle = { start: 'start', pause: 'pause', resume: 'resume', stop: 'stop' } as const;
  for (const action of Object.keys(lifecycle) as (keyof typeof lifecycle)[]) {
    app.post<{ Params: IdParams }>(`/api/agents/:id/${action}`, async (request) => {
      const agent = requireAgent(request.params.id);
      if (action === 'start' || action === 'resume') runtime.assertCanRun(agent);
      runtime.runner[action](agent.id);
      return requireAgent(request.params.id);
    });
  }
  const runOnce = (trigger: 'ANALYZE' | 'RUN_ONCE') => async (request: FastifyRequest<{ Params: IdParams }>, reply: FastifyReply) => {
    const record = await runtime.runner.runCycle(requireAgent(request.params.id).id, trigger);
    if (!record) return reply.code(409).send({ error: 'A cycle is already running for this agent' });
    return record;
  };
  app.post<{ Params: IdParams }>('/api/agents/:id/analyze', runOnce('ANALYZE'));
  app.post<{ Params: IdParams }>('/api/agents/:id/run', runOnce('RUN_ONCE'));
  app.post('/api/agents/stop-all', async () => {
    const stopped = runtime.runner.stopAll();
    runtime.runner.retireAllSignals();
    runtime.setPlaying(false);
    runtime.disarm();
    return { stopped };
  });

  // ---- master password ------------------------------------------------------------------
  app.get('/api/vault', async () => ({ status: runtime.vaultStatus() }));
  app.post('/api/vault', async (request) => {
    await runtime.enableVault(VaultCreateSchema.parse(request.body).password);
    return { status: runtime.vaultStatus() };
  });
  app.post('/api/vault/unlock', async (request) => {
    const { failed } = await runtime.unlock(VaultPasswordSchema.parse(request.body).password);
    return { status: runtime.vaultStatus(), failed };
  });
  app.post('/api/vault/password', async (request) => {
    const { current, next } = VaultChangeSchema.parse(request.body);
    await runtime.changePassword(current, next);
    return { status: runtime.vaultStatus() };
  });
  app.post('/api/vault/disable', async (request) => {
    await runtime.disableVault(VaultPasswordSchema.parse(request.body).password);
    return { status: runtime.vaultStatus() };
  });
  app.post('/api/vault/reset', async (request) => {
    runtime.resetVault(VaultResetSchema.parse(request.body).confirmation);
    return { status: runtime.vaultStatus() };
  });

  // ---- exchange accounts ----------------------------------------------------------------
  app.get('/api/exchange/status', async () => runtime.exchangeStatuses());
  app.post('/api/exchange/okx/connect', async (request) => {
    const account = await runtime.connectOkx(OkxConnectSchema.parse(request.body));
    return accountView(account.id);
  });
  app.post<{ Params: IdParams }>('/api/exchange/okx/:id/disconnect', async (request) => {
    runtime.disconnectExchange(requireAccount(request.params.id));
    return { ok: true };
  });
  app.post<{ Params: IdParams }>('/api/exchange/okx/:id/arm', async (request) => {
    runtime.arm(requireAccount(request.params.id), ArmSchema.parse(request.body).confirmation);
    return { ok: true };
  });
  app.post<{ Params: IdParams }>('/api/exchange/okx/:id/disarm', async (request) => {
    runtime.disarm(requireAccount(request.params.id));
    return { ok: true };
  });

  // ---- suggestions ----------------------------------------------------------------------
  const requireSignal = (id: string) => {
    const signal = runtime.store.getDecision(id);
    if (!signal) throw Object.assign(new Error('Unknown suggestion'), { statusCode: 404 });
    if (signal.outcome !== 'SIGNAL') throw Object.assign(new Error('This suggestion is no longer pending'), { statusCode: 409 });
    return signal;
  };
  app.get('/api/signals', async () => runtime.store.listPendingSignals());
  app.post<{ Params: IdParams }>('/api/signals/:id/execute', async (request) => {
    const signal = requireSignal(request.params.id);
    runtime.assertCanExecuteSignal(signal);
    return runtime.runner.executeSignal(signal.id);
  });
  app.post<{ Params: IdParams }>('/api/signals/:id/dismiss', async (request) => runtime.runner.dismissSignal(requireSignal(request.params.id).id));

  app.get('/api/decisions', async (request) => {
    const { agentId, limit } = DecisionQuery.parse(request.query);
    return runtime.store.listDecisions({ limit, ...(agentId ? { agentId } : {}) });
  });
  app.get('/api/arena', async () => runtime.arena());

  // ---- demo & replay --------------------------------------------------------------------
  app.post('/api/demo/start', async () => {
    runtime.startDemo();
    return { demoActive: true };
  });
  app.post('/api/demo/reset', async () => {
    await runtime.resetDemo();
    return { demoActive: true };
  });
  app.post('/api/demo/exit', async () => {
    await runtime.exitDemo();
    return { demoActive: false };
  });
  app.post('/api/replay', async (request) => {
    const { playing, speed, steps } = ReplayControlSchema.parse(request.body);
    if (speed !== undefined) runtime.setSpeed(speed);
    if (playing !== undefined) runtime.setPlaying(playing);
    if (steps !== undefined) await runtime.stepReplay(steps);
    return runtime.replayState();
  });

  const webDist = join(REPO_ROOT, 'apps/web/dist');
  if (existsSync(webDist)) void app.register(fastifyStatic, { root: webDist });

  return { app, token };
}
