//! Paper engine rules settled for the Rust server (Plan 3, Task 4) that the Node scenario fixtures
//! cannot show: how `on_quote` reports events around a failure, that it never checks quote age,
//! that every engine write runs inside `PaperStore::transaction`, and that a store failure keeps
//! SQLite's busy signal.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mochi_core::paper::{
    CreateAccountArgs, EngineEvent, PaperEngineDeps, PaperExecutionEngine, PaperStore,
    StoreFailure, StoreFailureKind,
};
use mochi_domain::registry::Instruments;
use mochi_domain::{
    DataSource, EquityPoint, Fill, LedgerEvent, Order, OrderRequest, OrderStatus, PaperAccount,
    Position, Protection, Quote,
};
use mochi_store::{open_database, open_in_memory, SqlitePaperStore};
use serde_json::json;

const HOUR: i64 = 3_600_000;

fn quote(price: &str, ts: i64) -> Quote {
    serde_json::from_value(json!({
        "source": "REPLAY", "symbol": "SOLUSDT", "bid": price, "ask": price, "last": price,
        "mark": price, "markEstimated": false, "fundingRate": "0.0001", "nextFundingTime": null,
        "ts": ts,
    }))
    .unwrap()
}

struct Harness<S: PaperStore> {
    engine: PaperExecutionEngine<S>,
    quotes: Arc<Mutex<Vec<Quote>>>,
    clock: Arc<Mutex<i64>>,
    events: Arc<Mutex<Vec<EngineEvent>>>,
}

impl<S: PaperStore> Harness<S> {
    fn new(store: S) -> Self {
        let quotes: Arc<Mutex<Vec<Quote>>> = Arc::default();
        let clock = Arc::new(Mutex::new(HOUR));
        let events: Arc<Mutex<Vec<EngineEvent>>> = Arc::default();
        let ids = AtomicU64::new(0);
        let (q, c, e) = (quotes.clone(), clock.clone(), events.clone());
        let engine = PaperExecutionEngine::new(PaperEngineDeps {
            store,
            instruments: Instruments::with_defaults(),
            get_quote: Box::new(move |source, symbol| {
                q.lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|quote| quote.source == source && quote.symbol == symbol)
                    .cloned()
            }),
            now: Box::new(move |_| *c.lock().unwrap()),
            new_id: Box::new(move |prefix| {
                format!("{prefix}_{}", ids.fetch_add(1, Ordering::Relaxed) + 1)
            }),
            on_event: Box::new(move |event| e.lock().unwrap().push(event)),
        });
        Harness {
            engine,
            quotes,
            clock,
            events,
        }
    }

    fn set_quote(&self, quote: Quote) {
        self.quotes.lock().unwrap().push(quote);
    }

    fn account(&mut self, name: &str) -> PaperAccount {
        self.engine
            .create_account(CreateAccountArgs {
                name: name.into(),
                source: DataSource::Replay,
                initial_balance: Some("10000".into()),
                is_demo: None,
                config: Default::default(),
            })
            .unwrap()
    }

    fn take_events(&self) -> Vec<EngineEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

fn request(account_id: &str, client_order_id: &str, extra: serde_json::Value) -> OrderRequest {
    let mut body = json!({
        "accountId": account_id, "symbol": "SOLUSDT", "side": "BUY", "type": "MARKET",
        "quantity": "1", "leverage": 5, "clientOrderId": client_order_id,
    });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(body).unwrap()
}

fn funded_accounts(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Funding { account_id, .. } => Some(account_id.clone()),
            _ => None,
        })
        .collect()
}

/// A failing step must not hide what earlier steps committed, and must not leave its own
/// rolled-back events behind to be emitted by a later quote. This is how `set_protection`
/// already behaves.
#[test]
fn on_quote_flushes_committed_steps_and_drops_rolled_back_ones() {
    let mut h = Harness::new(SqlitePaperStore::new(open_in_memory().unwrap()));
    h.set_quote(quote("100", HOUR));
    let a = h.account("a");
    let b = h.account("b");
    h.engine
        .place_order(request(&a.id, "a", json!({})))
        .unwrap();
    h.engine
        .place_order(request(&b.id, "b", json!({})))
        .unwrap();
    let b_position = h
        .engine
        .store()
        .get_open_position(&b.id, "SOLUSDT")
        .unwrap()
        .unwrap();
    h.take_events();

    // B's funding step queues its event, then fails to save, so its transaction rolls back.
    h.engine
        .store()
        .connection()
        .execute_batch(&format!(
            "CREATE TRIGGER fail_b BEFORE UPDATE ON positions WHEN NEW.id = '{}' \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            b_position.id
        ))
        .unwrap();
    let boundary = quote("100", 8 * HOUR);
    let error = h.engine.on_quote(&boundary).unwrap_err();
    assert!(error.to_string().contains("disk full"), "{error}");
    assert_eq!(funded_accounts(&h.take_events()), vec![a.id.clone()]);

    // Once the store works again, the same quote funds only B: A's step had committed, and B's
    // rolled-back event was dropped rather than kept for this flush.
    h.engine
        .store()
        .connection()
        .execute_batch("DROP TRIGGER fail_b")
        .unwrap();
    h.engine.on_quote(&boundary).unwrap();
    assert_eq!(funded_accounts(&h.take_events()), vec![b.id.clone()]);
}

/// Resting orders fill on whatever quote arrives, however old: the 60 s age rule protects new
/// orders being priced, not the replay of observed prices.
#[test]
fn on_quote_has_no_quote_age_check() {
    let mut h = Harness::new(SqlitePaperStore::new(open_in_memory().unwrap()));
    *h.clock.lock().unwrap() = 10 * HOUR;
    h.set_quote(quote("100", 10 * HOUR));
    let account = h.account("aged");
    let limit = json!({ "type": "LIMIT", "price": "90" });
    let placed = h
        .engine
        .place_order(request(&account.id, "limit", limit))
        .unwrap();
    assert_eq!(placed.order.status, OrderStatus::Open);

    // Nine hours older than the engine's clock.
    h.engine.on_quote(&quote("89", HOUR)).unwrap();
    let order = h
        .engine
        .store()
        .get_order(&placed.order.id)
        .unwrap()
        .unwrap();
    assert_eq!(order.status, OrderStatus::Filled);

    // Placing a new order on that stale quote is still refused.
    h.set_quote(quote("89", HOUR));
    let refused = h
        .engine
        .place_order(request(&account.id, "stale", json!({})))
        .unwrap();
    assert_eq!(refused.order.status, OrderStatus::Rejected);
    assert!(refused
        .order
        .reject_reason
        .unwrap()
        .starts_with("MARKET_DATA_STALE"));
}

/// Records every write the engine makes while no `transaction` is open.
struct AuditedStore {
    inner: SqlitePaperStore,
    depth: Cell<u32>,
    outside: RefCell<Vec<&'static str>>,
}

impl AuditedStore {
    fn write(&self, name: &'static str) {
        if self.depth.get() == 0 {
            self.outside.borrow_mut().push(name);
        }
    }
}

impl PaperStore for AuditedStore {
    fn transaction<T, E: From<StoreFailure>>(
        &self,
        f: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        self.depth.set(self.depth.get() + 1);
        let result = self.inner.transaction(f);
        self.depth.set(self.depth.get() - 1);
        result
    }
    fn insert_account(&self, a: &PaperAccount) -> Result<(), StoreFailure> {
        self.write("insert_account");
        self.inner.insert_account(a)
    }
    fn save_account(&self, a: &PaperAccount) -> Result<(), StoreFailure> {
        self.write("save_account");
        self.inner.save_account(a)
    }
    fn get_account(&self, id: &str) -> Result<Option<PaperAccount>, StoreFailure> {
        self.inner.get_account(id)
    }
    fn list_accounts(&self) -> Result<Vec<PaperAccount>, StoreFailure> {
        self.inner.list_accounts()
    }
    fn delete_account_data(&self, id: &str, keep: bool) -> Result<(), StoreFailure> {
        self.write("delete_account_data");
        self.inner.delete_account_data(id, keep)
    }
    fn insert_order(&self, o: &Order) -> Result<(), StoreFailure> {
        self.write("insert_order");
        self.inner.insert_order(o)
    }
    fn save_order(&self, o: &Order) -> Result<(), StoreFailure> {
        self.write("save_order");
        self.inner.save_order(o)
    }
    fn get_order(&self, id: &str) -> Result<Option<Order>, StoreFailure> {
        self.inner.get_order(id)
    }
    fn get_order_by_client_id(
        &self,
        account_id: &str,
        client_order_id: &str,
    ) -> Result<Option<Order>, StoreFailure> {
        self.inner
            .get_order_by_client_id(account_id, client_order_id)
    }
    fn list_open_orders(&self, account_id: &str) -> Result<Vec<Order>, StoreFailure> {
        self.inner.list_open_orders(account_id)
    }
    fn list_open_orders_for_market(
        &self,
        source: DataSource,
        symbol: &str,
    ) -> Result<Vec<Order>, StoreFailure> {
        self.inner.list_open_orders_for_market(source, symbol)
    }
    fn list_orders(&self, account_id: &str, limit: usize) -> Result<Vec<Order>, StoreFailure> {
        self.inner.list_orders(account_id, limit)
    }
    fn insert_fill(&self, f: &Fill) -> Result<(), StoreFailure> {
        self.write("insert_fill");
        self.inner.insert_fill(f)
    }
    fn list_fills_by_order(&self, order_id: &str) -> Result<Vec<Fill>, StoreFailure> {
        self.inner.list_fills_by_order(order_id)
    }
    fn list_fills(&self, account_id: &str, limit: usize) -> Result<Vec<Fill>, StoreFailure> {
        self.inner.list_fills(account_id, limit)
    }
    fn insert_position(&self, p: &Position) -> Result<(), StoreFailure> {
        self.write("insert_position");
        self.inner.insert_position(p)
    }
    fn save_position(&self, p: &Position) -> Result<(), StoreFailure> {
        self.write("save_position");
        self.inner.save_position(p)
    }
    fn get_open_position(
        &self,
        account_id: &str,
        symbol: &str,
    ) -> Result<Option<Position>, StoreFailure> {
        self.inner.get_open_position(account_id, symbol)
    }
    fn list_open_positions(&self, account_id: &str) -> Result<Vec<Position>, StoreFailure> {
        self.inner.list_open_positions(account_id)
    }
    fn list_open_positions_for_market(
        &self,
        source: DataSource,
        symbol: &str,
    ) -> Result<Vec<Position>, StoreFailure> {
        self.inner.list_open_positions_for_market(source, symbol)
    }
    fn list_closed_positions(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<Position>, StoreFailure> {
        self.inner.list_closed_positions(account_id, limit)
    }
    fn append_ledger(&self, e: &LedgerEvent) -> Result<(), StoreFailure> {
        self.write("append_ledger");
        self.inner.append_ledger(e)
    }
    fn list_ledger(
        &self,
        account_id: &str,
        since_ts: Option<i64>,
    ) -> Result<Vec<LedgerEvent>, StoreFailure> {
        self.inner.list_ledger(account_id, since_ts)
    }
    fn upsert_equity_point(&self, p: &EquityPoint) -> Result<(), StoreFailure> {
        self.write("upsert_equity_point");
        self.inner.upsert_equity_point(p)
    }
    fn list_equity(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<EquityPoint>, StoreFailure> {
        self.inner.list_equity(account_id, limit)
    }
}

/// Every path that writes: accounts, a rejected order, a cancel, a fill, protection, a trailing
/// stop ratcheting, a protection order left without its position, liquidation and equity.
#[test]
fn every_engine_write_is_inside_a_transaction() {
    let mut h = Harness::new(AuditedStore {
        inner: SqlitePaperStore::new(open_in_memory().unwrap()),
        depth: Cell::new(0),
        outside: RefCell::new(Vec::new()),
    });
    h.set_quote(quote("100", HOUR));
    let account = h.account("audited");
    let other = h.account("orphan");
    h.engine.reset_account(&other.id).unwrap();

    // Rejected: far more than the margin allows.
    let huge = json!({ "quantity": "100000" });
    let rejected = h
        .engine
        .place_order(request(&account.id, "huge", huge))
        .unwrap();
    assert_eq!(rejected.order.status, OrderStatus::Rejected);

    // A resting limit, cancelled.
    let limit = json!({ "type": "LIMIT", "price": "50" });
    let resting = h
        .engine
        .place_order(request(&account.id, "rest", limit))
        .unwrap();
    h.engine.cancel_order(&resting.order.id).unwrap();

    // A position with a trailing stop that ratchets on a higher quote.
    h.engine
        .place_order(request(&account.id, "open", json!({})))
        .unwrap();
    let trailing = serde_json::from_value(json!({ "kind": "RATIO", "callback": "0.05" })).unwrap();
    let protection = Protection {
        stop_loss: None,
        take_profit: None,
        trailing_stop: Some(trailing),
    };
    h.engine
        .set_protection(&account.id, "SOLUSDT", &protection)
        .unwrap();
    h.engine.on_quote(&quote("110", HOUR + 60_000)).unwrap();

    // A stop-loss whose position is gone (here by hand) is cancelled on the next quote.
    h.engine
        .place_order(request(&other.id, "other", json!({ "stopLoss": "80" })))
        .unwrap();
    h.engine
        .store()
        .inner
        .connection()
        .execute(
            "UPDATE positions SET status='CLOSED' WHERE account_id=?",
            [&other.id],
        )
        .unwrap();
    h.engine.on_quote(&quote("111", HOUR + 120_000)).unwrap();

    // Liquidation, then an equity point.
    h.engine.on_quote(&quote("1", HOUR + 180_000)).unwrap();
    h.engine.record_equity(&account.id).unwrap();

    let liquidated = h
        .take_events()
        .iter()
        .any(|event| matches!(event, EngineEvent::Liquidation { .. }));
    assert!(liquidated, "the scenario must reach liquidation");
    let outside = h.engine.store().outside.borrow().clone();
    assert!(
        outside.is_empty(),
        "writes outside a transaction: {outside:?}"
    );
}

#[test]
fn store_failure_keeps_the_busy_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.db");
    let holder = open_database(&path).unwrap();
    let conn = open_database(&path).unwrap();
    conn.busy_timeout(Duration::ZERO).unwrap();
    let store = SqlitePaperStore::new(conn);

    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let failure = store
        .transaction(|| Ok::<(), StoreFailure>(()))
        .unwrap_err();
    assert_eq!(failure.kind, StoreFailureKind::Busy);
    assert!(failure.is_busy());
    holder.execute_batch("ROLLBACK").unwrap();
    store.transaction(|| Ok::<(), StoreFailure>(())).unwrap();

    // Any other failure, here a duplicate primary key, is not busy.
    let mut h = Harness::new(store);
    let account = h.account("twice");
    let failure = h.engine.store().insert_account(&account).unwrap_err();
    assert_eq!(failure.kind, StoreFailureKind::Other);
    assert!(failure.message.contains("UNIQUE"), "{}", failure.message);
}
