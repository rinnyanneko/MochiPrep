//! The runtime: one task that owns the paper engine, the replay tape, the live feed handle, the
//! settings, agent futures, provider keys, the master password's data key and the instrument
//! registry, and the OKX sessions. Slow analysis, key derivation and OKX requests leave the actor
//! with owned snapshots and return to apply their result.
//!
//! Routes reach it through [`RuntimeHandle`]: every method sends a message to that one task and
//! awaits the answer. The engine is synchronous and not `Sync`, so this way it is only ever used
//! by one thread at a time, in the order requests arrive, with no lock for a route to forget.

mod agent_cycle;
mod agent_execution;
mod exchange;
mod exchange_sync;
mod execution;
pub use exchange::{ExchangeStatus, ARM_PHRASE};
pub use execution::{
    ExecutionEngine, ExecutionTarget, OkxOptions, ProtectionFailure, ProtectionMeta,
};
mod ai_api;
pub use agent_execution::effective_risk;
pub mod agents;
pub mod events;
pub mod live_feed;
pub mod news;
pub mod redact;
pub mod replay_clock;
mod scheduling;
pub use scheduling::DemoAction;
mod vault;
pub use vault::{UnlockOutcome, VAULT_RESET_PHRASE};

use std::fmt::Display;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use mochi_core::paper::{
    EngineError, EngineEvent, PaperEngineDeps, PaperExecutionEngine, PaperStore,
};
use mochi_domain::registry::Instruments;
use mochi_domain::{
    default_instruments, AccountBalance, AccountVenue, Candle, DataSource, FeedState, FeedStatus,
    InstrumentSpecification, PaperAccount, PositionSide, Quote, ReplayState, ServerEvent,
    Timeframe, ToastLevel, Topic, VaultStatus, DEFAULT_SYMBOLS,
};
use mochi_market::okx_public::OkxPublicFeed;
use mochi_market::replay::{ReplayMarketData, TapeTooShort};
use mochi_store::{
    delete_instrument, get_setting, has_vault, list_instruments, rows, save_instrument,
    set_setting, SqlitePaperStore, StoreError,
};
use mochi_vault::{KdfParams, DEFAULT_KDF};
use rusqlite::Connection;
use serde::{Serialize, Serializer};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{interval_at, sleep_until, Instant, MissedTickBehavior};

use crate::error::ApiError;
use events::EventHub;
pub use live_feed::{LiveFeed, OfflineFeed};
pub use news::{NewsService, OfflineNews};
use redact::redact_secrets;
pub use replay_clock::ReplayClock;
use replay_clock::DEFAULT_SPEED;

/// Equity history points on the replay, in tape time (Node's REPLAY_EQUITY_EVERY_MS).
const REPLAY_EQUITY_EVERY_MS: i64 = 15 * 60_000;
/// Equity history points on live data, in wall-clock time (LIVE_EQUITY_EVERY_MS).
const LIVE_EQUITY_EVERY: Duration = Duration::from_secs(60);
/// Node's `liveTimer` interval.
const LIVE_TICK: Duration = Duration::from_secs(1);
/// Messages waiting for the runtime task. A full queue makes callers wait, not fail.
const COMMAND_QUEUE: usize = 256;

/// The provider name Node's `ReplayMarketData` reports.
const REPLAY_PROVIDER: &str = "Synthetic replay";

pub type Engine = PaperExecutionEngine<SqlitePaperStore>;

/// A message to the runtime task: some work to run on the runtime.
type Command = Box<dyn FnOnce(&mut Runtime) + Send>;

pub struct RuntimeOptions {
    /// An open, migrated database (`mochi_store::open_database`).
    pub db: Connection,
    /// Tape length in minutes; `None` is the bundled 18 days. Tests use short tapes.
    pub replay_minutes: Option<usize>,
    pub live: Arc<dyn LiveFeed>,
    /// Quotes the live feed receives, in arrival order. `None` when it sends none.
    pub live_quotes: Option<mpsc::Receiver<Quote>>,
    /// The live feed's connection changes, each sent to the web app as a `feed` event.
    pub live_statuses: Option<mpsc::UnboundedReceiver<FeedStatus>>,
    /// Headlines for `/api/news`, fetched outside the runtime task.
    pub news: NewsService,
    pub provider_factory: Option<agents::ProviderFactory>,
    /// The scrypt cost for new password wraps. Tests use a cheap one; a stored wrap always
    /// carries its own cost, so this never affects unlocking.
    pub kdf: KdfParams,
    /// How OKX private trading is reached.
    pub okx: OkxOptions,
}

impl RuntimeOptions {
    /// The full tape and no network at all: no live connection, and news feeds that never
    /// connect.
    pub fn new(db: Connection) -> Self {
        RuntimeOptions {
            db,
            replay_minutes: None,
            live: Arc::new(OfflineFeed),
            live_quotes: None,
            live_statuses: None,
            news: NewsService::with_transport(Arc::new(OfflineNews), Arc::new(unix_ms)),
            provider_factory: None,
            kdf: DEFAULT_KDF,
            okx: OkxOptions::default(),
        }
    }

    /// The real OKX public feed, its quotes and status changes wired to the runtime, and the
    /// real news feeds.
    pub fn with_live_feed(db: Connection, feed: OkxPublicFeed) -> Self {
        let live_quotes = Some(feed.quotes());
        let live_statuses = Some(feed.statuses());
        RuntimeOptions {
            db,
            replay_minutes: None,
            live: Arc::new(feed),
            live_quotes,
            live_statuses,
            news: NewsService::new(),
            provider_factory: None,
            kdf: DEFAULT_KDF,
            okx: OkxOptions::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("Cannot read the database: {0}")]
    Store(#[from] StoreError),
    #[error(transparent)]
    Replay(#[from] TapeTooShort),
}

/// The `/api/state` body, keys in Node's order.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateBody {
    pub demo_active: bool,
    pub vault: VaultStatus,
    pub replay: ReplayState,
    pub feeds: Vec<FeedStatus>,
    /// Written as Node's INSTRUMENTS object: `{ "BTCUSDT": spec, ... }` in registry order.
    #[serde(serialize_with = "instruments_as_object")]
    pub instruments: Vec<InstrumentSpecification>,
}

/// Serialize instruments keyed by symbol, matching the Node state response.
fn instruments_as_object<S: Serializer>(
    specs: &[InstrumentSpecification],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_map(specs.iter().map(|spec| (&spec.symbol, spec)))
}

/// How routes talk to the runtime. Cheap to clone; every clone reaches the same task.
#[derive(Clone)]
pub struct RuntimeHandle {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<ServerEvent>,
}

impl RuntimeHandle {
    /// Builds the runtime and spawns its task. Must be called inside a tokio runtime.
    pub fn start(options: RuntimeOptions) -> Result<Self, StartError> {
        let live = LiveChannels {
            quotes: options.live_quotes,
            statuses: options.live_statuses,
        };
        let mut runtime = Runtime::open(
            options.db,
            options.replay_minutes,
            options.live,
            options.news,
            options.provider_factory,
            options.kdf,
            options.okx,
        )?;
        let events = runtime.events.sender();
        let (commands, inbox) = mpsc::channel(COMMAND_QUEUE);
        let handle = RuntimeHandle { commands, events };
        runtime.handle = Some(handle.downgrade());
        // After the added pairs are tracked, so the first subscribe covers them (Node's
        // `runtime.start()` after the constructor).
        runtime.live.start();
        tokio::spawn(run(runtime, inbox, live));
        Ok(handle)
    }

    /// Every event from now on. A receiver that falls behind skips the oldest events.
    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }

    /// How many event subscriptions are open: one per connected `/api/events` stream.
    pub fn subscriber_count(&self) -> usize {
        self.events.receiver_count()
    }

    /// Runs `work` on the runtime task and returns its result. Calls run one at a time, in the
    /// order they arrive. Engine events the work causes are handled before the answer is sent.
    pub async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Runtime) -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        let (reply, answer) = oneshot::channel();
        let command: Command = Box::new(move |runtime| {
            let value = work(runtime);
            runtime.handle_engine_events();
            // The caller may have gone away (a closed request); the work is done regardless.
            let _ = reply.send(value);
        });
        self.commands.send(command).await.map_err(|_| stopped())?;
        answer.await.map_err(|_| self.reply_error())
    }

    // A caught panic can drop a reply while the actor still accepts commands.
    fn reply_error(&self) -> ApiError {
        if self.commands.is_closed() {
            stopped()
        } else {
            internal_error()
        }
    }

    pub async fn invalidate(&self, topics: &[Topic]) -> Result<(), ApiError> {
        let topics = topics.to_vec();
        self.call(move |runtime| runtime.invalidate(&topics)).await
    }

    pub async fn replay_state(&self) -> Result<ReplayState, ApiError> {
        self.call(|runtime| runtime.replay_state()).await
    }

    pub async fn set_playing(&self, playing: bool) -> Result<(), ApiError> {
        self.call(move |runtime| runtime.set_playing(playing)).await
    }

    pub async fn set_speed(&self, speed: u32) -> Result<(), ApiError> {
        self.call(move |runtime| runtime.set_speed(speed)).await?
    }

    /// Returns once these steps have run. A timer batch in progress finishes first.
    pub async fn step_replay(&self, steps: u32) -> Result<(), ApiError> {
        let (reply, done) = oneshot::channel();
        self.call(move |runtime| runtime.queue_replay(steps, Some(reply)))
            .await?;
        done.await.map_err(|_| self.reply_error())?
    }

    pub async fn state(&self) -> Result<StateBody, ApiError> {
        self.call(|runtime| runtime.state()).await?
    }

    /// Pauses the replay and stops the task, which closes the database. Returns when done;
    /// later calls on any clone answer 503.
    pub async fn shutdown(&self) {
        let (done, finished) = oneshot::channel();
        let command: Command = Box::new(move |runtime| runtime.stopping = Some(done));
        if self.commands.send(command).await.is_ok() {
            let _ = finished.await;
        }
    }
}

/// A request whose work panicked. The panic itself is logged; the client learns nothing more.
fn internal_error() -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
}

fn stopped() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "The backend is shutting down",
    )
}

/// Everything the runtime task owns. Only reachable through [`RuntimeHandle::call`].
pub struct Runtime {
    engine: Engine,
    agents: agents::Agents,
    replay_batch: Option<scheduling::ReplayBatch>,
    replay_queue: std::collections::VecDeque<scheduling::ReplayRequest>,
    replay_idle: Vec<oneshot::Sender<()>>,
    replay_idle_work: Vec<Command>,
    signal_raised: bool,
    /// Shared with the engine's quote and clock callbacks, which cannot borrow the runtime.
    /// Only the runtime task locks it, so it is never contended.
    replay: Arc<Mutex<ReplayMarketData>>,
    live: Arc<dyn LiveFeed>,
    news: NewsService,
    /// Built-ins first, then added pairs in the order registered: the key order of Node's
    /// global INSTRUMENTS object, which `/api/state` sends. The engine keeps its own copy,
    /// updated alongside this one in `register_instrument` and `unregister_instrument`.
    instruments: Vec<InstrumentSpecification>,
    clock: ReplayClock,
    events: EventHub,
    /// What the engine reported during the current piece of work, handled right after it.
    engine_events: Arc<Mutex<Vec<EngineEvent>>>,
    last_replay_equity_at: i64,
    last_live_equity_at: Option<Instant>,
    /// Set by `shutdown`; the task stops after the current message.
    stopping: Option<oneshot::Sender<()>>,
    vault: vault::VaultState,
    /// Connected exchange accounts by account id. Memory only: a restart forgets them.
    exchanges: std::collections::HashMap<String, execution::ExchangeSession>,
    okx: OkxOptions,
    /// The task's own handle, weak so that work it owns never keeps it alive. Set by `start`.
    handle: Option<execution::WeakRuntimeHandle>,
    /// Work out on the network that ends with something for the task to run.
    tasks:
        futures_util::stream::FuturesUnordered<futures_util::future::BoxFuture<'static, Command>>,
    /// When the live tick last synced the exchange accounts; `None` syncs on the first tick.
    last_exchange_sync_at: Option<Instant>,
    /// A live tick waiting on its exchange sync: later ticks are skipped until it is done.
    live_ticking: bool,
}

/// The data behind a poisoned lock is still consistent here: nothing panics halfway through
/// changing the tape.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before 1970");
    i64::try_from(since_epoch.as_millis()).expect("timestamp fits in i64")
}

impl Runtime {
    /// Node's constructor order: the engine over the open database, every added pair
    /// registered and tracked, then the tape cursor and speed restored. The replay is paused.
    fn open(
        db: Connection,
        replay_minutes: Option<usize>,
        live: Arc<dyn LiveFeed>,
        news: NewsService,
        provider_factory: Option<agents::ProviderFactory>,
        kdf: KdfParams,
        okx: OkxOptions,
    ) -> Result<Self, StartError> {
        let replay = Arc::new(Mutex::new(ReplayMarketData::new(replay_minutes)?));
        let engine_events: Arc<Mutex<Vec<EngineEvent>>> = Arc::default();
        let engine = PaperExecutionEngine::new(PaperEngineDeps {
            store: SqlitePaperStore::new(db),
            instruments: Instruments::with_defaults(),
            get_quote: {
                let (replay, live) = (replay.clone(), live.clone());
                Box::new(move |source, symbol| match source {
                    DataSource::Replay => lock(&replay).get_quote(symbol),
                    DataSource::Live => live.get_quote(symbol),
                })
            },
            now: {
                let replay = replay.clone();
                // Replay time is the tape's (bar time + 60 s); live time is the wall clock.
                Box::new(move |source| match source {
                    DataSource::Replay => lock(&replay).now(),
                    DataSource::Live => unix_ms(),
                })
            },
            // `<prefix>_<uuid v4>`, as Node's engine draws with `crypto.randomUUID()`.
            new_id: Box::new(|prefix| format!("{prefix}_{}", uuid::Uuid::new_v4())),
            on_event: {
                let queue = engine_events.clone();
                Box::new(move |event| lock(&queue).push(event))
            },
        });

        let mut runtime = Runtime {
            engine,
            agents: agents::Agents {
                factory: provider_factory,
                ..Default::default()
            },
            replay_batch: None,
            replay_queue: Default::default(),
            replay_idle: Vec::new(),
            replay_idle_work: Vec::new(),
            signal_raised: false,
            replay,
            live,
            news,
            instruments: default_instruments(),
            clock: ReplayClock::new(DEFAULT_SPEED),
            events: EventHub::new(),
            engine_events,
            last_replay_equity_at: 0,
            last_live_equity_at: None,
            stopping: None,
            vault: vault::VaultState::new(kdf),
            exchanges: Default::default(),
            okx,
            handle: None,
            tasks: Default::default(),
            last_exchange_sync_at: None,
            live_ticking: false,
        };

        // The mock provider the demo needs, if this database does not have it yet.
        rows::seed_mock_provider(runtime.connection(), unix_ms())?;
        for spec in list_instruments(runtime.connection())? {
            runtime.live.track(&spec.symbol);
            runtime.register_instrument(spec);
        }

        if !has_vault(runtime.connection())? {
            runtime.agents.keys = rows::get_provider_keys(runtime.connection())?;
        }
        // All durable state is in SQLite; only the tape cursor needs seeking.
        if let Some(cursor) = get_setting::<i64>(runtime.connection(), "replay.cursor")? {
            lock(&runtime.replay).seek(cursor);
        }
        let speed = get_setting::<u32>(runtime.connection(), "replay.speed")?;
        runtime.clock.set_speed(speed.unwrap_or(DEFAULT_SPEED));
        runtime.last_replay_equity_at = lock(&runtime.replay).now();
        // Agents never survive the process: nothing trades after a restart until the owner
        // starts it. Pending futures are intentionally process-local.
        if rows::pause_agents_after_restart(runtime.connection())? > 0 {
            runtime.invalidate(&[Topic::Agents]);
        }
        Ok(runtime)
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// For routes that change paper state. Events the engine reports are handled when the
    /// `call` that used this returns.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    pub fn connection(&self) -> &Connection {
        self.engine.store().connection()
    }

    /// Every loaded AI key and saved gateway header value, including invalid provider rows, then
    /// each OKX session's key, secret and passphrase (Node's `secrets()` order).
    pub fn secrets(&self) -> Vec<String> {
        let mut secrets: Vec<_> = self.agents.keys.values().cloned().collect();
        // Invalid provider metadata must not hide otherwise valid saved gateway credentials.
        if let Ok(mut statement) = self
            .connection()
            .prepare("SELECT extra_headers FROM providers")
        {
            if let Ok(headers) = statement.query_map([], |r| r.get::<_, String>(0)) {
                for raw in headers.flatten() {
                    if let Ok(headers) =
                        serde_json::from_str::<std::collections::HashMap<String, String>>(&raw)
                    {
                        secrets.extend(headers.into_values());
                    }
                }
            }
        }
        for session in self.exchanges.values() {
            secrets.extend(session.secrets.iter().map(|s| s.to_string()));
        }
        secrets
    }

    /// Node's `runtime.redact`.
    pub fn redact(&self, text: &str) -> String {
        let secrets = self.secrets();
        redact_secrets(text, secrets.iter().map(String::as_str))
    }

    /// A runtime or engine failure as the 400 Node's error handler sends, with secrets removed.
    pub fn api_error(&self, error: impl Display) -> ApiError {
        ApiError::bad_request(self.redact(&error.to_string()))
    }

    pub fn emit(&self, event: ServerEvent) {
        self.events.emit(event);
    }

    pub fn invalidate(&mut self, topics: &[Topic]) {
        self.events.invalidate(topics);
    }

    /// Adds or replaces a pair in both registries (Node's `registerInstrument`).
    pub fn register_instrument(&mut self, spec: InstrumentSpecification) {
        match self
            .instruments
            .iter_mut()
            .find(|s| s.symbol == spec.symbol)
        {
            Some(existing) => *existing = spec.clone(),
            None => self.instruments.push(spec.clone()),
        }
        self.engine.instruments_mut().register(spec);
    }

    /// Removes an added pair from both registries. Built-in pairs always stay.
    pub fn unregister_instrument(&mut self, symbol: &str) {
        if !DEFAULT_SYMBOLS.contains(&symbol) {
            self.instruments.retain(|s| s.symbol != symbol);
        }
        self.engine.instruments_mut().unregister(symbol);
    }

    pub fn instruments(&self) -> &[InstrumentSpecification] {
        &self.instruments
    }

    /// Node's `INSTRUMENTS[symbol]`: a built-in or added pair.
    pub fn has_instrument(&self, symbol: &str) -> bool {
        self.instruments.iter().any(|spec| spec.symbol == symbol)
    }

    /// The live feed, for the routes that await it (candles, the OKX instrument list) outside
    /// this task.
    pub fn live(&self) -> Arc<dyn LiveFeed> {
        self.live.clone()
    }

    /// The second half of Node's `addInstrument`, once the spec has come fresh from OKX: saved,
    /// registered, tracked live.
    pub fn add_instrument(&mut self, spec: InstrumentSpecification) -> Result<(), ApiError> {
        save_instrument(self.connection(), &spec).map_err(|e| self.api_error(e))?;
        let symbol = spec.symbol.clone();
        self.register_instrument(spec);
        self.live.track(&symbol);
        self.invalidate(&[Topic::Instruments]);
        Ok(())
    }

    /// Node's `removeInstrument`, refusals in its order. Positions are Node's `positionsOf`: a
    /// connected exchange account's from its last sync, none for a disconnected one.
    pub fn remove_instrument(&mut self, symbol: &str) -> Result<(), ApiError> {
        if DEFAULT_SYMBOLS.contains(&symbol) {
            return Err(ApiError::bad_request(format!("{symbol} is built in")));
        }
        if !self.has_instrument(symbol) {
            return Err(ApiError::bad_request(format!(
                "{symbol} is not an added pair"
            )));
        }
        let agents = rows::list_agents(self.connection()).map_err(|e| self.api_error(e))?;
        if agents.iter().any(|agent| agent.symbol == symbol) {
            return Err(ApiError::bad_request("Agents still use this pair"));
        }
        let store = self.engine.store();
        for account in store.list_accounts().map_err(|e| self.api_error(e))? {
            let held = if account.venue == AccountVenue::Paper {
                store
                    .list_open_positions(&account.id)
                    .map_err(|e| self.api_error(e))?
                    .iter()
                    .any(|p| p.symbol == symbol)
            } else {
                self.exchange_session(&account.id).is_some_and(|session| {
                    session
                        .engine
                        .positions()
                        .iter()
                        .any(|p| p.position.symbol == symbol)
                })
            };
            if held {
                return Err(ApiError::bad_request(format!(
                    "{} has an open {symbol} position",
                    account.name
                )));
            }
            let orders = store
                .list_open_orders(&account.id)
                .map_err(|e| self.api_error(e))?;
            if orders.iter().any(|o| o.symbol == symbol) {
                return Err(ApiError::bad_request(format!(
                    "{} has open {symbol} orders",
                    account.name
                )));
            }
        }
        delete_instrument(self.connection(), symbol).map_err(|e| self.api_error(e))?;
        self.live.untrack(symbol);
        self.unregister_instrument(symbol);
        self.invalidate(&[Topic::Instruments]);
        Ok(())
    }

    // ---- accounts -------------------------------------------------------------------------

    /// The account, or Node's 404 `Unknown account`.
    pub fn require_account(&self, id: &str) -> Result<PaperAccount, ApiError> {
        self.engine
            .store()
            .get_account(id)
            .map_err(|e| self.api_error(e))?
            .ok_or_else(|| ApiError::not_found("Unknown account"))
    }

    /// Node's `balanceOf`: a read model that never fails for a missing exchange session. A
    /// connected exchange account shows its last synced balance; otherwise it shows its last
    /// equity point, or its initial balance before it has one, as Node does while disconnected.
    pub fn balance_of(&self, account: &PaperAccount) -> Result<AccountBalance, ApiError> {
        if account.venue == AccountVenue::Paper {
            return self
                .engine
                .balance_of(&account.id)
                .map_err(|e| self.api_error(e));
        }
        if let Some(balance) = self
            .exchange_session(&account.id)
            .and_then(|session| session.engine.cached_balance())
        {
            return Ok(balance);
        }
        let point = self
            .engine
            .store()
            .list_equity(&account.id, 1)
            .map_err(|e| self.api_error(e))?
            .pop();
        let equity = point
            .as_ref()
            .map_or_else(|| account.initial_balance.clone(), |p| p.equity.clone());
        Ok(AccountBalance {
            account_id: account.id.clone(),
            initial_balance: account.initial_balance.clone(),
            wallet_balance: point
                .as_ref()
                .map_or_else(|| equity.clone(), |p| p.wallet.clone()),
            available_balance: "0".to_string(),
            used_margin: "0".to_string(),
            order_margin: "0".to_string(),
            equity,
            realized_pnl: "0".to_string(),
            unrealized_pnl: point.map_or_else(|| "0".to_string(), |p| p.unrealized),
            fees_paid: "0".to_string(),
            funding_paid: "0".to_string(),
        })
    }

    /// `{ ...account, balance }`, in Node's key order.
    pub fn account_view(&self, id: &str) -> Result<serde_json::Value, ApiError> {
        let account = self.require_account(id)?;
        let balance = self.balance_of(&account)?;
        let mut view = serde_json::to_value(&account).map_err(|e| self.api_error(e))?;
        let balance = serde_json::to_value(&balance).map_err(|e| self.api_error(e))?;
        if let serde_json::Value::Object(fields) = &mut view {
            fields.insert("balance".to_string(), balance);
        }
        Ok(view)
    }

    // ---- market ---------------------------------------------------------------------------

    /// Custom pairs exist only on live data: the replay tape has just the built-in three.
    pub fn assert_tradable(&self, symbol: &str, source: DataSource) -> Result<(), ApiError> {
        if !self.instruments.iter().any(|spec| spec.symbol == symbol) {
            return Err(ApiError::bad_request(format!(
                "{symbol} is not an added pair. Add it in Settings first."
            )));
        }
        if source == DataSource::Replay && !DEFAULT_SYMBOLS.contains(&symbol) {
            return Err(ApiError::bad_request(format!(
                "{symbol} has no replay data. Use a Live OKX account."
            )));
        }
        Ok(())
    }

    /// The latest quote of every registered pair that has one, in registry order.
    pub fn quotes(&self, source: DataSource) -> Vec<Quote> {
        self.instruments
            .iter()
            .filter_map(|spec| match source {
                DataSource::Replay => lock(&self.replay).get_quote(&spec.symbol),
                DataSource::Live => self.live.get_quote(&spec.symbol),
            })
            .collect()
    }

    /// Replay candles, oldest first. Live candles come from the live feed, awaited by the route.
    pub fn replay_candles(&self, symbol: &str, timeframe: Timeframe, limit: usize) -> Vec<Candle> {
        lock(&self.replay).get_candles(symbol, timeframe, limit)
    }

    // ---- read models ----------------------------------------------------------------------

    /// Expose the replay cursor and controls using the feed's current bounds.
    pub fn replay_state(&self) -> ReplayState {
        let replay = lock(&self.replay);
        let start = replay.first_playable_time();
        let end = replay.end_time();
        let now = replay.now();
        ReplayState {
            loaded: true,
            playing: self.clock.playing(),
            speed: self.clock.speed(),
            now: Some(now),
            start_time: Some(start),
            end_time: Some(end),
            at_end: replay.at_end(),
            // Like Math.min(1, Math.max(0, x)), NaN included.
            progress: ((now - start) as f64 / (end - start) as f64).clamp(0.0, 1.0),
        }
    }

    /// REPLAY first, then LIVE, as Node's `feeds()`.
    pub fn feeds(&self) -> Vec<FeedStatus> {
        let replay = FeedStatus {
            source: DataSource::Replay,
            provider: REPLAY_PROVIDER.to_string(),
            state: FeedState::OfflineReplay,
            last_message_at: Some(lock(&self.replay).now()),
            detail: None,
        };
        vec![replay, self.live.status()]
    }

    /// Assemble state from the actor-owned database and feeds without inventing missing data.
    pub fn state(&self) -> Result<StateBody, ApiError> {
        let demo = get_setting::<serde_json::Value>(self.connection(), "demo.active")
            .map_err(|e| self.api_error(e))?;
        Ok(StateBody {
            // Node's `=== true`: any other stored value is not active.
            demo_active: demo == Some(serde_json::Value::Bool(true)),
            vault: self.vault_status()?,
            replay: self.replay_state(),
            feeds: self.feeds(),
            instruments: self.instruments.clone(),
        })
    }

    // ---- replay ---------------------------------------------------------------------------

    /// Emits the replay state only when `playing` changes, as Node does.
    pub fn set_playing(&mut self, playing: bool) {
        if self.clock.set_playing(playing) {
            self.emit(ServerEvent::Replay {
                state: self.replay_state(),
            });
        }
    }

    /// Update the replay clock, persist its speed, and publish the new control state.
    pub fn set_speed(&mut self, speed: u32) -> Result<(), ApiError> {
        self.clock.set_speed(speed);
        set_setting(self.connection(), "replay.speed", &speed).map_err(|e| self.api_error(e))?;
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
        Ok(())
    }

    /// Pauses the tape and says why: `Replay stopped: <message>`, secrets removed.
    fn stop_replay_with(&mut self, message: &str) {
        self.set_playing(false);
        let message = format!("Replay stopped: {}", self.redact(message));
        self.emit(ServerEvent::Toast {
            level: ToastLevel::Error,
            message,
        });
    }

    /// An equity point for every paper account on `source`.
    fn record_equity(&mut self, source: DataSource) -> Result<(), EngineError> {
        for account in self.engine.store().list_accounts()? {
            if account.source == source && account.venue == AccountVenue::Paper {
                self.engine.record_equity(&account.id)?;
            }
        }
        Ok(())
    }

    // ---- live -----------------------------------------------------------------------------

    /// Every live quote reaches the engine (resting orders, liquidation, funding), then the
    /// subscribers.
    fn on_live_quote(&mut self, quote: Quote) {
        let result = self.engine.on_quote(&quote);
        self.handle_engine_events();
        if let Err(error) = result {
            // Node lets this throw out of the WebSocket handler. Here the quote still goes out
            // for display, and the next quote tries again.
            tracing::warn!(error = %self.redact(&error.to_string()), "live quote failed in the paper engine");
        }
        self.events.emit_live_quote(quote);
    }

    /// The rest of the live tick, after any exchange sync: agent scheduling each second, and
    /// equity sampling on its own 60 s cadence.
    pub(super) fn after_live_sync(&mut self) {
        if let Err(error) = self.tick_agents(DataSource::Live) {
            self.emit(ServerEvent::Toast {
                level: ToastLevel::Error,
                message: self.redact(error.message()),
            });
        }
        let now = Instant::now();
        if self
            .last_live_equity_at
            .is_some_and(|at| now - at < LIVE_EQUITY_EVERY)
        {
            return;
        }
        self.last_live_equity_at = Some(now);
        match self.record_equity(DataSource::Live) {
            Ok(()) => self.invalidate(&[Topic::Arena]),
            Err(error) => {
                let message = self.redact(&error.to_string());
                self.emit(ServerEvent::Toast {
                    level: ToastLevel::Error,
                    message,
                });
            }
        }
    }

    /// Puts the runtime back in a usable state after work panicked: the engine's queued events
    /// belong to a rolled-back transaction, so they are discarded. An asynchronous replay
    /// batch survives unrelated command failures and must keep blocking timer credit.
    fn recover_from_panic(&mut self, panic: &(dyn std::any::Any + Send)) {
        self.engine.discard_queued_events();
        lock(&self.engine_events).clear();
        if self.replay_batch.is_none() {
            self.clock.end_batch();
        }
        let text = match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
            (Some(text), _) => text.to_string(),
            (_, Some(text)) => text.clone(),
            _ => "a panic without a message".to_string(),
        };
        tracing::error!(error = %self.redact(&text), "runtime work panicked; carrying on");
    }

    /// Starts a batch without ending it, so a test can panic in the middle of one.
    #[doc(hidden)]
    pub fn begin_batch_for_test(&mut self) {
        self.clock.begin_batch();
    }

    // ---- engine events --------------------------------------------------------------------

    /// Node's `onEngineEvent`: anything the engine changed makes the web app refetch, and a
    /// liquidation also gets a toast.
    fn handle_engine_events(&mut self) {
        let events = std::mem::take(&mut *lock(&self.engine_events));
        for event in events {
            self.invalidate(&[
                Topic::Accounts,
                Topic::Orders,
                Topic::Positions,
                Topic::Arena,
            ]);
            if let EngineEvent::Liquidation { position } = event {
                let _ = self.pause_account_agents(&position.account_id, "Paused after liquidation");
                let name = match self.engine.store().get_account(&position.account_id) {
                    Ok(Some(account)) => account.name,
                    _ => "Account".to_string(),
                };
                let side = match position.side {
                    PositionSide::Long => "LONG",
                    PositionSide::Short => "SHORT",
                };
                self.emit(ServerEvent::Toast {
                    level: ToastLevel::Error,
                    message: format!(
                        "{name}: {side} {} was liquidated (simulated)",
                        position.symbol
                    ),
                });
            }
        }
    }
}

/// What woke the runtime task.
enum Wake {
    Command(Command),
    /// Every handle is gone.
    HandlesDropped,
    ReplayTick,
    FlushInvalidate,
    LiveQuote(Quote),
    LiveFeedClosed,
    LiveStatus(FeedStatus),
    LiveStatusesClosed,
    LiveTick,
    Agent(Box<agent_cycle::Completion>),
    /// Network work done; what it returned runs on the task.
    Task(Command),
}

/// What the live feed sends the runtime task.
struct LiveChannels {
    quotes: Option<mpsc::Receiver<Quote>>,
    statuses: Option<mpsc::UnboundedReceiver<FeedStatus>>,
}

/// The runtime task: handles one thing at a time until shut down.
async fn run(mut runtime: Runtime, mut inbox: mpsc::Receiver<Command>, mut live: LiveChannels) {
    // The first tick is one second away, like setInterval's.
    let mut live_tick = interval_at(Instant::now() + LIVE_TICK, LIVE_TICK);
    live_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let flush_at = runtime.events.flush_at();
        let wake = tokio::select! {
            command = inbox.recv() => match command {
                Some(command) => Wake::Command(command),
                None => Wake::HandlesDropped,
            },
            () = runtime.clock.next_tick() => Wake::ReplayTick,
            () = sleep_until_some(flush_at) => Wake::FlushInvalidate,
            quote = next_quote(&mut live.quotes) => match quote {
                Some(quote) => Wake::LiveQuote(quote),
                None => Wake::LiveFeedClosed,
            },
            status = next_status(&mut live.statuses) => match status {
                Some(status) => Wake::LiveStatus(status),
                None => Wake::LiveStatusesClosed,
            },
            _ = live_tick.tick() => Wake::LiveTick,
            Some(completion) = futures_util::StreamExt::next(&mut runtime.agents.pending), if !runtime.agents.pending.is_empty() => Wake::Agent(Box::new(completion)),
            Some(work) = futures_util::StreamExt::next(&mut runtime.tasks), if !runtime.tasks.is_empty() => Wake::Task(work),
        };
        let cycle = match &wake {
            Wake::Agent(completion) => Some(completion.identity()),
            _ => None,
        };
        let work: Box<dyn FnOnce(&mut Runtime)> = match wake {
            Wake::HandlesDropped => break,
            Wake::LiveFeedClosed => {
                live.quotes = None;
                continue;
            }
            Wake::LiveStatusesClosed => {
                live.statuses = None;
                continue;
            }
            Wake::Command(command) => command,
            Wake::ReplayTick => Box::new(Runtime::on_replay_tick),
            Wake::FlushInvalidate => Box::new(|runtime| runtime.events.flush_invalidate()),
            Wake::LiveQuote(quote) => Box::new(move |runtime| runtime.on_live_quote(quote)),
            // Node's `live.onStatus((status) => this.emit({ type: 'feed', status }))`.
            Wake::LiveStatus(status) => {
                Box::new(move |runtime| runtime.emit(ServerEvent::Feed { status }))
            }
            Wake::LiveTick => Box::new(Runtime::live_tick),
            Wake::Agent(completion) => Box::new(move |r| r.complete_cycle(*completion)),
            Wake::Task(work) => work,
        };
        // A panic fails only this piece of work, as a thrown error fails one request in Node;
        // the task, and with it live paper trading, carries on. Unwind safety: the store rolls
        // its transaction back while unwinding, and `recover_from_panic` clears the rest.
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| work(&mut runtime))) {
            // The completion owns the reply and is dropped on unwind. Release only its
            // matching cycle so the replay barrier and later requests cannot be stranded.
            if let Some((id, token)) = cycle {
                if runtime.agents.active.get(&id) == Some(&token) {
                    runtime.agents.active.remove(&id);
                }
            }
            runtime.recover_from_panic(panic.as_ref());
        }
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| runtime.pump_replay())) {
            runtime.recover_from_panic(panic.as_ref());
        }
        if let Some(done) = runtime.stopping.take() {
            // Node's `stop()`: pause, stop the live feed, and drop pending timers. Dropping the
            // runtime closes the database before `shutdown` returns.
            runtime.set_playing(false);
            runtime.live.stop();
            // Mark shutdown before dropping pending reply senders.
            inbox.close();
            drop(runtime);
            let _ = done.send(());
            return;
        }
    }
    runtime.set_playing(false);
    runtime.live.stop();
}

/// Disable an optional select branch by waiting forever when no deadline exists.
async fn sleep_until_some(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Disable quote polling when no live receiver exists.
async fn next_quote(quotes: &mut Option<mpsc::Receiver<Quote>>) -> Option<Quote> {
    match quotes {
        Some(quotes) => quotes.recv().await,
        None => std::future::pending().await,
    }
}

/// Disable status polling when no live receiver exists.
async fn next_status(
    statuses: &mut Option<mpsc::UnboundedReceiver<FeedStatus>>,
) -> Option<FeedStatus> {
    match statuses {
        Some(statuses) => statuses.recv().await,
        None => std::future::pending().await,
    }
}
