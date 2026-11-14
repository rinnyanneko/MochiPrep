//! Node's `ExecutionEngine` seam: routes and agents trade through one trait, whichever engine is
//! behind the account. The paper engine runs synchronously on the runtime task, one `call` per
//! operation. An OKX engine runs its network work off the task, in its account's own queue, and
//! reaches the database only through short `call`s back to the task, so it never holds the
//! runtime while it waits on OKX.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use mochi_core::paper::{DailyRisk, ProtectionResult};
use mochi_domain::{
    AccountBalance, AccountVenue, Order, OrderRequest, OrderResult, PaperAccount, PositionView,
    Protection,
};
use mochi_okx::engine::{EngineError, OkxEngine, OkxEngineDeps, Rows, StoreAccess, StoreWork};
use mochi_okx::{Clock, Credentials, NetTransport, OkxClient, Transport, Venue};
use tokio::sync::{broadcast, mpsc};
use zeroize::Zeroizing;

use super::{stopped, unix_ms, Command, LiveFeed, Runtime, RuntimeHandle};
use crate::error::ApiError;

/// Who asked for a protection change, as Node's `setProtection` meta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionMeta {
    pub client_order_id: String,
    pub agent_id: Option<String>,
    pub decision_id: Option<String>,
}

/// A refused protection change. `after` is Node's `ProtectionChangeError.after`: set when the
/// change was refused part-way, and then it is what is in force, including legs already placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionFailure {
    pub error: ApiError,
    pub after: Option<Protection>,
}

impl ProtectionFailure {
    fn refused(error: ApiError) -> Self {
        ProtectionFailure { error, after: None }
    }
}

/// One account's trading, as routes and agents see it. Errors are already the HTTP answer: a
/// refusal is a 400 with its reason, secrets removed.
pub trait ExecutionEngine: Send + Sync {
    fn place_order(&self, request: OrderRequest) -> BoxFuture<'_, Result<OrderResult, ApiError>>;
    fn cancel_order(&self, order_id: String) -> BoxFuture<'_, Result<(), ApiError>>;
    /// Closes `fraction` (0 < fraction <= 1) of the position, reduce-only, even when disarmed.
    fn close_position(
        &self,
        account_id: String,
        symbol: String,
        fraction: f64,
        client_order_id: String,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>>;
    fn positions(&self, account_id: String) -> BoxFuture<'_, Result<Vec<PositionView>, ApiError>>;
    fn open_orders(&self, account_id: String) -> BoxFuture<'_, Result<Vec<Order>, ApiError>>;
    fn balance(&self, account_id: String) -> BoxFuture<'_, Result<AccountBalance, ApiError>>;
    /// Realized and unrealized change since 00:00 UTC, for the daily-loss limit.
    fn daily_risk(&self, account_id: String) -> BoxFuture<'_, Result<DailyRisk, ApiError>>;
    /// Replaces the whole protection of an open position. A `None` leg removes that leg. While
    /// the result is `pending`, an exchange has not confirmed a placement and the old legs are
    /// still in force.
    fn set_protection(
        &self,
        account_id: String,
        symbol: String,
        protection: Protection,
        meta: ProtectionMeta,
    ) -> BoxFuture<'_, Result<ProtectionResult, ProtectionFailure>>;
    /// The protection in force for a position, as the runner and risk engine see it.
    fn protection_of(
        &self,
        account_id: String,
        symbol: String,
    ) -> BoxFuture<'_, Result<Protection, ApiError>>;
}

/// Which engine trades an account, resolved on the runtime task.
#[derive(Clone)]
pub enum ExecutionTarget {
    Paper,
    Okx(Arc<OkxEngine>),
}

/// How OKX is reached. Tests inject an in-process fake and a fixed clock.
#[derive(Clone)]
pub struct OkxOptions {
    /// `None`: the real network, through `NetTransport`.
    pub transport: Option<Arc<dyn Transport>>,
    /// Node's `okx.baseUrl`: overrides the venue's host.
    pub base_url: Option<String>,
    /// Signs requests and timestamps engine decisions, in Unix milliseconds.
    pub now: Clock,
}

impl Default for OkxOptions {
    fn default() -> Self {
        OkxOptions {
            transport: None,
            base_url: None,
            now: Arc::new(unix_ms),
        }
    }
}

impl OkxOptions {
    fn transport(&self) -> Arc<dyn Transport> {
        self.transport
            .clone()
            .unwrap_or_else(|| Arc::new(NetTransport::new()))
    }

    /// A signed client for this key, on the venue's host unless a test overrides it.
    pub(super) fn client(&self, credentials: Credentials, venue: Venue) -> OkxClient {
        let mut client = OkxClient::new(credentials, venue, self.transport());
        if let Some(base) = &self.base_url {
            client = client.with_base_url(base.clone());
        }
        let now = self.now.clone();
        client.with_clock(move || now())
    }
}

/// A connected exchange account. Sessions live in memory only, so a restart forgets them.
pub(super) struct ExchangeSession {
    pub engine: Arc<OkxEngine>,
    pub armed: Arc<AtomicBool>,
    /// The key, secret and passphrase (Node's `session.secrets`), so every text that leaves the
    /// runtime has them removed. Wiped when the session goes.
    pub secrets: [Zeroizing<String>; 3],
    /// Whether the last sync succeeded: only the change to unhealthy pauses agents.
    pub was_healthy: bool,
}

impl ExchangeSession {
    /// The engine reads arming through this flag. Node's engine reads the account's current
    /// session, so an engine whose session was replaced or removed is never armed; clearing the
    /// flag keeps that for anything still holding the old engine.
    pub(super) fn retire(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

/// The venue an exchange account trades on.
pub(super) fn okx_venue(venue: AccountVenue) -> Option<Venue> {
    match venue {
        AccountVenue::OkxDemo => Some(Venue::OkxDemo),
        AccountVenue::OkxLive => Some(Venue::OkxLive),
        AccountVenue::Paper => None,
    }
}

/// Node's `new OkxExecutionEngine({...})` for one account. Arming is read through `armed`.
pub(super) fn okx_engine(
    runtime: WeakRuntimeHandle,
    account_id: &str,
    client: OkxClient,
    options: &OkxOptions,
    live: Arc<dyn LiveFeed>,
    armed: Arc<AtomicBool>,
) -> Arc<OkxEngine> {
    OkxEngine::new(OkxEngineDeps {
        account_id: account_id.to_string(),
        venue: client.venue(),
        client,
        store: Arc::new(ActorStore { runtime }),
        get_quote: Arc::new(move |symbol: &str| live.get_quote(symbol)),
        now: options.now.clone(),
        is_armed: Arc::new(move || armed.load(Ordering::SeqCst)),
        new_id: Arc::new(|prefix: &str| format!("{prefix}_{}", uuid::Uuid::new_v4())),
    })
}

impl Runtime {
    /// Node's `execution(accountId)`. An exchange account without a session cannot trade.
    pub fn execution(&self, account: &PaperAccount) -> Result<ExecutionTarget, ApiError> {
        if account.venue == AccountVenue::Paper {
            return Ok(ExecutionTarget::Paper);
        }
        match self.exchanges.get(&account.id) {
            Some(session) => Ok(ExecutionTarget::Okx(session.engine.clone())),
            None => Err(ApiError::bad_request(format!(
                "{} is not connected to OKX. Enter its API key in Settings.",
                account.name
            ))),
        }
    }

    /// `execution(accountId)` by id. A missing account is Node's plain `Unknown account` (400).
    pub fn execution_target(&self, account_id: &str) -> Result<ExecutionTarget, ApiError> {
        let account = self
            .require_account(account_id)
            .map_err(|e| ApiError::bad_request(e.message()))?;
        self.execution(&account)
    }

    pub(super) fn exchange_session(&self, account_id: &str) -> Option<&ExchangeSession> {
        self.exchanges.get(account_id)
    }

    /// The engine behind a target, for work that runs off the task (agent cycles). It holds no
    /// strong handle, so it never keeps a stopped runtime alive.
    pub(super) fn engine_for(&self, target: ExecutionTarget) -> Arc<dyn ExecutionEngine> {
        let runtime = self
            .handle
            .clone()
            .expect("the handle is set before the task starts");
        execution_engine(runtime, target)
    }
}

fn execution_engine(
    runtime: WeakRuntimeHandle,
    target: ExecutionTarget,
) -> Arc<dyn ExecutionEngine> {
    match target {
        ExecutionTarget::Paper => Arc::new(PaperExecution { runtime }),
        ExecutionTarget::Okx(engine) => Arc::new(OkxExecution { engine, runtime }),
    }
}

impl RuntimeHandle {
    pub fn engine_for(&self, target: ExecutionTarget) -> Arc<dyn ExecutionEngine> {
        execution_engine(self.downgrade(), target)
    }

    /// Resolves the engine for an account (404 `Unknown account` when it is gone).
    pub async fn execution(&self, account_id: &str) -> Result<Arc<dyn ExecutionEngine>, ApiError> {
        let id = account_id.to_string();
        let target = self
            .call(move |runtime| {
                let account = runtime.require_account(&id)?;
                runtime.execution(&account)
            })
            .await??;
        Ok(self.engine_for(target))
    }

    /// A handle that does not keep the runtime alive: what an OKX engine, itself owned by the
    /// runtime, holds to reach the store.
    pub(super) fn downgrade(&self) -> WeakRuntimeHandle {
        WeakRuntimeHandle {
            commands: self.commands.downgrade(),
            events: self.events.clone(),
        }
    }
}

/// See [`RuntimeHandle::downgrade`].
#[derive(Clone)]
pub(super) struct WeakRuntimeHandle {
    pub(super) commands: mpsc::WeakSender<Command>,
    pub(super) events: broadcast::Sender<mochi_domain::ServerEvent>,
}

impl WeakRuntimeHandle {
    pub(super) fn upgrade(&self) -> Option<RuntimeHandle> {
        Some(RuntimeHandle {
            commands: self.commands.upgrade()?,
            events: self.events.clone(),
        })
    }

    /// Runs `work` on the task, or answers 503 when the runtime has stopped.
    async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Runtime) -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        match self.upgrade() {
            Some(runtime) => runtime.call(work).await,
            None => Err(stopped()),
        }
    }
}

/// The store as the runtime task owns it. Each piece of work runs there, alone.
pub(super) struct ActorStore {
    pub runtime: WeakRuntimeHandle,
}

impl StoreAccess for ActorStore {
    fn run(&self, work: StoreWork) -> mochi_okx::BoxFuture<'static, ()> {
        let runtime = self.runtime.upgrade();
        Box::pin(async move {
            if let Some(runtime) = runtime {
                // A runtime that stopped drops the work unrun; the engine reports it.
                let _ = runtime
                    .call(move |runtime| {
                        work(&Rows {
                            store: runtime.engine().store(),
                            instruments: runtime.instruments(),
                        })
                    })
                    .await;
            }
        })
    }
}

struct PaperExecution {
    runtime: WeakRuntimeHandle,
}

impl ExecutionEngine for PaperExecution {
    fn place_order(&self, request: OrderRequest) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    r.engine_mut()
                        .place_order(request)
                        .map_err(|e| r.api_error(e))
                })
                .await?
        })
    }

    fn cancel_order(&self, order_id: String) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    r.engine_mut()
                        .cancel_order(&order_id)
                        .map_err(|e| r.api_error(e))
                })
                .await?
        })
    }

    fn close_position(
        &self,
        account_id: String,
        symbol: String,
        fraction: f64,
        client_order_id: String,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    r.engine_mut()
                        .close_position(&account_id, &symbol, fraction, &client_order_id)
                        .map_err(|e| r.api_error(e))
                })
                .await?
        })
    }

    fn positions(&self, account_id: String) -> BoxFuture<'_, Result<Vec<PositionView>, ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    r.engine()
                        .position_views(&account_id)
                        .map_err(|e| r.api_error(e))
                })
                .await?
        })
    }

    fn open_orders(&self, account_id: String) -> BoxFuture<'_, Result<Vec<Order>, ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    r.engine()
                        .open_orders(&account_id)
                        .map_err(|e| r.api_error(e))
                })
                .await?
        })
    }

    fn balance(&self, account_id: String) -> BoxFuture<'_, Result<AccountBalance, ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    r.engine()
                        .balance_of(&account_id)
                        .map_err(|e| r.api_error(e))
                })
                .await?
        })
    }

    fn daily_risk(&self, account_id: String) -> BoxFuture<'_, Result<DailyRisk, ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    r.engine()
                        .daily_risk(&account_id)
                        .map_err(|e| r.api_error(e))
                })
                .await?
        })
    }

    fn set_protection(
        &self,
        account_id: String,
        symbol: String,
        protection: Protection,
        _meta: ProtectionMeta,
    ) -> BoxFuture<'_, Result<ProtectionResult, ProtectionFailure>> {
        Box::pin(async move {
            // A paper change is one transaction: it is never refused part-way.
            let result = self
                .runtime
                .call(move |r| {
                    r.engine_mut()
                        .set_protection(&account_id, &symbol, &protection)
                        .map_err(|e| r.api_error(e))
                })
                .await
                .map_err(ProtectionFailure::refused)?;
            result.map_err(ProtectionFailure::refused)
        })
    }

    fn protection_of(
        &self,
        account_id: String,
        symbol: String,
    ) -> BoxFuture<'_, Result<Protection, ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    r.engine()
                        .protection_of(&account_id, &symbol)
                        .map_err(|e| r.api_error(e))
                })
                .await?
        })
    }
}

struct OkxExecution {
    engine: Arc<OkxEngine>,
    runtime: WeakRuntimeHandle,
}

impl OkxExecution {
    /// A thrown engine error, as Node's handler answers it: 400, secrets removed on the task,
    /// the exchange keys included.
    async fn refused(&self, error: EngineError) -> ApiError {
        let text = error.to_string();
        match self.runtime.call(move |r| r.api_error(text)).await {
            Ok(error) | Err(error) => error,
        }
    }

    async fn answer<T>(&self, result: Result<T, EngineError>) -> Result<T, ApiError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => Err(self.refused(error).await),
        }
    }
}

impl ExecutionEngine for OkxExecution {
    fn place_order(&self, request: OrderRequest) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move { self.answer(self.engine.place_order(request).await).await })
    }

    fn cancel_order(&self, order_id: String) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(async move { self.answer(self.engine.cancel_order(&order_id).await).await })
    }

    fn close_position(
        &self,
        account_id: String,
        symbol: String,
        fraction: f64,
        client_order_id: String,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move {
            let result = self
                .engine
                .close_position(&account_id, &symbol, fraction, &client_order_id)
                .await;
            self.answer(result).await
        })
    }

    fn positions(&self, _account_id: String) -> BoxFuture<'_, Result<Vec<PositionView>, ApiError>> {
        Box::pin(async move { Ok(self.engine.positions()) })
    }

    fn open_orders(&self, account_id: String) -> BoxFuture<'_, Result<Vec<Order>, ApiError>> {
        Box::pin(async move {
            self.answer(self.engine.open_orders(&account_id).await)
                .await
        })
    }

    fn balance(&self, _account_id: String) -> BoxFuture<'_, Result<AccountBalance, ApiError>> {
        Box::pin(async move { self.answer(self.engine.balance()).await })
    }

    fn daily_risk(&self, account_id: String) -> BoxFuture<'_, Result<DailyRisk, ApiError>> {
        Box::pin(async move { self.answer(self.engine.daily_risk(&account_id).await).await })
    }

    fn set_protection(
        &self,
        account_id: String,
        symbol: String,
        protection: Protection,
        meta: ProtectionMeta,
    ) -> BoxFuture<'_, Result<ProtectionResult, ProtectionFailure>> {
        Box::pin(async move {
            let result = self
                .engine
                .set_protection(
                    &account_id,
                    &symbol,
                    protection,
                    meta.agent_id,
                    meta.decision_id,
                )
                .await;
            match result {
                Ok(result) => Ok(result),
                Err(error) => {
                    let after = match &error {
                        EngineError::ProtectionChange { after, .. } => Some((**after).clone()),
                        _ => None,
                    };
                    Err(ProtectionFailure {
                        error: self.refused(error).await,
                        after,
                    })
                }
            }
        })
    }

    fn protection_of(
        &self,
        _account_id: String,
        symbol: String,
    ) -> BoxFuture<'_, Result<Protection, ApiError>> {
        Box::pin(async move { Ok(self.engine.protection_of(&symbol)) })
    }
}

/// Seams for this crate's own tests, behind the `test-seams` feature so a production build has
/// no way to a session that skipped the connect checks.
#[cfg(feature = "test-seams")]
impl RuntimeHandle {
    /// Builds an OKX session for an existing exchange account and runs its first full sync, with
    /// none of the connect checks.
    #[doc(hidden)]
    pub async fn attach_okx_for_test(
        &self,
        account_id: &str,
        credentials: Credentials,
        armed: bool,
    ) -> Result<(), ApiError> {
        let id = account_id.to_string();
        let (venue, options, live) = self
            .call(move |r| {
                let account = r.require_account(&id)?;
                let venue = okx_venue(account.venue)
                    .ok_or_else(|| ApiError::bad_request("Not an exchange account"))?;
                Ok((venue, r.okx.clone(), r.live()))
            })
            .await??;
        let secrets = [
            Zeroizing::new(credentials.api_key().to_string()),
            Zeroizing::new(credentials.secret_key().to_string()),
            Zeroizing::new(credentials.passphrase().to_string()),
        ];
        let client = options.client(credentials, venue);
        let flag = Arc::new(AtomicBool::new(armed));
        let engine = okx_engine(
            self.downgrade(),
            account_id,
            client,
            &options,
            live,
            flag.clone(),
        );
        if let Err(error) = engine.sync().await {
            let text = error.to_string();
            return Err(self.call(move |r| r.api_error(text)).await?);
        }
        let id = account_id.to_string();
        self.call(move |r| {
            let session = ExchangeSession {
                engine,
                armed: flag,
                secrets,
                was_healthy: true,
            };
            if let Some(old) = r.exchanges.insert(id, session) {
                old.retire();
            }
        })
        .await
    }

    /// Arms or disarms a session without the phrase.
    #[doc(hidden)]
    pub async fn set_armed_for_test(&self, account_id: &str, armed: bool) -> Result<(), ApiError> {
        let id = account_id.to_string();
        self.call(move |r| {
            if let Some(session) = r.exchanges.get(&id) {
                session.armed.store(armed, Ordering::SeqCst);
            }
        })
        .await
    }

    /// Runs one full OKX sync for a connected account, outside the sync loop.
    #[doc(hidden)]
    pub async fn sync_okx_for_test(&self, account_id: &str) -> Result<(), ApiError> {
        let id = account_id.to_string();
        let engine = self
            .call(move |r| r.exchange_session(&id).map(|s| s.engine.clone()))
            .await?
            .ok_or_else(|| ApiError::bad_request("Connect this account first"))?;
        match engine.sync().await {
            Ok(()) => Ok(()),
            Err(error) => {
                let text = error.to_string();
                Err(self.call(move |r| r.api_error(text)).await?)
            }
        }
    }
}
