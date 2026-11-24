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
    indicator_snapshot, redact_secrets, with_owner_message, with_trailing_exit, AiRequest,
    AiResponse, MarketContext, ProviderError,
};
use mochi_core::paper::PaperStore;
use mochi_domain::{
    AiAction, AiDecisionData, Candle, DataSource, NewsItem, OrderType, PaperAccount, Quote,
    RiskSettings, ServerEvent, Timeframe, ToastLevel,
};
use mochi_store::rows;
use serde_json::{json, Value};
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
    BatchCancelled(Batch, Vec<Option<Vec<Candle>>>, Result<(), ApiError>),
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
}
impl Batch {
    fn open_legs(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.legs.len()).filter(|&i| self.open[i])
    }
    fn settle(&mut self, i: usize, outcome: &str, detail: Option<String>) {
        settle(&mut self.legs[i].record, outcome, detail);
        self.open[i] = false;
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
                Ok(None) => self.finish_cycle(c),
                Ok(Some((engine, act))) => {
                    self.agents.pending.push(Box::pin(async move {
                        let acted = act.run(engine).await;
                        Completion::Acted(c, Box::new(acted))
                    }));
                }
                Err(e) => self.data_error(c, &e),
            },
            Completion::Acted(mut c, acted) => {
                settle_acted(&mut c.record, *acted);
                self.finish_cycle(c);
            }
            Completion::BatchCandles(b, candles) => self.batch_candles(b, candles),
            Completion::BatchCancelled(mut b, _, Err(e)) => {
                let detail = format!("Could not read the account: {}", e.message());
                for i in b.open_legs().collect::<Vec<_>>() {
                    let detail = b.legs[i].clean(&detail);
                    b.settle(i, "DATA_ERROR", Some(detail));
                }
                self.finish_batch(b)
            }
            Completion::BatchCancelled(b, closed, Ok(())) => self.read_batch(b, closed),
            Completion::BatchNews(b, contexts, news) => self.begin_batch_model(b, contexts, news),
            Completion::BatchModel(b, response) => self.complete_batch_model(b, response),
            Completion::BatchActed(mut b, i, acted) => {
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
            ExecutionTarget::Okx(_) if stale.is_empty() => self.read_account(c, closed),
            ExecutionTarget::Okx(_) => {
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
        self.agents.pending.push(Box::pin(async move {
            let request = AiRequest {
                model: c.agent.model.clone(),
                system: with_trailing_exit(build_system_prompt(trailing), trail),
                user: with_owner_message(build_user_prompt(&context, trailing), message.as_deref()),
                context: Some(context),
            };
            let started = Instant::now();
            let response =
                std::panic::AssertUnwindSafe(async { provider.complete(&request).await })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        Err(ProviderError::new(
                            mochi_ai::ProviderErrorCode::BadResponse,
                            "AI provider panicked",
                        ))
                    });
            c.record["latencyMs"] = json!(started.elapsed().as_millis() as u64);
            if let Ok(r) = &response {
                c.record["inputTokens"] = json!(r.input_tokens);
                c.record["outputTokens"] = json!(r.output_tokens);
                c.record["estimatedCost"] =
                    json!(estimate_cost(&config, r.input_tokens, r.output_tokens));
            }
            Completion::Model(c, response)
        }));
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
            || (c.record["trigger"] == "SCHEDULE" && !latest.running)
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
            let suggested: Vec<Value> = owned
                .iter()
                .filter(|r| r["outcome"] == "SIGNAL")
                .map(|r| r["symbol"].clone())
                .collect();
            if !suggested.is_empty() {
                self.retire_signals(
                    |s| s["agentId"] == id.as_str() && suggested.contains(&s["symbol"]),
                    "Replaced by a newer suggestion",
                )?;
            }
            for record in &owned {
                rows::insert_decision(self.connection(), record).map_err(|e| self.api_error(e))?;
                if let Some(log) = &self.operation_logs {
                    let outcome = record["outcome"].as_str().unwrap_or("UNKNOWN");
                    log.record(format_args!("agent cycle -> {outcome}"));
                }
            }
            let first = owned[0].clone();
            // A cycle can finish after a lifecycle action. Keep its decision in History,
            // but leave the newer schedule and failure state alone.
            if self
                .agents
                .generations
                .get(&id)
                .copied()
                .unwrap_or_default()
                != generation
            {
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
            if failed && (critical.is_some() || a.consecutive_errors >= 3) {
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
                }
            })
            .collect();
        let n = legs.len();
        let mut b = Batch {
            legs,
            open: vec![true; n],
            reply,
            decisions: (0..n).map(|_| None).collect(),
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
    /// The agent's unfilled entries from its last cycle expire, on every pair at once.
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
                .map(|order| order.id)
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
                for id in stale {
                    if let Err(e) = self.engine.cancel_order(&id) {
                        let e = self.api_error(e);
                        for i in b.open_legs().collect::<Vec<_>>() {
                            let detail = b.legs[i].clean(e.message());
                            b.settle(i, "DATA_ERROR", Some(detail));
                        }
                        return self.finish_batch(b);
                    }
                }
                self.read_batch(b, closed);
            }
            ExecutionTarget::Okx(_) if stale.is_empty() => self.read_batch(b, closed),
            ExecutionTarget::Okx(_) => {
                let engine = self.engine_for(target);
                self.agents.pending.push(Box::pin(async move {
                    let mut result = Ok(());
                    for id in stale {
                        result = engine.cancel_order(id).await;
                        if result.is_err() {
                            break;
                        }
                    }
                    Completion::BatchCancelled(b, closed, result)
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
        let contexts: Vec<MarketContext> = contexts.into_iter().flatten().collect();
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
        let trailing = agent.allow_trailing_stop || trail;
        self.agents.pending.push(Box::pin(async move {
            let request = AiRequest {
                model: agent.model.clone(),
                system: with_trailing_exit(build_multi_system_prompt(trailing), trail),
                user: with_owner_message(
                    build_multi_user_prompt(
                        &contexts,
                        (!news.is_empty()).then_some(news.as_slice()),
                        &agent.strategy,
                        trailing,
                    ),
                    message.as_deref(),
                ),
                context: None,
            };
            let started = Instant::now();
            let response =
                std::panic::AssertUnwindSafe(async { provider.complete(&request).await })
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        Err(ProviderError::new(
                            mochi_ai::ProviderErrorCode::BadResponse,
                            "AI provider panicked",
                        ))
                    });
            let latency = started.elapsed().as_millis() as u64;
            for (n, &i) in open.iter().enumerate() {
                b.legs[i].record["latencyMs"] = json!(latency);
                // One call answers every pair; its tokens and cost count once, on the first.
                if let (0, Ok(r)) = (n, &response) {
                    b.legs[i].record["inputTokens"] = json!(r.input_tokens);
                    b.legs[i].record["outputTokens"] = json!(r.output_tokens);
                    b.legs[i].record["estimatedCost"] =
                        json!(estimate_cost(&config, r.input_tokens, r.output_tokens));
                }
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
    fn finish_batch(&mut self, b: Batch) {
        self.finish_legs(b.legs, b.reply);
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
