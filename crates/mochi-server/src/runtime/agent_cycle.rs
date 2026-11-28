//! A cycle crosses the actor boundary only with owned snapshots. Every execution returns to
//! the actor and rechecks ownership, lifecycle, prices, balances and protection.
//!
//! A cycle trades through the engine its account had when it began (Node's `execution()` at
//! the start of `analyzeAndAct`). On an exchange, cancelling stale entries and the final order
//! or protection change go out off the actor and come back as completions.
//!
//! An agent with several pairs runs a [`Batch`]: one leg per pair, each with its own decision
//! record, risk check and execution, around one model call that sees every pair. The legs
//! execute one after another, so each risk check sees what the legs before it used.
use super::{
    agent_execution::{
        position_binding_field, same_protection, settle, settle_acted, target_protection, Acted,
        Outgoing, RiskPosition,
    },
    agents::{CycleReply, CycleTrigger},
    execution::ExecutionTarget,
    Runtime,
};
use crate::{
    agent_parse::{parse_decision, parse_decisions},
    error::ApiError,
};
use futures_util::FutureExt;
use mochi_ai::{
    build_multi_system_prompt, build_multi_user_prompt, build_system_prompt, build_user_prompt,
    indicator_snapshot, redact_secrets, with_owner_message, with_refusal, with_trailing_exit,
    AiProvider, AiRequest, AiResponse, MarketContext, ProviderError,
};
use mochi_core::paper::{PaperStore, StoreFailure};
use mochi_domain::{
    AiAction, AiDecisionData, Candle, DataSource, NewsItem, OrderType, PaperAccount, Quote,
    RiskSettings, ServerEvent, Timeframe, ToastLevel,
};
use mochi_store::rows;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

pub(super) struct Cycle {
    agent: rows::Agent,
    record: Value,
    source: DataSource,
    timeframe: Timeframe,
    generation: u64,
    reply: Option<CycleReply>,
    quote: Option<Quote>,
    analyzed_position_id: Option<String>,
    stale: bool,
    critical_error: Option<String>,
    secrets: Vec<String>,
    /// The engine this cycle trades through; `None` only before it is resolved.
    target: Option<ExecutionTarget>,
    /// The first model request without the owner's message, and that message, kept for one
    /// second try if the reply is refused. Taken by that try, so a cycle asks the model at most
    /// twice.
    first_request: Option<(AiRequest, Option<String>)>,
}
pub(super) enum Completion {
    Candles(Cycle, Result<Vec<Candle>, String>),
    /// An exchange cancelled the agent's unfilled entries from its last cycle.
    Cancelled(Cycle, Vec<Candle>, Result<(), ApiError>),
    News(Cycle, Box<MarketContext>),
    Model(Cycle, Result<AiResponse, ProviderError>),
    /// An exchange answered the decision's order or protection change.
    Acted(Cycle, Box<Acted>),
    BatchCandles(Batch, Vec<Option<Result<Vec<Candle>, String>>>),
    /// The pairs whose stale entries could not be cancelled, each with the first refusal.
    BatchCancelled(Batch, Vec<Option<Vec<Candle>>>, Vec<(String, ApiError)>),
    BatchNews(Batch, Vec<Option<MarketContext>>, Vec<NewsItem>),
    BatchModel(Batch, Result<AiResponse, ProviderError>),
    /// An exchange answered the order or protection change of leg `.1`.
    BatchActed(Batch, usize, Box<Acted>),
}

/// One cycle of an agent with several pairs. `legs[i]` trades `agent.symbols[i]`; a leg leaves
/// `open` once its record is settled, or once its agent or pair changed under it.
pub(super) struct Batch {
    legs: Vec<Cycle>,
    open: Vec<bool>,
    reply: Option<CycleReply>,
    /// The model's answer for each leg, from the shared call.
    decisions: Vec<Option<Result<AiDecisionData, String>>>,
    /// What the shared call was built from, kept for one second try for the refused legs.
    first_prompt: Option<BatchPrompt>,
}

/// The parts of a batch prompt, so a second try can ask about only the legs that were refused.
pub(super) struct BatchPrompt {
    system: String,
    /// Each leg's market data, by leg; `None` for a leg the model was not asked about.
    contexts: Vec<Option<MarketContext>>,
    news: Vec<NewsItem>,
    message: Option<String>,
    trailing: bool,
}

/// Refused by the risk checks, or a reply that did not parse: the outcomes a second try with
/// the reasons can change. Provider errors and data problems are not the model's to fix.
fn refused(record: &Value) -> bool {
    record["outcome"] == "RISK_REJECTED" || record["outcome"] == "INVALID_OUTPUT"
}

/// The reply text a second try quotes back: short, since the reasons matter more.
const REFUSED_REPLY_MAX: usize = 2000;

/// Moves a refused attempt's result into `attempts`, so the record shows both tries, and
/// clears it for the second.
fn keep_attempt(record: &mut Value) {
    let mut attempt = serde_json::Map::new();
    for key in [
        "rawResponse",
        "decision",
        "validationError",
        "risk",
        "outcome",
        "outcomeDetail",
    ] {
        attempt.insert(key.into(), record[key].take());
    }
    if !record["attempts"].is_array() {
        record["attempts"] = json!([]);
    }
    record["attempts"]
        .as_array_mut()
        .expect("just made an array")
        .push(Value::Object(attempt));
    settle(record, "DATA_ERROR", None);
}

/// A second try that could not reach the model leaves the refusal of the first as the outcome,
/// noting the failure: a provider hiccup on a retry the cycle did not need is not a failed cycle.
/// Critical provider errors are left alone by the caller, so they still pause the agent.
fn restore_refusal(record: &mut Value) {
    if record["outcome"] != "AI_ERROR" {
        return;
    }
    let Some(Value::Object(mut attempt)) = record
        .get_mut("attempts")
        .and_then(Value::as_array_mut)
        .and_then(Vec::pop)
    else {
        return;
    };
    let failure = record["outcomeDetail"].as_str().unwrap_or("").to_string();
    for (key, value) in std::mem::take(&mut attempt) {
        record[key] = value;
    }
    let detail = record["outcomeDetail"].as_str().unwrap_or("").to_string();
    record["outcomeDetail"] = json!(format!(
        "{detail} (asking the model again failed: {failure})"
    ));
    if record["attempts"].as_array().is_some_and(Vec::is_empty) {
        record.as_object_mut().map(|o| o.remove("attempts"));
    }
}

/// Adds one model call's time, tokens and cost to a record, which may already count an earlier
/// call of the same cycle.
fn add_usage(
    record: &mut Value,
    config: &mochi_ai::ProviderConfig,
    response: &Result<AiResponse, ProviderError>,
    latency: u64,
    tokens: bool,
) {
    record["latencyMs"] = json!(record["latencyMs"].as_u64().unwrap_or(0) + latency);
    let Ok(r) = response else { return };
    if !tokens {
        return;
    }
    let sum = |before: &Value, now: Option<f64>| match (before.as_f64(), now) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    };
    record["inputTokens"] = json!(sum(&record["inputTokens"], r.input_tokens));
    record["outputTokens"] = json!(sum(&record["outputTokens"], r.output_tokens));
    // Each call's estimate is already rounded to 6 places, so the sum is too; added as decimals.
    let cost = estimate_cost(config, r.input_tokens, r.output_tokens)
        .and_then(|c| mochi_decimal::Dec::parse(&c).ok());
    let before = record["estimatedCost"]
        .as_str()
        .and_then(|c| mochi_decimal::Dec::parse(c).ok());
    if let Some(total) = match (before, cost) {
        (Some(a), Some(b)) => Some(a.add(&b)),
        (a, b) => a.or(b),
    } {
        record["estimatedCost"] = json!(total.to_fixed_dp(6));
    }
}

/// One model call, with its time in ms. A panicking provider is an error, not a crash.
async fn ask(
    provider: Arc<dyn AiProvider>,
    request: &AiRequest,
) -> (Result<AiResponse, ProviderError>, u64) {
    let started = Instant::now();
    let response = std::panic::AssertUnwindSafe(async { provider.complete(request).await })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            Err(ProviderError::new(
                mochi_ai::ProviderErrorCode::BadResponse,
                "AI provider panicked",
            ))
        });
    (response, started.elapsed().as_millis() as u64)
}
impl Batch {
    fn open_legs(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.legs.len()).filter(|&i| self.open[i])
    }
    fn settle(&mut self, i: usize, outcome: &str, detail: Option<String>) {
        settle(&mut self.legs[i].record, outcome, detail);
        self.open[i] = false;
    }
    /// A data error on the open leg that trades `symbol`, if there is one.
    fn settle_pair(&mut self, symbol: &str, detail: &str) {
        let leg = self
            .open_legs()
            .find(|&i| self.legs[i].agent.symbol == symbol);
        if let Some(i) = leg {
            let detail = self.legs[i].clean(detail);
            self.settle(i, "DATA_ERROR", Some(detail));
        }
    }
}
impl Cycle {
    fn clean(&self, text: &str) -> String {
        redact_secrets(text, self.secrets.iter().map(String::as_str))
    }

    fn provider_error(&mut self, error: ProviderError) {
        let detail = self.clean(&error.message);
        let display = if error.critical() {
            self.critical_error = Some(error.message);
            format!("[critical] {detail}")
        } else {
            detail
        };
        settle(&mut self.record, "AI_ERROR", Some(display));
    }

    fn target(&self) -> ExecutionTarget {
        self.target
            .clone()
            .expect("the engine is resolved before any work leaves the actor")
    }
}
impl Completion {
    pub(super) fn identity(&self) -> (String, String) {
        let c = match self {
            Self::Candles(c, _)
            | Self::Cancelled(c, _, _)
            | Self::News(c, _)
            | Self::Model(c, _)
            | Self::Acted(c, _) => c,
            Self::BatchCandles(b, _)
            | Self::BatchCancelled(b, _, _)
            | Self::BatchNews(b, _, _)
            | Self::BatchModel(b, _)
            | Self::BatchActed(b, _, _) => &b.legs[0],
        };
        (c.agent.id.clone(), c.record["id"].as_str().unwrap().into())
    }
}
/// Why saving a cycle's records failed: a step of the runtime's own, or the transaction.
enum SaveFailure {
    Api(ApiError),
    Store(StoreFailure),
}
impl From<StoreFailure> for SaveFailure {
    fn from(e: StoreFailure) -> Self {
        SaveFailure::Store(e)
    }
}

fn clean_record(record: &mut Value, secrets: &[String]) {
    let clean = |v: &mut Value| {
        if let Value::String(s) = v {
            *s = redact_secrets(s, secrets.iter().map(String::as_str));
        }
    };
    // Only free text crosses this boundary: short keys must not rewrite IDs, enums,
    // validated dates, or decimal quantities needed to execute a saved signal.
    for path in [
        "/rawResponse",
        "/validationError",
        "/outcomeDetail",
        "/newsError",
        "/decision/reason",
        "/ownerMessage",
    ] {
        if let Some(v) = record.pointer_mut(path) {
            clean(v);
        }
    }
    // An approved symbol is the trusted agent symbol. A mismatched model symbol can echo
    // arbitrary credentials and is retained only as rejected diagnostic text.
    if record["decision"]["symbol"] != record["symbol"] {
        if let Some(v) = record.pointer_mut("/decision/symbol") {
            clean(v);
        }
    }
    if let Some(checks) = record
        .pointer_mut("/risk/checks")
        .and_then(Value::as_array_mut)
    {
        for check in checks {
            clean(&mut check["detail"]);
        }
    }
    if let Some(reasons) = record
        .pointer_mut("/risk/rejectionReasons")
        .and_then(Value::as_array_mut)
    {
        for reason in reasons {
            clean(reason);
        }
    }
    if let Some(attempts) = record.get_mut("attempts").and_then(Value::as_array_mut) {
        for attempt in attempts {
            clean_record(attempt, secrets);
        }
    }
    if let Some(news) = record["news"].as_array_mut() {
        for item in news {
            for field in ["id", "source", "title", "url"] {
                clean(&mut item[field]);
            }
        }
    }
}
impl Runtime {
    pub(super) fn begin_cycle(
        &mut self,
        id: &str,
        trigger: CycleTrigger,
        reply: Option<CycleReply>,
    ) -> Result<(), ApiError> {
        if self.is_agent_busy(id) {
            if let Some(reply) = reply {
                let _ = reply.send(Ok(None));
            }
            return Ok(());
        }
        let mut agent = self.require_agent(id)?;
        let account = self.require_account(&agent.account_id)?;
        let timeframe: Timeframe =
            serde_json::from_value(json!(agent.timeframe)).map_err(|e| self.api_error(e))?;
        if agent.symbols.len() > 1 {
            return self.begin_batch(agent, account, timeframe, trigger, reply);
        }
        let now = self.market_now(account.source);
        let record = new_record(&agent, &account, now, trigger);
        let token = record["id"].as_str().unwrap().to_string();
        agent.status = "ANALYZING".into();
        agent.status_detail = None;
        self.save_agent(&agent)?;
        let mut c = Cycle {
            generation: *self.agents.generations.get(id).unwrap_or(&0),
            agent,
            record,
            source: account.source,
            timeframe,
            reply,
            quote: None,
            analyzed_position_id: None,
            stale: false,
            critical_error: None,
            secrets: self.secrets(),
            target: None,
            first_request: None,
        };
        self.agents.active.insert(id.into(), token);
        // Node's `execution(accountId)`: an exchange account without a session cannot trade.
        match self.execution(&account) {
            Ok(target) => c.target = Some(target),
            Err(e) => {
                let detail = c.clean(e.message());
                settle(&mut c.record, "DATA_ERROR", Some(detail));
                self.finish_cycle(c);
                return Ok(());
            }
        }
        let Some(quote) = self.market_quote(c.source, &c.agent.symbol) else {
            settle(
                &mut c.record,
                "DATA_ERROR",
                Some("No quote available yet".into()),
            );
            self.finish_cycle(c);
            return Ok(());
        };
        c.record["snapshot"] = json!(quote);
        c.quote = Some(quote);
        let live = self.live.clone();
        let candles = if c.source == DataSource::Replay {
            Some(self.replay_candles(&c.agent.symbol, timeframe, 200))
        } else {
            None
        };
        self.agents.pending.push(Box::pin(async move {
            // Include future construction as well as every poll of external feed work.
            let result = std::panic::AssertUnwindSafe(async {
                match candles {
                    Some(c) => Ok(c),
                    None => live.candles(&c.agent.symbol, c.timeframe, 200).await,
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("Candle provider panicked".into()));
            Completion::Candles(c, result)
        }));
        Ok(())
    }
    pub(super) fn complete_cycle(&mut self, completion: Completion) {
        match completion {
            Completion::Candles(mut c, candles) => match self.closed_candles(&mut c, candles) {
                Ok(Some(closed)) => self.cancel_stale_entries(c, closed),
                Ok(None) => self.finish_cycle(c),
                Err(e) => self.data_error(c, &e),
            },
            Completion::Cancelled(c, _, Err(e)) => {
                let e =
                    ApiError::bad_request(format!("Could not read the account: {}", e.message()));
                self.data_error(c, &e)
            }
            Completion::Cancelled(c, closed, Ok(())) => self.read_account(c, closed),
            Completion::News(c, context) => self.begin_model(c, *context),
            Completion::Model(mut c, response) => match self.complete_model(&mut c, response) {
                Ok(None) => self.finish_or_retry(c),
                Ok(Some((engine, act))) => {
                    self.agents.pending.push(Box::pin(async move {
                        let acted = act.run(engine).await;
                        Completion::Acted(c, Box::new(acted))
                    }));
                }
                Err(e) => self.data_error(c, &e),
            },
            Completion::Acted(mut c, acted) => {
                self.own_order_answered(&c.agent.id, &c.agent.symbol, &acted);
                settle_acted(&mut c.record, *acted);
                self.finish_cycle(c);
            }
            Completion::BatchCandles(b, candles) => self.batch_candles(b, candles),
            Completion::BatchCancelled(mut b, closed, failed) => {
                for (symbol, e) in &failed {
                    let detail = format!("Could not read the account: {}", e.message());
                    b.settle_pair(symbol, &detail);
                }
                self.read_batch(b, closed)
            }
            Completion::BatchNews(b, contexts, news) => self.begin_batch_model(b, contexts, news),
            Completion::BatchModel(b, response) => self.complete_batch_model(b, response),
            Completion::BatchActed(mut b, i, acted) => {
                let leg = &b.legs[i].agent;
                self.own_order_answered(&leg.id, &leg.symbol, &acted);
                settle_acted(&mut b.legs[i].record, *acted);
                self.advance_batch(b, i + 1)
            }
        }
    }
    fn data_error(&mut self, mut c: Cycle, e: &ApiError) {
        let detail = c.clean(e.message());
        settle(&mut c.record, "DATA_ERROR", Some(detail));
        self.finish_cycle(c);
    }
    /// Unfilled entries from the agent's last cycle expire here. A paper account cancels them
    /// on the actor; an exchange cancels them off it, and the cycle carries on when it answers.
    fn cancel_stale_entries(&mut self, c: Cycle, closed: Vec<Candle>) {
        let target = c.target();
        let stale = match self.venue_open_orders(&target, &c.agent.account_id) {
            Ok(orders) => orders
                .into_iter()
                .filter(|order| {
                    order.agent_id.as_deref() == Some(&c.agent.id)
                        && order.order_type == OrderType::Limit
                        && !order.reduce_only
                })
                .map(|order| order.id)
                .collect::<Vec<_>>(),
            Err(e) => return self.data_error(c, &e),
        };
        match target {
            ExecutionTarget::Paper => {
                for id in stale {
                    if let Err(e) = self.engine.cancel_order(&id) {
                        let e = self.api_error(e);
                        return self.data_error(c, &e);
                    }
                }
                self.read_account(c, closed);
            }
            ExecutionTarget::Exchange(_) if stale.is_empty() => self.read_account(c, closed),
            ExecutionTarget::Exchange(_) => {
                let engine = self.engine_for(target);
                self.agents.pending.push(Box::pin(async move {
                    let mut result = Ok(());
                    for id in stale {
                        result = engine.cancel_order(id).await;
                        if result.is_err() {
                            break;
                        }
                    }
                    Completion::Cancelled(c, closed, result)
                }));
            }
        }
    }
    /// The account state the model is shown, then news or the model itself.
    fn read_account(&mut self, mut c: Cycle, closed: Vec<Candle>) {
        match self.prepare_model(&mut c, closed) {
            Ok(Some(mut context)) => {
                if c.agent.use_news && c.source == DataSource::Live {
                    let news = self.news.clone();
                    let sources = self.news_sources();
                    let base = self.news_base_currency(&c.agent.symbol);
                    self.agents.pending.push(Box::pin(async move {
                        match sources {
                            Ok(sources) => match std::panic::AssertUnwindSafe(news.for_symbol(
                                sources,
                                &base,
                                mochi_market::news::DEFAULT_NEWS_LIMIT,
                            ))
                            .catch_unwind()
                            .await
                            {
                                Ok(n) => {
                                    c.record["news"] = json!(n.items);
                                    c.record["newsError"] = json!(n.error);
                                    context.news = Some(n.items);
                                }
                                Err(_) => {
                                    c.record["news"] = json!([]);
                                    c.record["newsError"] = json!("Could not load news");
                                    context.news = Some(vec![]);
                                }
                            },
                            Err(e) => {
                                c.record["news"] = json!([]);
                                c.record["newsError"] = json!(c.clean(e.message()));
                                context.news = Some(vec![]);
                            }
                        }
                        Completion::News(c, Box::new(context))
                    }));
                } else {
                    self.begin_model(c, context);
                }
            }
            Ok(None) => self.finish_cycle(c),
            Err(e) => self.data_error(c, &e),
        }
    }
    fn begin_model(&mut self, mut c: Cycle, context: MarketContext) {
        // Resolve after optional news: keys/provider settings may change during that wait.
        let (provider, config) = match self.resolve_provider(&c.agent) {
            Ok(p) => p,
            Err(e) => {
                c.record["latencyMs"] = json!(0);
                c.provider_error(e);
                self.finish_cycle(c);
                return;
            }
        };
        c.secrets.extend(self.secrets());
        c.record["providerKind"] = json!(config.kind);
        // The mock AI was removed, so a new record never comes from it. Old ones keep their flag.
        c.record["isMock"] = json!(false);
        // Cleaned like the rest of the record: a key pasted into a message never reaches the
        // model or the history.
        let message = self.agents.take_messages(&c.agent.id).map(|m| c.clean(&m));
        if let Some(message) = &message {
            c.record["ownerMessage"] = json!(message);
        }
        let trail = self.trails_instead(&c.agent);
        let trailing = c.agent.allow_trailing_stop || trail;
        let base = AiRequest {
            model: c.agent.model.clone(),
            system: with_trailing_exit(build_system_prompt(trailing), trail),
            user: build_user_prompt(&context, trailing),
            context: Some(context),
        };
        let mut request = base.clone();
        request.user = with_owner_message(request.user, message.as_deref());
        c.first_request = Some((base, message));
        self.send_model(c, provider, config, request);
    }
    fn send_model(
        &mut self,
        mut c: Cycle,
        provider: Arc<dyn AiProvider>,
        config: mochi_ai::ProviderConfig,
        request: AiRequest,
    ) {
        self.agents.pending.push(Box::pin(async move {
            let (response, latency) = ask(provider, &request).await;
            add_usage(&mut c.record, &config, &response, latency, true);
            Completion::Model(c, response)
        }));
    }
    /// A refused reply gets one second try, told what was refused and why; anything else, or a
    /// second refusal, finishes the cycle.
    fn finish_or_retry(&mut self, mut c: Cycle) {
        if c.critical_error.is_none() {
            restore_refusal(&mut c.record);
        }
        if !refused(&c.record) || !self.cycle_owned(&c).unwrap_or(false) {
            return self.finish_cycle(c);
        }
        let Some((mut request, message)) = c.first_request.take() else {
            return self.finish_cycle(c);
        };
        let (provider, config) = match self.resolve_provider(&c.agent) {
            Ok(p) => p,
            Err(_) => return self.finish_cycle(c),
        };
        // Cleaned again with the secrets known now, which may include a key added since the
        // reply came, and only then shortened, so a cut can never leave a key's prefix.
        c.secrets.extend(self.secrets());
        let reply = truncate_utf16(
            &c.clean(c.record["rawResponse"].as_str().unwrap_or("")),
            REFUSED_REPLY_MAX,
        );
        let why = c.clean(c.record["outcomeDetail"].as_str().unwrap_or(""));
        let message = message.map(|m| c.clean(&m));
        keep_attempt(&mut c.record);
        request.user = with_refusal(
            with_owner_message(request.user, message.as_deref()),
            &reply,
            &[(c.agent.symbol.clone(), why)],
        );
        self.send_model(c, provider, config, request);
    }
    /// The closed candles the model is shown, or `None` once the record is settled.
    fn closed_candles(
        &mut self,
        c: &mut Cycle,
        candles: Result<Vec<Candle>, String>,
    ) -> Result<Option<Vec<Candle>>, ApiError> {
        if !self.cycle_owned(c)? {
            return Ok(None);
        }
        let closed: Vec<_> = match candles {
            Ok(c) => c.into_iter().filter(|c| c.closed).collect(),
            Err(e) => {
                let detail = c.clean(&format!("Could not load candles: {e}"));
                settle(&mut c.record, "DATA_ERROR", Some(detail));
                return Ok(None);
            }
        };
        if closed.len() < 30 {
            settle(
                &mut c.record,
                "DATA_ERROR",
                Some(format!("Only {} closed candles, need 30", closed.len())),
            );
            return Ok(None);
        }
        Ok(Some(closed))
    }
    fn prepare_model(
        &mut self,
        c: &mut Cycle,
        closed: Vec<Candle>,
    ) -> Result<Option<MarketContext>, ApiError> {
        let target = c.target();
        let balance = self.venue_balance(&target, &c.agent.account_id)?;
        let position = self
            .venue_positions(&target, &c.agent.account_id)?
            .into_iter()
            .find(|p| p.position.symbol == c.agent.symbol);
        c.analyzed_position_id = position.as_ref().map(|p| p.position.id.clone());
        self.saw_position(&c.agent);
        let risk: RiskSettings =
            serde_json::from_str(c.agent.risk.get()).map_err(|e| self.api_error(e))?;
        let quote = c.quote.clone().unwrap();
        let now = c.record["ts"].as_i64().unwrap();
        let stale = is_stale(
            &quote,
            closed.last().unwrap().time,
            c.timeframe.ms(),
            now,
            risk.max_quote_age_sec,
        )?;
        c.stale = stale.is_some();
        if position.is_none() {
            if let Some(stale) = stale {
                settle(&mut c.record, "DATA_ERROR", Some(stale));
                return Ok(None);
            }
        }
        let indicators = indicator_snapshot(&closed);
        c.record["indicators"] = json!(indicators);
        let context = MarketContext {
            symbol: c.agent.symbol.clone(),
            timeframe: c.timeframe,
            now,
            horizon_sec: c.agent.interval_sec as f64,
            candles: closed,
            indicators,
            quote,
            position,
            balance,
            risk,
            strategy: c.agent.strategy.clone(),
            news: None,
        };
        Ok(Some(context))
    }
    fn complete_model(
        &mut self,
        c: &mut Cycle,
        response: Result<AiResponse, ProviderError>,
    ) -> Result<Option<Outgoing>, ApiError> {
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                c.provider_error(e);
                return Ok(None);
            }
        };
        c.secrets.extend(self.secrets());
        // Parse the original only locally. Truncating first can expose the prefix of a key.
        c.record["rawResponse"] = json!(truncate_utf16(&c.clean(&response.text), 8000));
        self.decide(c, parse_decision(&response.text))
    }
    /// One pair's validated decision through ownership, the risk engine, and then a suggestion
    /// or an order. `None` once the record is settled, or when the leg no longer owns it.
    fn decide(
        &mut self,
        c: &mut Cycle,
        parsed: Result<AiDecisionData, String>,
    ) -> Result<Option<Outgoing>, ApiError> {
        let d = match parsed {
            Ok(d) => d,
            Err(e) => {
                let detail = c.clean(&e);
                c.record["validationError"] = json!(detail);
                settle(&mut c.record, "INVALID_OUTPUT", Some(detail));
                return Ok(None);
            }
        };
        c.record["decision"] = json!(d);
        if !self.cycle_owned(c)? {
            return Ok(None);
        }
        let a = self.require_account(&c.agent.account_id)?;
        let target = c.target();
        let protection = self.current_protection(&c.agent, &target)?;
        let quote = if c.stale {
            None
        } else {
            self.market_quote(c.source, &c.agent.symbol)
        };
        let (risk, checked) = self.assess(
            &c.agent,
            &a,
            &d,
            quote,
            RiskPosition {
                bound_id: c.analyzed_position_id.as_deref(),
                protection: protection.as_ref(),
            },
            &target,
        )?;
        c.record["risk"] = json!(risk);
        if c.record["trigger"] == "ANALYZE" {
            settle(
                &mut c.record,
                "ANALYSIS_ONLY",
                Some("Analyze Now never places orders".into()),
            );
            return Ok(None);
        }
        if !risk.approved {
            settle(
                &mut c.record,
                "RISK_REJECTED",
                Some(risk.rejection_reasons.join(" | ")),
            );
            return Ok(None);
        }
        if d.action == AiAction::Hold {
            settle(&mut c.record, "HOLD", None);
            return Ok(None);
        }
        if let Some(field) = position_binding_field(d.action) {
            let position_id = c.analyzed_position_id.as_deref();
            if checked
                .as_ref()
                .is_some_and(|checked| Some(checked.position_id.as_str()) != position_id)
                || self
                    .position_target(&c.agent, &target, position_id)?
                    .is_none()
            {
                settle(
                    &mut c.record,
                    "ORDER_REJECTED",
                    Some("The position changed since it was analyzed".into()),
                );
                return Ok(None);
            }
            c.record[field] = json!(position_id);
        }
        if d.action == AiAction::Protect {
            let p = protection.ok_or_else(|| {
                ApiError::bad_request("The position changed while it was being checked")
            })?;
            // Identical active OKX protection can still hide an unsent leg to remove.
            if matches!(target, ExecutionTarget::Paper)
                && same_protection(&target_protection(&d, &p), &p)
            {
                settle(
                    &mut c.record,
                    "HOLD",
                    Some("Protection already as requested".into()),
                );
                return Ok(None);
            }
            c.record["protectionBefore"] = json!(p);
        }
        let latest = self.require_agent(&c.agent.id)?;
        if *self.agents.generations.get(&c.agent.id).unwrap_or(&0) != c.generation
            || ((c.record["trigger"] == "SCHEDULE" || c.record["trigger"] == "POSITION_CHANGE")
                && !latest.running)
        {
            settle(
                &mut c.record,
                "ANALYSIS_ONLY",
                Some("Agent was stopped before execution".into()),
            );
            return Ok(None);
        }
        if c.agent.execution_mode == "SIGNAL" {
            let expiry = self
                .market_now(c.source)
                .saturating_add(c.agent.interval_sec.saturating_mul(1000));
            let ai_expiry = d
                .expires_at
                .as_deref()
                .and_then(mochi_core::risk::parse_iso_datetime)
                .unwrap_or(i64::MAX);
            c.record["signalExpiresAt"] = json!(expiry.min(ai_expiry));
            settle(
                &mut c.record,
                "SIGNAL",
                Some("Waiting for you to accept or dismiss".into()),
            );
            return Ok(None);
        }
        let mut a = latest;
        a.status = "EXECUTING".into();
        self.save_agent(&a)?;
        self.execute_decision(&c.agent, &d, &mut c.record, &target, checked)
    }
    fn cycle_owned(&self, c: &Cycle) -> Result<bool, ApiError> {
        let Some(a) =
            rows::get_agent(self.connection(), &c.agent.id).map_err(|e| self.api_error(e))?
        else {
            return Ok(false);
        };
        // The cycle's pair (a leg's own pair) must still be one the agent trades.
        Ok(a.created_at == c.agent.created_at
            && a.account_id == c.agent.account_id
            && a.provider_id == c.agent.provider_id
            && a.symbols.contains(&c.agent.symbol)
            && self
                .engine
                .store()
                .get_account(&a.account_id)
                .map_err(|e| self.api_error(e))?
                .is_some())
    }
    fn finish_cycle(&mut self, mut c: Cycle) {
        let reply = c.reply.take();
        self.finish_legs(vec![c], reply);
    }
    /// Saves every leg's record and settles the agent once: its schedule, its error count and
    /// its status. A batch failed only if every leg did; one critical provider error pauses it.
    fn finish_legs(&mut self, mut legs: Vec<Cycle>, reply: Option<CycleReply>) {
        let (id, generation, source) = {
            let c = &legs[0];
            (c.agent.id.clone(), c.generation, c.source)
        };
        let result = (|| -> Result<Option<Value>, ApiError> {
            let mut owned = Vec::new();
            for c in &mut legs {
                if self.cycle_owned(c)? {
                    c.secrets.extend(self.secrets());
                    clean_record(&mut c.record, &c.secrets);
                    owned.push(c.record.clone());
                }
            }
            if owned.is_empty() {
                if reply.is_some() {
                    return Err(ApiError::conflict(
                        "The agent or its account changed while the cycle was running",
                    ));
                }
                return Ok(None);
            }
            let stale = self
                .agents
                .generations
                .get(&id)
                .copied()
                .unwrap_or_default()
                != generation;
            // One pair can settle as a suggestion while another waits for its second try. If
            // the agent was stopped meanwhile, that suggestion is history only, as a single
            // pair's is when its answer comes after Stop: nothing pending appears after it.
            if stale {
                for record in owned.iter_mut().filter(|r| r["outcome"] == "SIGNAL") {
                    settle(
                        record,
                        "ANALYSIS_ONLY",
                        Some("Agent was stopped before execution".into()),
                    );
                }
            }
            // Each record is saved with the retirement of the suggestion it replaces, in one
            // transaction, so a failed insert leaves that pair's earlier suggestion pending.
            // The other pairs are still saved: an executed order cannot be rolled back, so its
            // record must stay. The first failure is reported once every pair was tried.
            let mut first_error = None;
            let mut replaced = false;
            for record in &owned {
                let replaces = (record["outcome"] == "SIGNAL").then(|| record["symbol"].clone());
                let saved = self
                    .engine
                    .store()
                    .transaction(|| -> Result<(), SaveFailure> {
                        if let Some(symbol) = &replaces {
                            self.retire_signal_rows(
                                |s| s["agentId"] == id.as_str() && s["symbol"] == *symbol,
                                "Replaced by a newer suggestion",
                            )
                            .map_err(SaveFailure::Api)?;
                        }
                        rows::insert_decision(self.connection(), record)
                            .map_err(|e| SaveFailure::Api(self.api_error(e)))
                    })
                    .map_err(|e| match e {
                        SaveFailure::Api(e) => e,
                        SaveFailure::Store(e) => self.api_error(e),
                    });
                match saved {
                    Ok(()) => {
                        replaced |= replaces.is_some();
                        if let Some(log) = &self.operation_logs {
                            let outcome = record["outcome"].as_str().unwrap_or("UNKNOWN");
                            log.record(format_args!("agent cycle -> {outcome}"));
                        }
                    }
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
            if replaced {
                self.invalidate(&[mochi_domain::Topic::Decisions]);
            }
            if let Some(e) = first_error {
                return Err(e);
            }
            let first = owned[0].clone();
            // A cycle can finish after a lifecycle action. Keep its decision in History,
            // but leave the newer schedule and failure state alone.
            if stale {
                self.invalidate_cycle();
                return Ok(Some(first));
            }
            let mut a = self.require_agent(&id)?;
            let is_failure =
                |r: &Value| r["outcome"] == "AI_ERROR" || r["outcome"] == "INVALID_OUTPUT";
            let failed = owned.iter().all(is_failure);
            a.consecutive_errors = if failed {
                a.consecutive_errors.saturating_add(1)
            } else {
                0
            };
            let now = self.market_now(source);
            a.last_run_at = Some(now);
            if a.running {
                a.next_run_at = Some(now.saturating_add(a.interval_sec.saturating_mul(1000)));
            }
            let detail = owned
                .iter()
                .find(|r| is_failure(r))
                .and_then(|r| r["outcomeDetail"].as_str())
                .map(String::from);
            let critical = legs.iter().find_map(|c| c.critical_error.clone());
            // A critical error (no key, refused credentials) pauses even when another pair
            // settled otherwise: every later call to the provider fails the same way.
            if critical.is_some() || (failed && a.consecutive_errors >= 3) {
                let detail = critical
                    .as_deref()
                    .map(|text| legs[0].clean(text))
                    .unwrap_or_else(|| format!("{} failed cycles in a row", a.consecutive_errors));
                a.running = false;
                a.status = "PAUSED".into();
                a.next_run_at = None;
                a.status_detail = Some(format!("Auto-paused: {detail}"));
                self.emit(ServerEvent::Toast {
                    level: ToastLevel::Error,
                    message: format!("{} auto-paused: {detail}", a.name),
                });
            } else if failed {
                a.status = "ERROR".into();
                a.status_detail = detail;
            } else if a.status == "ANALYZING" || a.status == "EXECUTING" {
                a.status = if a.running { "WAITING" } else { "IDLE" }.into();
                a.status_detail = None;
            }
            self.save_agent(&a)?;
            self.invalidate_cycle();
            for record in owned.iter().filter(|r| r["outcome"] == "SIGNAL") {
                if source == DataSource::Replay {
                    self.signal_raised = true;
                    self.set_playing(false);
                }
                self.emit(ServerEvent::Toast {
                    level: ToastLevel::Info,
                    message: format!(
                        "{} suggests {} {}",
                        a.name,
                        record["decision"]["action"].as_str().unwrap_or_default(),
                        record["symbol"].as_str().unwrap_or_default()
                    ),
                });
            }
            Ok(Some(first))
        })();
        self.agents.active.remove(&id);
        if let Some(reply) = reply {
            let _ = reply.send(result);
        } else if let Err(e) = result {
            self.stop_replay_with(e.message());
        }
        self.handle_engine_events();
    }

    // ---- several pairs ------------------------------------------------------------------------

    /// Starts a cycle over every pair of the agent. Pairs without a quote settle at once.
    fn begin_batch(
        &mut self,
        mut agent: rows::Agent,
        account: PaperAccount,
        timeframe: Timeframe,
        trigger: CycleTrigger,
        reply: Option<CycleReply>,
    ) -> Result<(), ApiError> {
        let now = self.market_now(account.source);
        agent.status = "ANALYZING".into();
        agent.status_detail = None;
        self.save_agent(&agent)?;
        let generation = *self.agents.generations.get(&agent.id).unwrap_or(&0);
        let secrets = self.secrets();
        let legs: Vec<Cycle> = agent
            .symbols
            .iter()
            .map(|symbol| {
                let mut leg = agent.clone();
                leg.symbol = symbol.clone();
                Cycle {
                    record: new_record(&leg, &account, now, trigger),
                    agent: leg,
                    source: account.source,
                    timeframe,
                    generation,
                    reply: None,
                    quote: None,
                    analyzed_position_id: None,
                    stale: false,
                    critical_error: None,
                    secrets: secrets.clone(),
                    target: None,
                    first_request: None,
                }
            })
            .collect();
        let n = legs.len();
        let mut b = Batch {
            legs,
            open: vec![true; n],
            reply,
            decisions: (0..n).map(|_| None).collect(),
            first_prompt: None,
        };
        let token = b.legs[0].record["id"].as_str().unwrap().to_string();
        self.agents.active.insert(agent.id.clone(), token);
        match self.execution(&account) {
            Ok(target) => {
                for leg in &mut b.legs {
                    leg.target = Some(target.clone());
                }
            }
            Err(e) => {
                for i in 0..n {
                    let detail = b.legs[i].clean(e.message());
                    b.settle(i, "DATA_ERROR", Some(detail));
                }
                self.finish_batch(b);
                return Ok(());
            }
        }
        for i in 0..n {
            match self.market_quote(b.legs[i].source, &b.legs[i].agent.symbol) {
                Some(quote) => {
                    b.legs[i].record["snapshot"] = json!(quote);
                    b.legs[i].quote = Some(quote);
                }
                None => b.settle(i, "DATA_ERROR", Some("No quote available yet".into())),
            }
        }
        if b.open_legs().next().is_none() {
            self.finish_batch(b);
            return Ok(());
        }
        let symbols: Vec<Option<String>> = (0..n)
            .map(|i| b.open[i].then(|| b.legs[i].agent.symbol.clone()))
            .collect();
        if account.source == DataSource::Replay {
            let candles = symbols
                .iter()
                .map(|s| {
                    s.as_ref()
                        .map(|s| Ok(self.replay_candles(s, timeframe, 200)))
                })
                .collect();
            self.batch_candles(b, candles);
            return Ok(());
        }
        let live = self.live.clone();
        self.agents.pending.push(Box::pin(async move {
            let mut candles = Vec::new();
            for symbol in symbols {
                candles.push(match symbol {
                    None => None,
                    Some(symbol) => Some(
                        std::panic::AssertUnwindSafe(live.candles(&symbol, timeframe, 200))
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| Err("Candle provider panicked".into())),
                    ),
                });
            }
            Completion::BatchCandles(b, candles)
        }));
        Ok(())
    }
    fn batch_candles(&mut self, mut b: Batch, candles: Vec<Option<Result<Vec<Candle>, String>>>) {
        let mut closed: Vec<Option<Vec<Candle>>> = Vec::new();
        for (i, result) in candles.into_iter().enumerate() {
            let Some(result) = result.filter(|_| b.open[i]) else {
                closed.push(None);
                continue;
            };
            match self.closed_candles(&mut b.legs[i], result) {
                Ok(Some(c)) => closed.push(Some(c)),
                Ok(None) => {
                    b.open[i] = false;
                    closed.push(None);
                }
                Err(e) => {
                    let detail = b.legs[i].clean(e.message());
                    b.settle(i, "DATA_ERROR", Some(detail));
                    closed.push(None);
                }
            }
        }
        if b.open_legs().next().is_none() {
            return self.finish_batch(b);
        }
        self.cancel_batch_entries(b, closed);
    }
    /// The agent's unfilled entries from its last cycle expire, on every pair at once. A pair
    /// whose entry cannot be cancelled sits this cycle out; the others carry on, so one order
    /// the venue keeps refusing to cancel does not hold back every pair.
    fn cancel_batch_entries(&mut self, mut b: Batch, closed: Vec<Option<Vec<Candle>>>) {
        let target = b.legs[0].target();
        let stale = match self.venue_open_orders(&target, &b.legs[0].agent.account_id) {
            Ok(orders) => orders
                .into_iter()
                .filter(|order| {
                    order.agent_id.as_deref() == Some(&b.legs[0].agent.id)
                        && order.order_type == OrderType::Limit
                        && !order.reduce_only
                })
                .map(|order| (order.id, order.symbol))
                .collect::<Vec<_>>(),
            Err(e) => {
                for i in b.open_legs().collect::<Vec<_>>() {
                    let detail = b.legs[i].clean(e.message());
                    b.settle(i, "DATA_ERROR", Some(detail));
                }
                return self.finish_batch(b);
            }
        };
        match target {
            ExecutionTarget::Paper => {
                for (id, symbol) in stale {
                    if let Err(e) = self.engine.cancel_order(&id) {
                        let e = self.api_error(e);
                        b.settle_pair(&symbol, e.message());
                    }
                }
                self.read_batch(b, closed);
            }
            ExecutionTarget::Exchange(_) if stale.is_empty() => self.read_batch(b, closed),
            ExecutionTarget::Exchange(_) => {
                let engine = self.engine_for(target);
                self.agents.pending.push(Box::pin(async move {
                    let mut failed: Vec<(String, ApiError)> = Vec::new();
                    for (id, symbol) in stale {
                        // A pair already refused keeps its first answer; its other entries
                        // are left for the next cycle.
                        if failed.iter().any(|(s, _)| *s == symbol) {
                            continue;
                        }
                        if let Err(e) = engine.cancel_order(id).await {
                            failed.push((symbol, e));
                        }
                    }
                    Completion::BatchCancelled(b, closed, failed)
                }));
            }
        }
    }
    /// Each open pair's account view and indicators, then news for every pair, then the model.
    fn read_batch(&mut self, mut b: Batch, closed: Vec<Option<Vec<Candle>>>) {
        let mut contexts: Vec<Option<MarketContext>> = Vec::new();
        for (i, closed) in closed.into_iter().enumerate() {
            let Some(closed) = closed.filter(|_| b.open[i]) else {
                contexts.push(None);
                continue;
            };
            match self.prepare_model(&mut b.legs[i], closed) {
                Ok(Some(context)) => contexts.push(Some(context)),
                Ok(None) => {
                    b.open[i] = false;
                    contexts.push(None);
                }
                Err(e) => {
                    let detail = b.legs[i].clean(e.message());
                    b.settle(i, "DATA_ERROR", Some(detail));
                    contexts.push(None);
                }
            }
        }
        if b.open_legs().next().is_none() {
            return self.finish_batch(b);
        }
        let first = &b.legs[0];
        if !(first.agent.use_news && first.source == DataSource::Live) {
            return self.begin_batch_model(b, contexts, Vec::new());
        }
        let news = self.news.clone();
        let sources = self.news_sources();
        let mut bases: Vec<String> = Vec::new();
        for i in b.open_legs() {
            let base = self.news_base_currency(&b.legs[i].agent.symbol);
            if !bases.contains(&base) {
                bases.push(base);
            }
        }
        self.agents.pending.push(Box::pin(async move {
            let mut items: Vec<NewsItem> = Vec::new();
            let mut error: Option<String> = None;
            match sources {
                Ok(sources) => {
                    for base in bases {
                        match std::panic::AssertUnwindSafe(news.for_symbol(
                            sources.clone(),
                            &base,
                            mochi_market::news::DEFAULT_NEWS_LIMIT,
                        ))
                        .catch_unwind()
                        .await
                        {
                            Ok(n) => {
                                for item in n.items {
                                    if !items.iter().any(|known| known.id == item.id) {
                                        items.push(item);
                                    }
                                }
                                error = error.or(n.error);
                            }
                            Err(_) => error = error.or(Some("Could not load news".into())),
                        }
                    }
                }
                Err(e) => error = Some(b.legs[0].clean(e.message())),
            }
            for i in b.open_legs().collect::<Vec<_>>() {
                b.legs[i].record["news"] = json!(items);
                b.legs[i].record["newsError"] = json!(error);
            }
            Completion::BatchNews(b, contexts, items)
        }));
    }
    /// One model call for every open pair.
    fn begin_batch_model(
        &mut self,
        mut b: Batch,
        contexts: Vec<Option<MarketContext>>,
        news: Vec<NewsItem>,
    ) {
        let open: Vec<usize> = b.open_legs().collect();
        let (provider, config) = match self.resolve_provider(&b.legs[open[0]].agent) {
            Ok(p) => p,
            Err(e) => {
                for &i in &open {
                    b.legs[i].record["latencyMs"] = json!(0);
                    b.legs[i].provider_error(e.clone());
                    b.open[i] = false;
                }
                return self.finish_batch(b);
            }
        };
        let secrets = self.secrets();
        for &i in &open {
            b.legs[i].secrets.extend(secrets.iter().cloned());
            b.legs[i].record["providerKind"] = json!(config.kind);
            b.legs[i].record["isMock"] = json!(false);
        }
        let agent = b.legs[open[0]].agent.clone();
        let message = self
            .agents
            .take_messages(&agent.id)
            .map(|m| b.legs[open[0]].clean(&m));
        if let Some(message) = &message {
            for &i in &open {
                b.legs[i].record["ownerMessage"] = json!(message);
            }
        }
        let trail = self.trails_instead(&agent);
        let prompt = BatchPrompt {
            system: with_trailing_exit(
                build_multi_system_prompt(agent.allow_trailing_stop || trail),
                trail,
            ),
            contexts,
            news,
            message,
            trailing: agent.allow_trailing_stop || trail,
        };
        let request = batch_request(&agent, &prompt, &open);
        b.first_prompt = Some(prompt);
        self.send_batch(b, provider, config, request);
    }
    fn send_batch(
        &mut self,
        mut b: Batch,
        provider: Arc<dyn AiProvider>,
        config: mochi_ai::ProviderConfig,
        request: AiRequest,
    ) {
        let open: Vec<usize> = b.open_legs().collect();
        self.agents.pending.push(Box::pin(async move {
            let (response, latency) = ask(provider, &request).await;
            for (n, &i) in open.iter().enumerate() {
                // One call answers every pair; its tokens and cost count once, on the first.
                add_usage(&mut b.legs[i].record, &config, &response, latency, n == 0);
            }
            Completion::BatchModel(b, response)
        }));
    }
    fn complete_batch_model(&mut self, mut b: Batch, response: Result<AiResponse, ProviderError>) {
        let open: Vec<usize> = b.open_legs().collect();
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                for &i in &open {
                    b.legs[i].provider_error(e.clone());
                    b.open[i] = false;
                }
                return self.finish_batch(b);
            }
        };
        let secrets = self.secrets();
        let symbols: Vec<String> = open
            .iter()
            .map(|&i| b.legs[i].agent.symbol.clone())
            .collect();
        for &i in &open {
            b.legs[i].secrets.extend(secrets.iter().cloned());
            // Parse the original only locally. Truncating first can expose the prefix of a key.
            let raw = truncate_utf16(&b.legs[i].clean(&response.text), 8000);
            b.legs[i].record["rawResponse"] = json!(raw);
        }
        match parse_decisions(&response.text, &symbols) {
            Ok(decisions) => {
                for (&i, decision) in open.iter().zip(decisions) {
                    b.decisions[i] = Some(decision);
                }
                self.advance_batch(b, 0);
            }
            Err(e) => {
                for &i in &open {
                    let detail = b.legs[i].clean(&e);
                    b.legs[i].record["validationError"] = json!(detail);
                    b.settle(i, "INVALID_OUTPUT", Some(detail));
                }
                self.finish_batch(b);
            }
        }
    }
    /// Decides the open legs from `from` on, in pair order. An exchange order leaves the actor
    /// and the batch carries on with the next leg when it answers.
    fn advance_batch(&mut self, mut b: Batch, from: usize) {
        for i in from..b.legs.len() {
            if !b.open[i] {
                continue;
            }
            let Some(decision) = b.decisions[i].take() else {
                continue;
            };
            b.open[i] = false;
            match self.decide(&mut b.legs[i], decision) {
                Ok(None) => {}
                Ok(Some((engine, act))) => {
                    self.agents.pending.push(Box::pin(async move {
                        let acted = act.run(engine).await;
                        Completion::BatchActed(b, i, Box::new(acted))
                    }));
                    return;
                }
                Err(e) => {
                    let detail = b.legs[i].clean(e.message());
                    settle(&mut b.legs[i].record, "DATA_ERROR", Some(detail));
                }
            }
        }
        self.finish_batch(b);
    }
    /// Like [`Self::finish_or_retry`], for the legs of a batch: one second call about only the
    /// refused legs, after the others have acted.
    fn finish_batch(&mut self, mut b: Batch) {
        for leg in &mut b.legs {
            if leg.critical_error.is_none() {
                restore_refusal(&mut leg.record);
            }
        }
        let retry: Vec<usize> = (0..b.legs.len())
            .filter(|&i| {
                refused(&b.legs[i].record) && self.cycle_owned(&b.legs[i]).unwrap_or(false)
            })
            .filter(|&i| {
                b.first_prompt
                    .as_ref()
                    .is_some_and(|p| p.contexts[i].is_some())
            })
            .collect();
        let mut prompt = match (retry.is_empty(), b.first_prompt.take()) {
            (false, Some(prompt)) => prompt,
            _ => return self.finish_legs(b.legs, b.reply),
        };
        let (provider, config) = match self.resolve_provider(&b.legs[retry[0]].agent) {
            Ok(p) => p,
            Err(_) => return self.finish_legs(b.legs, b.reply),
        };
        // As for one pair: cleaned with the secrets known now, then shortened. The shared reply
        // was the same text on every leg.
        let secrets = self.secrets();
        for &i in &retry {
            b.legs[i].secrets.extend(secrets.iter().cloned());
        }
        let first = &b.legs[retry[0]];
        let reply = truncate_utf16(
            &first.clean(first.record["rawResponse"].as_str().unwrap_or("")),
            REFUSED_REPLY_MAX,
        );
        prompt.message = prompt.message.map(|m| first.clean(&m));
        let mut why = Vec::new();
        for &i in &retry {
            let leg = &mut b.legs[i];
            why.push((
                leg.agent.symbol.clone(),
                leg.clean(leg.record["outcomeDetail"].as_str().unwrap_or("")),
            ));
            keep_attempt(&mut leg.record);
            b.open[i] = true;
        }
        let agent = b.legs[retry[0]].agent.clone();
        let mut request = batch_request(&agent, &prompt, &retry);
        request.user = with_refusal(request.user, &reply, &why);
        self.send_batch(b, provider, config, request);
    }
}
/// The shared call for the legs in `legs`, which all have market data in `prompt`.
fn batch_request(agent: &rows::Agent, prompt: &BatchPrompt, legs: &[usize]) -> AiRequest {
    let contexts: Vec<MarketContext> = legs
        .iter()
        .filter_map(|&i| prompt.contexts[i].clone())
        .collect();
    AiRequest {
        model: agent.model.clone(),
        system: prompt.system.clone(),
        user: with_owner_message(
            build_multi_user_prompt(
                &contexts,
                (!prompt.news.is_empty()).then_some(prompt.news.as_slice()),
                &agent.strategy,
                prompt.trailing,
            ),
            prompt.message.as_deref(),
        ),
        context: None,
    }
}

/// A new decision record: settled as DATA_ERROR until the cycle gets further.
fn new_record(
    agent: &rows::Agent,
    account: &PaperAccount,
    now: i64,
    trigger: CycleTrigger,
) -> Value {
    let token = format!("dec_{}", uuid::Uuid::new_v4());
    json!({"id":token,"agentId":agent.id,"agentName":agent.name,"accountId":agent.account_id,"source":account.source,"venue":account.venue,"ts":now,"trigger":trigger,"providerKind":null,"model":agent.model,"symbol":agent.symbol,"timeframe":agent.timeframe,"snapshot":null,"indicators":null,"decision":null,"rawResponse":null,"validationError":null,"risk":null,"outcome":"DATA_ERROR","outcomeDetail":null,"news":null,"newsError":null,"signalExpiresAt":null,"orderId":null,"fillIds":[],"latencyMs":null,"inputTokens":null,"outputTokens":null,"estimatedCost":null,"isMock":false})
}
fn truncate_utf16(text: &str, max: usize) -> String {
    let mut n = 0;
    text.chars()
        .take_while(|ch| {
            n += ch.len_utf16();
            n <= max
        })
        .collect()
}
fn is_stale(
    q: &Quote,
    last: i64,
    bar: i64,
    now: i64,
    max: f64,
) -> Result<Option<String>, ApiError> {
    let overflow = || ApiError::bad_request("Market timestamp difference overflow");
    let age = now.checked_sub(q.ts).ok_or_else(overflow)? as f64 / 1000.;
    if age > max {
        return Ok(Some(format!(
            "Quote is {}s old, limit {}s",
            mochi_domain::jsnum::js_to_fixed(age, 0),
            mochi_domain::jsnum::js_number_string(max)
        )));
    }
    if now
        .checked_sub(last.checked_add(bar).ok_or_else(overflow)?)
        .ok_or_else(overflow)?
        > 2 * bar
    {
        return Ok(Some("Candles lag the market by more than two bars".into()));
    }
    Ok(None)
}
fn estimate_cost(
    c: &mochi_ai::ProviderConfig,
    input: Option<f64>,
    output: Option<f64>,
) -> Option<String> {
    Some(mochi_domain::jsnum::js_to_fixed(
        (input? * c.input_price_per_m_tok? + output? * c.output_price_per_m_tok?) / 1_000_000.,
        6,
    ))
}
