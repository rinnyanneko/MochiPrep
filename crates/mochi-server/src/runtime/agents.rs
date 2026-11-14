//! Lifecycle and provider ownership. Slow work is owned by the actor, never by a self-handle.
use super::{Runtime, RuntimeHandle};
use crate::error::ApiError;
use futures_util::{future::BoxFuture, stream::FuturesUnordered};
use mochi_ai::{
    AiProvider, MockProvider, ProviderConfig, ProviderError, ProviderErrorCode, ProviderKind,
    RemoteProvider,
};
use mochi_domain::{DataSource, Topic, VaultStatus};
use mochi_store::rows;
use mochi_vault::{provider_context, seal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::oneshot;

pub type ProviderFactory =
    Arc<dyn Fn(&ProviderConfig, &str) -> Result<Arc<dyn AiProvider>, ProviderError> + Send + Sync>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CycleTrigger {
    Analyze,
    RunOnce,
    Schedule,
}
pub(super) type CycleReply = oneshot::Sender<Result<Option<Value>, ApiError>>;
#[derive(Default)]
pub(super) struct Agents {
    pub keys: HashMap<String, String>,
    pub factory: Option<ProviderFactory>,
    pub active: HashMap<String, String>,
    pub generations: HashMap<String, u64>,
    pub pending: FuturesUnordered<BoxFuture<'static, super::agent_cycle::Completion>>,
}
impl RuntimeHandle {
    pub async fn run_cycle(
        &self,
        id: impl Into<String>,
        trigger: CycleTrigger,
    ) -> Result<Option<Value>, ApiError> {
        let id = id.into();
        let (reply, done) = oneshot::channel();
        self.call(move |r| r.begin_cycle(&id, trigger, Some(reply)))
            .await??;
        done.await.map_err(|_| self.reply_error())?
    }
}
impl Runtime {
    pub fn require_agent(&self, id: &str) -> Result<rows::Agent, ApiError> {
        rows::get_agent(self.connection(), id)
            .map_err(|e| self.api_error(e))?
            .ok_or_else(|| ApiError::bad_request(format!("Unknown agent {id}")))
    }
    pub(super) fn save_agent(&mut self, agent: &rows::Agent) -> Result<(), ApiError> {
        rows::save_agent(self.connection(), agent).map_err(|e| self.api_error(e))?;
        self.invalidate(&[Topic::Agents]);
        Ok(())
    }
    pub fn is_agent_busy(&self, id: &str) -> bool {
        self.agents.active.contains_key(id)
    }
    pub fn start_agent(&mut self, id: &str) -> Result<(), ApiError> {
        let mut a = self.require_agent(id)?;
        self.assert_can_run(&a)?;
        let source = self.require_account(&a.account_id)?.source;
        a.running = true;
        a.status = "WAITING".into();
        a.next_run_at = Some(self.market_now(source));
        a.status_detail = None;
        a.consecutive_errors = 0;
        self.save_agent(&a)?;
        if self.is_agent_busy(id) {
            *self.agents.generations.entry(id.into()).or_default() += 1;
        }
        Ok(())
    }
    pub fn resume_agent(&mut self, id: &str) -> Result<(), ApiError> {
        self.start_agent(id)
    }
    fn halt_agent(
        &mut self,
        id: &str,
        status: &str,
        detail: Option<String>,
    ) -> Result<(), ApiError> {
        let mut a = self.require_agent(id)?;
        *self.agents.generations.entry(id.into()).or_default() += 1;
        a.running = false;
        a.status = status.into();
        a.next_run_at = None;
        a.status_detail = detail;
        self.save_agent(&a)
    }
    pub fn pause_agent(&mut self, id: &str, detail: Option<String>) -> Result<(), ApiError> {
        self.halt_agent(id, "PAUSED", detail)
    }
    pub fn stop_agent(&mut self, id: &str) -> Result<(), ApiError> {
        self.halt_agent(id, "IDLE", None)
    }
    pub fn stop_all_agents(&mut self) -> Result<usize, ApiError> {
        let agents = rows::list_agents(self.connection()).map_err(|e| self.api_error(e))?;
        let mut count = 0;
        for a in agents {
            if a.running || a.status != "IDLE" {
                self.halt_agent(&a.id, "IDLE", Some("Stopped by Stop All".into()))?;
                count += 1;
            }
        }
        self.retire_all_signals()?;
        Ok(count)
    }
    pub fn pause_account_agents(&mut self, account: &str, detail: &str) -> Result<(), ApiError> {
        for a in rows::list_agents(self.connection()).map_err(|e| self.api_error(e))? {
            if a.account_id == account && a.running {
                self.pause_agent(&a.id, Some(detail.into()))?;
            }
        }
        Ok(())
    }
    /// Node's `setKey`: sealed while unlocked, refused while locked, plaintext while OFF.
    pub fn set_key(&mut self, id: &str, key: &str) -> Result<(), ApiError> {
        let now = super::unix_ms();
        let saved = match (self.vault_status()?, self.data_key()) {
            (VaultStatus::Locked, _) => {
                return Err(ApiError::bad_request(
                    "Unlock MochiPrep with your master password first",
                ))
            }
            (VaultStatus::Unlocked, Some(data_key)) => {
                let sealed = seal(data_key, key, &provider_context(id));
                mochi_store::save_sealed_provider_key(self.connection(), id, &sealed, now)
            }
            _ => rows::save_provider_key(self.connection(), id, key, now),
        };
        saved.map_err(|e| self.api_error(e))?;
        self.agents.keys.insert(id.into(), key.into());
        self.invalidate(&[Topic::Providers]);
        Ok(())
    }
    pub fn delete_key(&mut self, id: &str) -> Result<(), ApiError> {
        rows::delete_provider_key(self.connection(), id).map_err(|e| self.api_error(e))?;
        self.agents.keys.remove(id);
        self.invalidate(&[Topic::Providers]);
        Ok(())
    }
    pub fn delete_provider(&mut self, id: &str) -> Result<(), ApiError> {
        // SQLite cascades both durable key formats within this DELETE.
        rows::delete_provider(self.connection(), id).map_err(|e| self.api_error(e))?;
        self.agents.keys.remove(id);
        self.invalidate(&[Topic::Providers]);
        Ok(())
    }
    /// Node's `providerViews()`: a provider has a key when its key is loaded. While unlocked
    /// that includes sealed keys, which the plaintext table no longer holds.
    pub fn provider_views(&self) -> Result<Vec<rows::ProviderView>, ApiError> {
        let status = self.vault_status()?;
        let mut views = rows::list_provider_views(self.connection(), status != VaultStatus::Off)
            .map_err(|e| self.api_error(e))?;
        if status == VaultStatus::Unlocked {
            for view in &mut views {
                view.has_key = !view.requires_key || self.agents.keys.contains_key(&view.id);
            }
        }
        Ok(views)
    }
    pub(super) fn resolve_provider(
        &self,
        agent: &rows::Agent,
    ) -> Result<(Arc<dyn AiProvider>, ProviderConfig), ProviderError> {
        let fail = |s: String| ProviderError::new(ProviderErrorCode::BadResponse, self.redact(&s));
        let c = rows::get_provider(self.connection(), &agent.provider_id)
            .map_err(|e| fail(e.to_string()))?
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorCode::NoKey,
                    "The AI provider for this agent was removed",
                )
            })?;
        let provider = self.provider_for(&c)?;
        Ok((provider, c))
    }
    pub fn provider_for(&self, c: &ProviderConfig) -> Result<Arc<dyn AiProvider>, ProviderError> {
        let fail = |s: String| ProviderError::new(ProviderErrorCode::BadResponse, self.redact(&s));
        let key = self
            .agents
            .keys
            .get(&c.id)
            .map(String::as_str)
            .unwrap_or("");
        if c.kind != ProviderKind::Mock && key.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorCode::NoKey,
                if self
                    .vault_status()
                    .map_err(|e| fail(e.message().to_string()))?
                    == VaultStatus::Locked
                {
                    format!(
                        "Unlock MochiPrep with your master password to load the key for \"{}\".",
                        c.name
                    )
                } else {
                    format!("No API key saved for \"{}\". Enter it in Settings.", c.name)
                },
            ));
        }
        let provider: Arc<dyn AiProvider> = if c.kind == ProviderKind::Mock {
            Arc::new(MockProvider)
        } else {
            match &self.agents.factory {
                Some(f) => f(c, key)?,
                None => Arc::new(RemoteProvider::new(c.clone(), key.into())?),
            }
        };
        Ok(provider)
    }
    pub(super) fn market_now(&self, source: DataSource) -> i64 {
        match source {
            DataSource::Replay => super::lock(&self.replay).now(),
            DataSource::Live => super::unix_ms(),
        }
    }
    pub(super) fn market_quote(
        &self,
        source: DataSource,
        symbol: &str,
    ) -> Option<mochi_domain::Quote> {
        match source {
            DataSource::Replay => super::lock(&self.replay).get_quote(symbol),
            DataSource::Live => self.live.get_quote(symbol),
        }
    }
    pub(super) fn tick_agents(&mut self, source: DataSource) -> Result<(), ApiError> {
        self.expire_signals(source)?;
        let now = self.market_now(source);
        for a in rows::list_agents(self.connection()).map_err(|e| self.api_error(e))? {
            if a.running
                && a.next_run_at.is_some_and(|at| at <= now)
                && self.require_account(&a.account_id)?.source == source
            {
                self.begin_cycle(&a.id, CycleTrigger::Schedule, None)?;
            }
        }
        Ok(())
    }
}
