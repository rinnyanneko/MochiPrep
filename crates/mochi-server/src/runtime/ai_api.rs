//! API-owned agent configuration, and the removal of old demo data. Analysis stays in the runner.
use super::{unix_ms, Runtime};
use crate::{error::ApiError, validate::AgentInput};
use mochi_core::paper::PaperStore;
use mochi_domain::Topic;
use mochi_store::{delete_setting, rows};
use serde_json::{value::to_raw_value, Value};

impl Runtime {
    pub fn assert_agent_refs(
        &self,
        provider: &str,
        account: &str,
        symbols: &[String],
    ) -> Result<(), ApiError> {
        if rows::get_provider(self.connection(), provider)
            .map_err(|e| self.api_error(e))?
            .is_none()
        {
            return Err(ApiError::bad_request("Unknown AI provider"));
        }
        let account = self
            .engine()
            .store()
            .get_account(account)
            .map_err(|e| self.api_error(e))?
            .ok_or_else(|| ApiError::bad_request("Unknown paper account"))?;
        for symbol in symbols {
            self.assert_tradable(symbol, account.source)?;
        }
        Ok(())
    }
    pub fn create_agent(&mut self, input: AgentInput) -> Result<rows::Agent, ApiError> {
        let symbols = input.pairs();
        self.assert_agent_refs(&input.provider_id, &input.account_id, &symbols)?;
        let agent = rows::Agent {
            id: format!("agent_{}", uuid::Uuid::new_v4()),
            name: input.name,
            provider_id: input.provider_id,
            model: input.model,
            symbol: symbols[0].clone(),
            symbols,
            timeframe: serde_json::to_value(input.timeframe)
                .expect("timeframe")
                .as_str()
                .expect("timeframe string")
                .into(),
            strategy: input.strategy,
            account_id: input.account_id,
            risk: to_raw_value(&input.risk).map_err(|e| self.api_error(e))?,
            interval_sec: input.interval_sec,
            execution_mode: input.execution_mode,
            use_news: input.use_news,
            allow_trailing_stop: input.allow_trailing_stop,
            status: "IDLE".into(),
            running: false,
            next_run_at: None,
            last_run_at: None,
            status_detail: None,
            consecutive_errors: 0,
            is_demo: false,
            created_at: unix_ms(),
        };
        rows::insert_agent(self.connection(), &agent).map_err(|e| self.api_error(e))?;
        self.invalidate(&[Topic::Agents, Topic::Arena]);
        Ok(agent)
    }
    pub fn update_agent(&mut self, id: &str, input: AgentInput) -> Result<rows::Agent, ApiError> {
        self.assert_agent_refs(&input.provider_id, &input.account_id, &input.pairs())?;
        let mut agent = self.api_agent(id)?;
        self.assert_agent_editable(id)?;
        if agent.running && agent.account_id != input.account_id {
            let source = self.require_account(&input.account_id)?.source;
            let previous = self
                .engine()
                .store()
                .get_account(&agent.account_id)
                .map_err(|e| self.api_error(e))?;
            if previous.map(|account| account.source) != Some(source) {
                agent.next_run_at = Some(self.market_now(source));
            }
        }
        let moved = agent.account_id != input.account_id;
        let before = agent.symbols.clone();
        apply_input(&mut agent, input)?;
        rows::save_agent(self.connection(), &agent).map_err(|e| self.api_error(e))?;
        // What the agent saw, and any OKX order still settling, belong to the old account. On
        // the new one it starts over: pairs seen there for the first time are only recorded.
        if moved {
            self.forget_positions(id);
        } else {
            let removed: Vec<String> = before
                .into_iter()
                .filter(|symbol| !agent.symbols.contains(symbol))
                .collect();
            self.forget_pairs(id, &removed);
        }
        self.invalidate(&[Topic::Agents, Topic::Arena]);
        Ok(agent)
    }
    pub fn api_agent(&self, id: &str) -> Result<rows::Agent, ApiError> {
        rows::get_agent(self.connection(), id)
            .map_err(|e| self.api_error(e))?
            .ok_or_else(|| ApiError::not_found("Unknown agent"))
    }
    fn assert_agent_editable(&self, id: &str) -> Result<(), ApiError> {
        if self.is_agent_busy(id) {
            return Err(ApiError::conflict(
                "A cycle is already running for this agent; try again after it settles",
            ));
        }
        Ok(())
    }
    pub fn delete_agent(&mut self, id: &str) -> Result<(), ApiError> {
        let agent = self.api_agent(id)?;
        if agent.running {
            return Err(ApiError::bad_request("Stop the agent first"));
        }
        self.assert_agent_editable(id)?;
        rows::delete_agent(self.connection(), id).map_err(|e| self.api_error(e))?;
        // Messages for a deleted agent must not wait for an agent that will never run.
        self.agents.messages.remove(id);
        self.invalidate(&[Topic::Agents, Topic::Decisions, Topic::Arena]);
        Ok(())
    }
    pub fn api_signal(&self, id: &str) -> Result<Value, ApiError> {
        let signal = rows::get_decision(self.connection(), id)
            .map_err(|e| self.api_error(e))?
            .ok_or_else(|| ApiError::not_found("Unknown suggestion"))?;
        if signal["outcome"] != "SIGNAL" {
            return Err(ApiError::conflict("This suggestion is no longer pending"));
        }
        Ok(signal)
    }
    /// Deletes what the removed demo mode created: its agents, its accounts with their orders and
    /// ledger, and its setting. Runs at start; a database that never used the demo is unchanged.
    pub(super) fn remove_legacy_demo(&mut self) -> Result<(), ApiError> {
        // Asked in SQL, so a database without demo data is never parsed row by row here.
        if !rows::has_legacy_demo(self.connection()).map_err(|e| self.api_error(e))? {
            return Ok(());
        }
        self.engine()
            .store()
            .transaction(|| -> Result<(), mochi_core::paper::StoreFailure> {
                let failure = |e: mochi_store::StoreError| {
                    mochi_core::paper::StoreFailure::new(
                        mochi_core::paper::StoreFailureKind::Other,
                        e.to_string(),
                    )
                };
                // Accounts the user's own agents trade on stay, demo or not: deleting one would
                // break that agent's row, and with it every start of the app.
                let mut in_use = Vec::new();
                for agent in rows::list_agents(self.connection()).map_err(failure)? {
                    if agent.is_demo {
                        rows::delete_agent(self.connection(), &agent.id).map_err(failure)?;
                    } else {
                        in_use.push(agent.account_id);
                    }
                }
                for account in self.engine().store().list_accounts()? {
                    if !account.is_demo {
                        continue;
                    }
                    if in_use.contains(&account.id) {
                        // Kept as the user's account now, so this cleanup is not asked again.
                        self.connection()
                            .execute("UPDATE accounts SET is_demo=0 WHERE id=?", [&account.id])
                            .map_err(|e| failure(e.into()))?;
                    } else {
                        self.engine()
                            .store()
                            .delete_account_data(&account.id, false)?;
                    }
                }
                delete_setting(self.connection(), "demo.active").map_err(|e| {
                    mochi_core::paper::StoreFailure::new(
                        mochi_core::paper::StoreFailureKind::Other,
                        e.to_string(),
                    )
                })?;
                Ok(())
            })
            .map_err(|e| self.api_error(e))?;
        Ok(())
    }
}
fn apply_input(agent: &mut rows::Agent, input: AgentInput) -> Result<(), ApiError> {
    let symbols = input.pairs();
    agent.name = input.name;
    agent.provider_id = input.provider_id;
    agent.model = input.model;
    agent.symbol = symbols[0].clone();
    agent.symbols = symbols;
    agent.timeframe = serde_json::to_value(input.timeframe)
        .expect("timeframe")
        .as_str()
        .expect("timeframe string")
        .into();
    agent.strategy = input.strategy;
    agent.account_id = input.account_id;
    agent.risk = to_raw_value(&input.risk).map_err(|e| ApiError::bad_request(e.to_string()))?;
    agent.interval_sec = input.interval_sec;
    agent.execution_mode = input.execution_mode;
    agent.use_news = input.use_news;
    agent.allow_trailing_stop = input.allow_trailing_stop;
    Ok(())
}
