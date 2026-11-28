//! Node's `ExecutionEngine` seam: routes and agents trade through one trait, whichever engine is
//! behind the account. The paper engine runs synchronously on the runtime task, one `call` per
//! operation. An OKX engine runs its network work off the task, in its account's own queue, and
//! reaches the database only through short `call`s back to the task, so it never holds the
//! runtime while it waits on OKX.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use mochi_core::paper::{DailyRisk, PaperStore, ProtectionResult};
use mochi_domain::{
    AccountBalance, AccountVenue, Environment, ExchangeId, Order, OrderRequest, OrderResult,
    PaperAccount, PositionView, Protection, Topic,
};
use mochi_exchange::engine::{
    EngineDeps, EngineError, ExchangeEngine, Rows, StoreAccess, StoreWork, UnsentOrder,
};
use mochi_exchange::venue::VenueAdapter;
#[cfg(feature = "test-seams")]
use mochi_okx::OkxAdapter;
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
    /// Keep the trailing stop in force and ignore the one in the change. The engine reads it in
    /// the same serialized step as the change, so a concurrent trailing edit is never undone.
    pub keep_trailing: bool,
    pub keep_take_profit: bool,
    pub checked: Option<mochi_exchange::engine::CheckedProtection>,
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
    fn reduce_position(
        &self,
        _request: OrderRequest,
        _position_id: String,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async { Err(ApiError::bad_request("An OKX position is required")) })
    }
    fn close_agent_position(
        &self,
        _request: OrderRequest,
        _position_id: String,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async { Err(ApiError::bad_request("An OKX position is required")) })
    }
    fn cancel_order(&self, order_id: String) -> BoxFuture<'_, Result<(), ApiError>>;
    /// Closes `fraction` (0 < fraction <= 1) of the position, reduce-only, even when disarmed.
    fn close_position(
        &self,
        account_id: String,
        symbol: String,
        fraction: f64,
        client_order_id: String,
        position_id: Option<String>,
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
    Exchange(Arc<ExchangeEngine>),
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
    pub engine: Arc<ExchangeEngine>,
    /// OKX Demo or live, from the account's venue; fixed for the session's life.
    pub venue: Venue,
    pub armed: Arc<AtomicBool>,
    /// The key's values (Node's `session.secrets`; for OKX the key, secret and passphrase), so
    /// every text that leaves the runtime has them removed. Wiped when the session goes.
    pub secrets: Vec<Zeroizing<String>>,
    /// Whether the last sync succeeded: only the change to unhealthy pauses agents.
    pub was_healthy: bool,
    /// What its engine's store writes are checked against.
    pub link: Arc<SessionLink>,
}

/// Ties an engine's store writes to its session. Until the session registers, they are allowed
/// while its account has not changed since the connect began (see `Runtime::session_seq`);
/// once registered, only while it is still the account's session. So a sync or order still out
/// on OKX when its session is replaced or removed never writes over its successor's rows.
pub(super) struct SessionLink {
    pub account_id: String,
    pub epoch: u64,
    pub registered: AtomicBool,
    refused: AtomicBool,
}

impl SessionLink {
    pub(super) fn new(account_id: &str, epoch: u64) -> Arc<Self> {
        Arc::new(SessionLink {
            account_id: account_id.to_string(),
            epoch,
            registered: AtomicBool::new(false),
            refused: AtomicBool::new(false),
        })
    }
}

impl ExchangeSession {
    fn retire(self) -> Vec<UnsentOrder> {
        let unsent = self.engine.retire();
        self.armed.store(false, Ordering::SeqCst);
        unsent
    }
}

/// The OKX venue an exchange account trades on; `None` for paper. An account on an exchange
/// this build has no support for is refused here, so nothing builds an OKX client for it.
pub(super) fn okx_venue(venue: &AccountVenue) -> Result<Option<Venue>, ApiError> {
    match venue {
        AccountVenue::Paper => Ok(None),
        AccountVenue::Exchange {
            exchange,
            environment,
        } if *exchange == ExchangeId::okx() => Ok(Some(match environment {
            Environment::Demo => Venue::OkxDemo,
            Environment::Live => Venue::OkxLive,
        })),
        AccountVenue::Exchange { exchange, .. } => Err(ApiError::bad_request(format!(
            "No exchange support installed for {}",
            exchange.as_str().to_ascii_uppercase()
        ))),
    }
}

/// Node's `new OkxExecutionEngine({...})` for one account, trading through `venue`. Arming is
/// read through `armed`.
pub(super) fn exchange_engine(
    runtime: WeakRuntimeHandle,
    account_id: &str,
    venue: Arc<dyn VenueAdapter>,
    options: &OkxOptions,
    live: Arc<dyn LiveFeed>,
    armed: Arc<AtomicBool>,
    link: Arc<SessionLink>,
) -> Arc<ExchangeEngine> {
    ExchangeEngine::new(EngineDeps {
        account_id: account_id.to_string(),
        venue,
        store: Arc::new(ActorStore { runtime, link }),
        get_quote: Arc::new(move |symbol: &str| live.get_quote(symbol)),
        now: options.now.clone(),
        is_armed: Arc::new(move || armed.load(Ordering::SeqCst)),
        new_id: Arc::new(|prefix: &str| format!("{prefix}_{}", uuid::Uuid::new_v4())),
    })
}

impl Runtime {
    /// Node's `execution(accountId)`. An exchange account without a session cannot trade.
    pub fn execution(&self, account: &PaperAccount) -> Result<ExecutionTarget, ApiError> {
        if okx_venue(&account.venue)?.is_none() {
            return Ok(ExecutionTarget::Paper);
        }
        match self.exchanges.get(&account.id) {
            Some(session) => Ok(ExecutionTarget::Exchange(session.engine.clone())),
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

    /// Keep the unsent orders until finalization succeeds, without keeping the old session alive.
    pub(super) fn retire_exchange_session(&mut self, account_id: &str) -> Result<(), EngineError> {
        if let Some(session) = self.exchanges.remove(account_id) {
            let unsent = session.retire();
            if !unsent.is_empty() {
                self.retired_unsent_orders
                    .entry(account_id.to_owned())
                    .or_default()
                    .extend(unsent);
            }
        }
        self.restore_retired_order(account_id)
    }

    /// An orderly shutdown loses every in-memory proof of non-dispatch, so the sessions
    /// are retired and those proofs written one last time. A proof that still cannot be
    /// saved is reported; its order stays lookup-only after the restart.
    pub(super) fn drain_unsent_orders_for_shutdown(&mut self) {
        let accounts: Vec<String> = self
            .exchanges
            .keys()
            .chain(self.retired_unsent_orders.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for account_id in accounts {
            if let Err(error) = self.retire_exchange_session(&account_id) {
                tracing::warn!(
                    account = %account_id,
                    error = %self.redact(&error.to_string()),
                    "could not record unsent OKX orders before shutdown"
                );
            }
        }
    }

    pub(super) fn restore_retired_order(&mut self, account_id: &str) -> Result<(), EngineError> {
        let Some(mut orders) = self.retired_unsent_orders.remove(account_id) else {
            return Ok(());
        };
        let count = orders.len();
        let mut failure = None;
        {
            let rows = Rows {
                store: self.engine().store(),
                instruments: self.instruments(),
            };
            let now = (self.okx.now)();
            orders.retain(|order| match order.finalize(&rows, account_id, now) {
                Ok(_) => false,
                Err(error) => {
                    failure.get_or_insert(error);
                    true
                }
            });
        }
        if orders.len() < count {
            self.invalidate(&[Topic::Orders]);
        }
        if !orders.is_empty() {
            self.retired_unsent_orders
                .insert(account_id.to_owned(), orders);
        }
        failure.map_or(Ok(()), Err)
    }

    /// Whether the session `link` belongs to may still write to the store (see [`SessionLink`]).
    fn session_current(&self, link: &Arc<SessionLink>) -> bool {
        if link.registered.load(Ordering::SeqCst) {
            self.exchanges
                .get(&link.account_id)
                .is_some_and(|session| Arc::ptr_eq(&session.link, link))
        } else {
            !self.session_changed_since(&link.account_id, link.epoch)
        }
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
        ExecutionTarget::Exchange(engine) => Arc::new(ExchangeExecution { engine, runtime }),
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

/// The store as the runtime task owns it. Each piece of work runs there, alone, and only while
/// the engine's session is current, checked in the same step as the write.
pub(super) struct ActorStore {
    pub runtime: WeakRuntimeHandle,
    pub link: Arc<SessionLink>,
}

impl StoreAccess for ActorStore {
    fn run(&self, work: StoreWork) -> mochi_okx::BoxFuture<'static, ()> {
        let runtime = self.runtime.upgrade();
        let link = self.link.clone();
        Box::pin(async move {
            if let Some(runtime) = runtime {
                // A runtime that stopped, or a session no longer current, drops the work unrun;
                // the engine reports it.
                let _ = runtime
                    .call(move |runtime| {
                        if !runtime.session_current(&link) {
                            link.refused.store(true, Ordering::SeqCst);
                            return;
                        }
                        work(&Rows {
                            store: runtime.engine().store(),
                            instruments: runtime.instruments(),
                        })
                    })
                    .await;
            }
        })
    }

    fn refusal(&self) -> Option<String> {
        self.link.refused.load(Ordering::SeqCst).then(|| {
            "This OKX session was replaced or disconnected, so its result was not recorded".into()
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
        position_id: Option<String>,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move {
            self.runtime
                .call(move |r| {
                    if let Some(id) = position_id {
                        let store = r.engine().store();
                        if let Some(order) = store
                            .get_order_by_client_id(&account_id, &client_order_id)
                            .map_err(|e| r.api_error(e))?
                        {
                            let fills = store
                                .list_fills_by_order(&order.id)
                                .map_err(|e| r.api_error(e))?;
                            return Ok(OrderResult {
                                order,
                                fills,
                                duplicate: true,
                            });
                        }
                        let positions = r
                            .engine()
                            .position_views(&account_id)
                            .map_err(|e| r.api_error(e))?;
                        if !positions
                            .iter()
                            .any(|p| p.position.id == id && p.position.symbol == symbol)
                        {
                            return Err(ApiError::conflict(
                                "The selected position has changed. Refresh and try again.",
                            ));
                        }
                    }
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
        mut protection: Protection,
        meta: ProtectionMeta,
    ) -> BoxFuture<'_, Result<ProtectionResult, ProtectionFailure>> {
        Box::pin(async move {
            // A paper change is one transaction: it is never refused part-way. A kept trailing
            // stop is read in the same runtime step.
            let result = self
                .runtime
                .call(move |r| {
                    if meta.keep_trailing || meta.keep_take_profit {
                        let current = r
                            .engine()
                            .protection_of(&account_id, &symbol)
                            .map_err(|e| r.api_error(e))?;
                        if meta.keep_trailing {
                            protection.trailing_stop = current.trailing_stop;
                        }
                        if meta.keep_take_profit {
                            protection.take_profit = current.take_profit;
                        }
                    }
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

struct ExchangeExecution {
    engine: Arc<ExchangeEngine>,
    runtime: WeakRuntimeHandle,
}

impl ExchangeExecution {
    /// A thrown engine error, as Node's handler answers it: 400, secrets removed on the task,
    /// the exchange keys included.
    async fn refused(&self, error: EngineError) -> ApiError {
        let not_dispatched = matches!(error, EngineError::RefusedBeforeDispatch(_));
        let text = error.to_string();
        match self.runtime.call(move |r| r.api_error(text)).await {
            Ok(error) if not_dispatched => ApiError::not_dispatched(error.message()),
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

impl ExecutionEngine for ExchangeExecution {
    fn place_order(&self, request: OrderRequest) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move { self.answer(self.engine.place_order(request).await).await })
    }

    fn reduce_position(
        &self,
        request: OrderRequest,
        position_id: String,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move {
            self.answer(self.engine.reduce_position(request, position_id).await)
                .await
        })
    }

    fn close_agent_position(
        &self,
        request: OrderRequest,
        position_id: String,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move {
            self.answer(self.engine.close_agent_position(request, position_id).await)
                .await
        })
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
        position_id: Option<String>,
    ) -> BoxFuture<'_, Result<OrderResult, ApiError>> {
        Box::pin(async move {
            let position_id = position_id
                .ok_or_else(|| ApiError::bad_request("Select an OKX position to close."))?;
            let result = self
                .engine
                .close_selected_position(
                    &account_id,
                    &symbol,
                    fraction,
                    &client_order_id,
                    Some(&position_id),
                )
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
                .set_protection_order(
                    &account_id,
                    &symbol,
                    protection,
                    mochi_exchange::engine::ProtectionOrderOptions {
                        retain: mochi_exchange::engine::ProtectionRetention {
                            trailing_stop: meta.keep_trailing,
                            take_profit: meta.keep_take_profit,
                        },
                        agent_id: meta.agent_id,
                        decision_id: meta.decision_id,
                        checked: meta.checked,
                    },
                )
                .await;
            match result {
                Ok(result) => Ok(result),
                Err(error) => {
                    let after = match &error {
                        EngineError::ProtectionChange { after, .. } => after.as_deref().cloned(),
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
    /// Keeps automatic exchange syncs from consuming the reads a test has staged.
    #[doc(hidden)]
    pub async fn suspend_periodic_okx_sync_for_test(&self) -> Result<(), ApiError> {
        self.call(|r| {
            r.last_exchange_sync_at =
                Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3600));
        })
        .await
    }

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
        let (venue, options, live, epoch) = self
            .call(move |r| {
                let account = r.require_account(&id)?;
                let venue = okx_venue(&account.venue)?
                    .ok_or_else(|| ApiError::bad_request("Not an exchange account"))?;
                r.restore_retired_order(&id)
                    .map_err(|error| r.api_error(error))?;
                Ok((venue, r.okx.clone(), r.live(), r.session_seq))
            })
            .await??;
        let secrets = vec![
            Zeroizing::new(credentials.api_key().to_string()),
            Zeroizing::new(credentials.secret_key().to_string()),
            Zeroizing::new(credentials.passphrase().to_string()),
        ];
        let adapter = Arc::new(OkxAdapter::new(options.client(credentials, venue)));
        let flag = Arc::new(AtomicBool::new(armed));
        let link = SessionLink::new(account_id, epoch);
        let engine = exchange_engine(
            self.downgrade(),
            account_id,
            adapter,
            &options,
            live,
            flag.clone(),
            link.clone(),
        );
        if let Err(error) = engine.sync_before_registration().await {
            let text = error.to_string();
            return Err(self.call(move |r| r.api_error(text)).await?);
        }
        let id = account_id.to_string();
        self.call(move |r| {
            let session = ExchangeSession {
                engine,
                venue,
                armed: flag,
                secrets,
                was_healthy: true,
                link: link.clone(),
            };
            r.bump_session(&id);
            r.retire_exchange_session(&id)
                .map_err(|error| r.api_error(error))?;
            link.registered.store(true, Ordering::SeqCst);
            r.exchanges.insert(id, session);
            Ok(())
        })
        .await?
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
        // The history pass too, awaited here so a test sees what it read.
        match engine.sync().await {
            Ok(()) => {
                let _ = engine.sync_position_history().await;
                Ok(())
            }
            Err(error) => {
                let text = error.to_string();
                Err(self.call(move |r| r.api_error(text)).await?)
            }
        }
    }
}
