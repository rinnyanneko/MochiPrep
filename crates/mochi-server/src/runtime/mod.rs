//! The runtime: one task that owns the paper engine, the replay tape, the live feed handle, the
//! settings and the instrument registry. A port of `apps/server/src/runtime.ts` as far as the
//! paper and market routes need it; agents, AI keys, OKX trading, the vault and demo mode come
//! with later plans.
//!
//! Routes reach it through [`RuntimeHandle`]: every method sends a message to that one task and
//! awaits the answer. The engine is synchronous and not `Sync`, so this way it is only ever used
//! by one thread at a time, in the order requests arrive, with no lock for a route to forget.

pub mod events;
pub mod live_feed;
pub mod redact;
pub mod replay_clock;

use std::fmt::Display;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use mochi_core::paper::{
    EngineError, EngineEvent, PaperEngineDeps, PaperExecutionEngine, PaperStore,
};
use mochi_domain::registry::Instruments;
use mochi_domain::{
    default_instruments, AccountVenue, DataSource, FeedState, FeedStatus, InstrumentSpecification,
    PositionSide, Quote, ReplayState, ServerEvent, ToastLevel, Topic, VaultStatus, DEFAULT_SYMBOLS,
};
use mochi_market::replay::{ReplayMarketData, TapeTooShort};
use mochi_store::{
    get_setting, has_vault, list_instruments, set_setting, SqlitePaperStore, StoreError,
};
use rusqlite::Connection;
use serde::{Serialize, Serializer};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{interval_at, sleep_until, Instant, MissedTickBehavior};

use crate::error::ApiError;
use events::EventHub;
pub use live_feed::{LiveFeed, OfflineFeed};
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
}

impl RuntimeOptions {
    /// The full tape and no live connection.
    pub fn new(db: Connection) -> Self {
        RuntimeOptions {
            db,
            replay_minutes: None,
            live: Arc::new(OfflineFeed),
            live_quotes: None,
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
        let live_quotes = options.live_quotes;
        let runtime = Runtime::open(options.db, options.replay_minutes, options.live)?;
        let events = runtime.events.sender();
        let (commands, inbox) = mpsc::channel(COMMAND_QUEUE);
        tokio::spawn(run(runtime, inbox, live_quotes));
        Ok(RuntimeHandle { commands, events })
    }

    /// Every event from now on. A receiver that falls behind skips the oldest events.
    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
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
        answer.await.map_err(|_| stopped())
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
        self.call(move |runtime| runtime.step_replay(steps)).await?
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

fn stopped() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "The backend is shutting down",
    )
}

/// Everything the runtime task owns. Only reachable through [`RuntimeHandle::call`].
pub struct Runtime {
    engine: Engine,
    /// Shared with the engine's quote and clock callbacks, which cannot borrow the runtime.
    /// Only the runtime task locks it, so it is never contended.
    replay: Arc<Mutex<ReplayMarketData>>,
    live: Arc<dyn LiveFeed>,
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
            replay,
            live,
            instruments: default_instruments(),
            clock: ReplayClock::new(DEFAULT_SPEED),
            events: EventHub::new(),
            engine_events,
            last_replay_equity_at: 0,
            last_live_equity_at: None,
            stopping: None,
        };

        for spec in list_instruments(runtime.connection())? {
            runtime.live.track(&spec.symbol);
            runtime.register_instrument(spec);
        }

        // All durable state is in SQLite; only the tape cursor needs seeking.
        if let Some(cursor) = get_setting::<i64>(runtime.connection(), "replay.cursor")? {
            lock(&runtime.replay).seek(cursor);
        }
        let speed = get_setting::<u32>(runtime.connection(), "replay.speed")?;
        runtime.clock.set_speed(speed.unwrap_or(DEFAULT_SPEED));
        runtime.last_replay_equity_at = lock(&runtime.replay).now();
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

    /// Secrets that must never reach a client. None are held in memory yet: AI keys and OKX
    /// credentials arrive with later plans, and add themselves here.
    pub fn secrets(&self) -> Vec<String> {
        Vec::new()
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

    // ---- read models ----------------------------------------------------------------------

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

    pub fn state(&self) -> Result<StateBody, ApiError> {
        let conn = self.connection();
        let demo =
            get_setting::<serde_json::Value>(conn, "demo.active").map_err(|e| self.api_error(e))?;
        // Nothing in this backend unlocks a vault yet, so one that exists is LOCKED.
        let vault = if has_vault(conn).map_err(|e| self.api_error(e))? {
            VaultStatus::Locked
        } else {
            VaultStatus::Off
        };
        Ok(StateBody {
            // Node's `=== true`: any other stored value is not active.
            demo_active: demo == Some(serde_json::Value::Bool(true)),
            vault,
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

    pub fn set_speed(&mut self, speed: u32) -> Result<(), ApiError> {
        self.clock.set_speed(speed);
        set_setting(self.connection(), "replay.speed", &speed).map_err(|e| self.api_error(e))?;
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
        Ok(())
    }

    /// Runs a batch of `steps` bars. While it runs the timer earns no credit. Batches never
    /// overlap: they all run on this one task.
    pub fn step_replay(&mut self, steps: u32) -> Result<(), ApiError> {
        self.clock.begin_batch();
        let result = self.do_steps(steps);
        self.clock.end_batch();
        result
    }

    fn on_replay_tick(&mut self) {
        if let Some(steps) = self.clock.on_tick() {
            if let Err(error) = self.step_replay(steps) {
                tracing::error!(message = %error.message(), "replay batch failed");
            }
        }
    }

    /// Node's `doSteps`. An engine failure pauses the tape with a toast; the cursor is saved
    /// and the batch's events go out either way.
    fn do_steps(&mut self, steps: u32) -> Result<(), ApiError> {
        let mut quotes = Vec::new();
        if let Err(error) = self.advance(steps, &mut quotes) {
            self.set_playing(false);
            let message = format!("Replay stopped: {}", self.redact(&error.to_string()));
            self.emit(ServerEvent::Toast {
                level: ToastLevel::Error,
                message,
            });
        }
        let position = lock(&self.replay).position();
        set_setting(self.connection(), "replay.cursor", &position)
            .map_err(|e| self.api_error(e))?;
        // One quote per symbol per batch, from its last step: the web app needs no more.
        for quote in quotes {
            self.emit(ServerEvent::Quote { quote });
        }
        if lock(&self.replay).at_end() {
            self.set_playing(false);
        }
        self.emit(ServerEvent::Replay {
            state: self.replay_state(),
        });
        self.invalidate(&[Topic::Accounts, Topic::Positions]);
        Ok(())
    }

    /// Steps the tape and feeds every quote to the engine. `last` ends up holding the latest
    /// step's quotes, even when that step failed, as in Node.
    fn advance(&mut self, steps: u32, last: &mut Vec<Quote>) -> Result<(), EngineError> {
        for _ in 0..steps {
            let next = lock(&self.replay).step();
            if next.is_empty() {
                break;
            }
            *last = next;
            for quote in last.iter() {
                let result = self.engine.on_quote(quote);
                self.handle_engine_events();
                result?;
            }
            let now = lock(&self.replay).now();
            if now - self.last_replay_equity_at >= REPLAY_EQUITY_EVERY_MS {
                self.last_replay_equity_at = now;
                self.record_equity(DataSource::Replay)?;
                self.invalidate(&[Topic::Arena, Topic::Accounts]);
            }
        }
        Ok(())
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
            tracing::error!(message = %self.redact(&error.to_string()), "live quote failed in the paper engine");
        }
        self.events.emit_live_quote(quote);
    }

    /// Node's `liveTick`, without the parts that need agents or OKX trading: an equity point
    /// for live paper accounts every 60 s, the first one on the first tick.
    fn live_tick(&mut self) {
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
    LiveTick,
}

/// The runtime task: handles one thing at a time until shut down.
async fn run(
    mut runtime: Runtime,
    mut inbox: mpsc::Receiver<Command>,
    mut live_quotes: Option<mpsc::Receiver<Quote>>,
) {
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
            quote = next_live_quote(&mut live_quotes) => match quote {
                Some(quote) => Wake::LiveQuote(quote),
                None => Wake::LiveFeedClosed,
            },
            _ = live_tick.tick() => Wake::LiveTick,
        };
        match wake {
            Wake::Command(command) => command(&mut runtime),
            Wake::HandlesDropped => break,
            Wake::ReplayTick => runtime.on_replay_tick(),
            Wake::FlushInvalidate => runtime.events.flush_invalidate(),
            Wake::LiveQuote(quote) => runtime.on_live_quote(quote),
            Wake::LiveFeedClosed => live_quotes = None,
            Wake::LiveTick => runtime.live_tick(),
        }
        if let Some(done) = runtime.stopping.take() {
            // Node's `stop()`: pause, and drop pending timers. Dropping the runtime closes the
            // database before `shutdown` returns.
            runtime.set_playing(false);
            drop(runtime);
            let _ = done.send(());
            return;
        }
    }
    runtime.set_playing(false);
}

async fn sleep_until_some(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn next_live_quote(quotes: &mut Option<mpsc::Receiver<Quote>>) -> Option<Quote> {
    match quotes {
        Some(quotes) => quotes.recv().await,
        None => std::future::pending().await,
    }
}
