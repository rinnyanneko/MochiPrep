import { AgentInputSchema, ProviderInputSchema, ProviderKeySchema, DEFAULT_RISK_SETTINGS } from '@mochiprep/shared';
import { parseDecision } from '@mochiprep/agents';
import { z, type ZodType } from 'zod';
import { formatZodError } from '../src/app.js';

const omit = (o: Record<string, unknown>, key: string) => Object.fromEntries(Object.entries(o).filter(([k]) => k !== key));
export function aiValidationFixture() {
  const provider = { name: ' Gateway ', kind: 'OPENAI', baseUrl: null, defaultModel: ' model ', inputPricePerMTok: null, outputPricePerMTok: 0 };
  const agent = { name: ' Agent ', providerId: 'p', model: ' model ', symbol: 'DOGEUSDT', timeframe: '1m', strategy: '', accountId: 'a', intervalSec: 60 };
  const decision = { symbol: 'DOGEUSDT', action: 'HOLD', orderType: 'MARKET', entryPrice: null, stopLoss: null, takeProfit: null, reason: 'Waiting', confidence: 0.5, expiresAt: null };
  const headers = [undefined, {}, { 'X-Gateway': 'secret', 'x-gateway': 'last', '2': 'two', '1': 'one' }, { Authorization: 'secret', HOST: 'x' }, { 'invalid name': 'x' }, { 'X-Empty': '' }, { 'X-Line': 'x\ny' }, { 'X-Long': 'x'.repeat(2001) }, Object.fromEntries(Array.from({length:21}, (_,i)=>[`x-${i}`, 'v'])), null, [], { 'X-Number': 1 }, { ['x'.repeat(101)]: 'v' }];
  const api: { schema: string; input?: unknown; output?: unknown; error?: string }[] = [];
  const add = (schema: string, validator: ZodType, inputs: unknown[]) => {
    for (const input of inputs) {
      const parsed = validator.safeParse(input);
      api.push({ schema, input, ...(parsed.success ? { output: parsed.data } : { error: formatZodError(parsed.error) }) });
    }
  };
  add('ProviderInput', ProviderInputSchema, [provider, ...headers.map(extraHeaders=>({...provider, ...(extraHeaders === undefined ? {} : {extraHeaders})})), ...Object.keys(provider).map(key=>omit(provider,key)), ...['MOCK','OPENAI_COMPATIBLE','ANTHROPIC','GEMINI','DEEPSEEK','OPENCODE'].map(kind=>({...provider,kind})), {...provider, baseUrl:'https://example.test/v1'}, {...provider, baseUrl:'bad', inputPricePerMTok:-1, outputPricePerMTok:'2'}, {...provider, extra:1}, null, [], {}]);
  add('ProviderKey', ProviderKeySchema, [{apiKey:' secret '}, {apiKey:''}, {apiKey:'x'.repeat(1001)}, {apiKey:null}, {apiKey:5}, {apiKey:'x',extra:1}, {}, null]);
  add('ProviderTest', z.object({model:z.string().optional()}), [{}, {model:''}, {model:'  x '}, {model:null}, {model:5}, {model:'x',extra:1}, null, []]);
  add('AgentInput', AgentInputSchema, [agent, ...Object.keys(agent).map(key=>omit(agent,key)), ...[0,9,10,86400,86401,1.5,'60'].map(intervalSec=>({...agent,intervalSec})), {...agent,executionMode:'AUTO',useNews:true,allowTrailingStop:true}, {...agent, executionMode:null, useNews:1,allowTrailingStop:'true'}, {...agent,strategy:'x'.repeat(8001),timeframe:'2m',symbol:'btcusdt'}, {...agent,extra:1}, {...agent,risk:{}}, {...agent,risk:{maxLeverage:101,maxRiskPerTradePct:0,maxDailyLossPct:101,maxOpenPositions:1.5,requireStopLoss:null,maxQuoteAgeSec:0,maxNotional:0,extra:1}}, null, [], {}]);
  const modelInputs: unknown[] = [decision, {...decision,trailingStop:null}, {...decision,trailingStop:{kind:'RATIO',callback:0.1,activationPrice:null}}, {...decision,trailingStop:{kind:'SPREAD',callback:10,activationPrice:100}}, ...Object.keys(decision).map(key=>omit(decision,key)), {...decision,entryPrice:'1',stopLoss:0,takeProfit:-1,confidence:2}, {...decision,action:'BUY',orderType:'STOP'}, {...decision,trailingStop:{kind:'OTHER',callback:'1',extra:1}}, {...decision, extra:1}, {...decision,action:'LONG',orderType:'LIMIT'}, {...decision,action:'SHORT',orderType:'LIMIT',entryPrice:1}, {...decision,reason:''}, {...decision,reason:'x'.repeat(4001),symbol:'x'.repeat(33)}, ...['2024-02-29T12:30:00Z','2023-02-29T12:30:00Z','2024-01-01T12:30Z','2024-01-01T12:30:00.123+08:00','2024-01-01T24:00:00Z','2024-01-01T12:30:00+0800','2024-01-01T12:30:00','2024-01-01t12:30:00z'].map(expiresAt=>({...decision,expiresAt})), null, [], 1];
  for (const field of ['entryPrice','stopLoss','takeProfit','confidence']) {
    for (const value of [-1,0,1,1.1,'1',false,[],{}]) modelInputs.push({...decision,[field]:value});
  }
  for (const patch of [{reason:''},{confidence:2},{symbol:''},{expiresAt:'bad'},{extra:1},{reason:1},{stopLoss:'1'},{trailingStop:{kind:'RATIO',callback:0,activationPrice:null}},{trailingStop:{kind:'RATIO',callback:1}}]) modelInputs.push({...decision,action:'LONG',orderType:'LIMIT',...patch});
  for (const expiresAt of ['0000-02-29T00:00:00Z','1900-02-29T00:00:00Z','2000-02-29T00:00:00Z','2024-01-01T00:00:00+23:59','2024-01-01T00:00:00+24:00','2024-01-01T00:00:00Z\n']) modelInputs.push({...decision,expiresAt});
  for (const [field,values] of Object.entries({maxLeverage:[0,1,100,101,1.5],maxRiskPerTradePct:[0,0.1,100,101],maxDailyLossPct:[0,100,101],maxOpenPositions:[0,1,50,51,1.5,'1'],maxQuoteAgeSec:[0,1,3600,3601,1.5],maxNotional:[0,0.1,-1,'1']})) {
    add('AgentInput',AgentInputSchema,values.map(value=>({...agent,risk:{...DEFAULT_RISK_SETTINGS,[field]:value}})));
  }
  add('ProviderInput',ProviderInputSchema,[{...provider,extraHeaders:{'bad name':'',HOST:''}},{...provider,extraHeaders:{HOST:5}},{...provider,extraHeaders:{HOST:'x\r\ny'}},{...provider,extraHeaders:{'__proto__':'x',constructor:'x'}}]);
  const text = JSON.stringify(decision);
  const texts = [...modelInputs.map(v=>JSON.stringify(v)), text.replace('Waiting','\\ud83d\\ude00'), `\ufeff ${text} \ufeff`, `\`\`\`json\n${text}\n\`\`\``, `\`\`\`JSON ${text}\`\`\``, `\`\`\`\n${text}\n\`\`\``, `\`\`\`js\n${text}\n\`\`\``, 'not json', `prefix ${text}`, `${text} trailing`];
  // Rust deliberately fails closed on numeric overflow rather than changing workspace serde semantics.
  const overflowTexts = ['1e400','-1e400'].flatMap(value => ['entryPrice','stopLoss','takeProfit','confidence'].map(field => JSON.stringify({...decision,[field]:12345}).replace('12345',value)));
  const safeRejections = overflowTexts.map(text => ({text,nodeResult:parseDecision(text),rustError:'Response is not valid JSON'}));
  return {api, decisions:texts.map(text=>({text,result:parseDecision(text)})), safeRejections};
}
