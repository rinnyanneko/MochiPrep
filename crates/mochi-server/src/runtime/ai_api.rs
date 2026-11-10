//! API-owned agent configuration and demo ownership. Analysis stays in the runner.
use super::{lock, unix_ms, Runtime};
use crate::{error::ApiError, validate::AgentInput};
use mochi_core::paper::{CreateAccountArgs, PaperStore};
use mochi_domain::{default_risk_settings, DataSource, ServerEvent, Timeframe, Topic};
use mochi_store::{get_setting, rows, set_setting};
use serde_json::{value::to_raw_value, Value};

impl Runtime {
    pub fn assert_agent_refs(
        &self,
        provider: &str,
        account: &str,
        symbol: &str,
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
        self.assert_tradable(symbol, account.source)
    }
    pub fn create_agent(
        &mut self,
        input: AgentInput,
        is_demo: bool,
    ) -> Result<rows::Agent, ApiError> {
        self.assert_agent_refs(&input.provider_id, &input.account_id, &input.symbol)?;
        let agent = rows::Agent {
            id: format!("agent_{}", uuid::Uuid::new_v4()),
            name: input.name,
            provider_id: input.provider_id,
            model: input.model,
            symbol: input.symbol,
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
            is_demo,
            created_at: unix_ms(),
        };
        rows::insert_agent(self.connection(), &agent).map_err(|e| self.api_error(e))?;
        self.invalidate(&[Topic::Agents, Topic::Arena]);
        Ok(agent)
    }
    pub fn update_agent(&mut self, id: &str, input: AgentInput) -> Result<rows::Agent, ApiError> {
        self.assert_agent_refs(&input.provider_id, &input.account_id, &input.symbol)?;
        let mut agent = self.api_agent(id)?;
        self.assert_agent_editable(id)?;
        apply_input(&mut agent, input)?;
        rows::save_agent(self.connection(), &agent).map_err(|e| self.api_error(e))?;
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
    pub fn start_demo(&mut self) -> Result<(), ApiError> {
        if get_setting::<Value>(self.connection(), "demo.active").map_err(|e| self.api_error(e))?
            == Some(Value::Bool(true))
        {
            return Ok(());
        }
        {
            let mut replay = lock(&self.replay);
            replay.seek(0);
            set_setting(self.connection(), "replay.cursor", &replay.position())
                .map_err(|e| self.api_error(e))?;
            self.last_replay_equity_at = replay.now();
        }
        for (name, model, strategy, label) in [
            (
                "Momentum Mochi",
                "mock-momentum-v1",
                "Follow the trend: EMA20/EMA50 with MACD confirmation.",
                "Momentum",
            ),
            (
                "Reversion Daifuku",
                "mock-reversion-v1",
                "Fade RSI extremes and exit at the midline.",
                "Reversion",
            ),
            (
                "Reckless Rice Cake",
                "mock-reckless-v1",
                "Misbehaves on purpose to show the risk engine at work.",
                "Reckless",
            ),
        ] {
            let account = self
                .engine_mut()
                .create_account(CreateAccountArgs {
                    name: format!("{label} account"),
                    source: DataSource::Replay,
                    initial_balance: None,
                    is_demo: Some(true),
                    config: None,
                })
                .map_err(|e| self.api_error(e))?;
            self.create_agent(
                AgentInput {
                    name: name.into(),
                    provider_id: rows::MOCK_PROVIDER_ID.into(),
                    model: model.into(),
                    symbol: "BTCUSDT".into(),
                    timeframe: Timeframe::M15,
                    strategy: strategy.into(),
                    account_id: account.id,
                    risk: default_risk_settings(),
                    interval_sec: 900,
                    execution_mode: "AUTO".into(),
                    use_news: false,
                    allow_trailing_stop: false,
                },
                true,
            )?;
        }
        self.engine_mut()
            .create_account(CreateAccountArgs {
                name: "Demo terminal".into(),
                source: DataSource::Replay,
                initial_balance: None,
                is_demo: Some(true),
                config: None,
            })
            .map_err(|e| self.api_error(e))?;
        set_setting(self.connection(), "demo.active", &true).map_err(|e| self.api_error(e))?;
        self.invalidate_demo();
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
        Ok(())
    }
    pub fn clear_demo(&mut self) -> Result<(), ApiError> {
        self.engine()
            .store()
            .transaction(|| -> Result<(), mochi_core::paper::StoreFailure> {
                for agent in rows::list_agents(self.connection()).map_err(|e| {
                    mochi_core::paper::StoreFailure::new(
                        mochi_core::paper::StoreFailureKind::Other,
                        e.to_string(),
                    )
                })? {
                    if agent.is_demo {
                        rows::delete_agent(self.connection(), &agent.id).map_err(|e| {
                            mochi_core::paper::StoreFailure::new(
                                mochi_core::paper::StoreFailureKind::Other,
                                e.to_string(),
                            )
                        })?;
                    }
                }
                for account in self.engine().store().list_accounts()? {
                    if account.is_demo {
                        self.engine()
                            .store()
                            .delete_account_data(&account.id, false)?;
                    }
                }
                set_setting(self.connection(), "demo.active", &false).map_err(|e| {
                    mochi_core::paper::StoreFailure::new(
                        mochi_core::paper::StoreFailureKind::Other,
                        e.to_string(),
                    )
                })?;
                Ok(())
            })
            .map_err(|e| self.api_error(e))?;
        self.invalidate_demo();
        Ok(())
    }
    fn invalidate_demo(&mut self) {
        self.invalidate(&[
            Topic::Accounts,
            Topic::Agents,
            Topic::Arena,
            Topic::Decisions,
            Topic::Orders,
            Topic::Positions,
        ]);
    }
}
fn apply_input(agent: &mut rows::Agent, input: AgentInput) -> Result<(), ApiError> {
    agent.name = input.name;
    agent.provider_id = input.provider_id;
    agent.model = input.model;
    agent.symbol = input.symbol;
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
