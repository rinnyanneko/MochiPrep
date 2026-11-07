import fastify, { type FastifyInstance, type FastifyReply, type FastifyRequest } from 'fastify';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import fastifyStatic from '@fastify/static';
import { auditLedger } from '@mochiprep/core';
import {
  AgentInputSchema,
  ClosePositionSchema,
  CreateAccountSchema,
  INSTRUMENTS,
  PlaceOrderSchema,
  ProtectionSchema,
  ProviderInputSchema,
  ProviderKeySchema,
  ReplayControlSchema,
  SYMBOLS,
  TIMEFRAMES,
  type ServerEvent,
} from '@mochiprep/shared';
import { randomUUID } from 'node:crypto';
import { z, ZodError } from 'zod';
import { REPO_ROOT, type ServerConfig } from './config';
import { MOCK_PROVIDER_ID, type Runtime } from './runtime';

const CandleQuery = z.object({
  source: z.enum(['REPLAY', 'LIVE']),
  symbol: z.enum(SYMBOLS),
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
    return runtime.candles(source, symbol, timeframe, limit);
  });
  app.get('/api/market/quotes', async (request) => runtime.quotes(SourceQuery.parse(request.query).source));

  // ---- accounts -------------------------------------------------------------------------
  const accountView = (id: string) => ({ ...runtime.store.getAccount(id)!, balance: runtime.engine.balanceOf(id) });
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
    runtime.store.transaction(() => runtime.store.deleteAccountData(id, false));
    runtime.invalidate('accounts', 'orders', 'positions');
    return { ok: true };
  });
  app.get<{ Params: IdParams }>('/api/accounts/:id/positions', async (r) => runtime.engine.positionViews(requireAccount(r.params.id)));
  app.get<{ Params: IdParams }>('/api/accounts/:id/closed-positions', async (r) => {
    return runtime.store.listClosedPositions(requireAccount(r.params.id), LimitQuery.parse(r.query).limit);
  });
  app.get<{ Params: IdParams }>('/api/accounts/:id/orders', async (r) => runtime.store.listOrders(requireAccount(r.params.id), LimitQuery.parse(r.query).limit));
  app.get<{ Params: IdParams }>('/api/accounts/:id/fills', async (r) => runtime.store.listFills(requireAccount(r.params.id), LimitQuery.parse(r.query).limit));
  app.get<{ Params: IdParams }>('/api/accounts/:id/ledger', async (r) => runtime.store.listLedger(requireAccount(r.params.id)).slice(-LimitQuery.parse(r.query).limit).reverse());
  app.get<{ Params: IdParams }>('/api/accounts/:id/equity', async (r) => runtime.store.listEquity(requireAccount(r.params.id), LimitQuery.parse(r.query).limit));
  app.get<{ Params: IdParams }>('/api/accounts/:id/audit', async (r) => {
    const id = requireAccount(r.params.id);
    const issues = auditLedger(runtime.store.getAccount(id)!, runtime.store.listLedger(id));
    return { consistent: issues.length === 0, issues };
  });

  // ---- orders ---------------------------------------------------------------------------
  app.post('/api/orders', async (request) => {
    const input = PlaceOrderSchema.parse(request.body);
    requireAccount(input.accountId);
    const result = await runtime.engine.placeOrder(input);
    runtime.invalidate('accounts', 'orders', 'positions', 'arena');
    return result;
  });
  app.delete<{ Params: IdParams }>('/api/orders/:id', async (request) => {
    await runtime.engine.cancelOrder(request.params.id);
    runtime.invalidate('accounts', 'orders');
    return { ok: true };
  });
  app.post<{ Params: PositionParams }>('/api/positions/:accountId/:symbol/close', async (request) => {
    const { fraction, clientOrderId } = ClosePositionSchema.parse(request.body);
    const symbol = z.enum(SYMBOLS).parse(request.params.symbol);
    const result = runtime.engine.closePosition(requireAccount(request.params.accountId), symbol, fraction, clientOrderId);
    runtime.invalidate('accounts', 'orders', 'positions', 'arena');
    return result;
  });
  app.put<{ Params: PositionParams }>('/api/positions/:accountId/:symbol/protection', async (request) => {
    const { stopLoss, takeProfit } = ProtectionSchema.parse(request.body);
    const symbol = z.enum(SYMBOLS).parse(request.params.symbol);
    runtime.engine.setProtection(requireAccount(request.params.accountId), symbol, stopLoss, takeProfit);
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
    return provider;
  });
  app.put<{ Params: IdParams }>('/api/providers/:id', async (request) => {
    const provider = { ...editableProvider(request.params.id), ...ProviderInputSchema.parse(request.body) };
    runtime.store.saveProvider(provider);
    runtime.invalidate('providers');
    return provider;
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
      system: 'Reply with the single word OK.',
      user: 'Connection test.',
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
    runtime.assertAgentRefs(input.providerId, input.accountId);
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
      runtime.runner[action](requireAgent(request.params.id).id);
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
    runtime.setPlaying(false);
    return { stopped };
  });

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
