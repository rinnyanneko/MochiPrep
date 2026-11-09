//! Persistence the paper engine needs. Synchronous on purpose: every financial change happens
//! inside one `transaction`, with no await point where another fill could interleave. The engine
//! wraps even its single-row writes in one, so a store can rely on every write being in a
//! transaction. The methods mirror `packages/core/src/paper/store.ts` one for one.

use mochi_domain::{DataSource, EquityPoint, Fill, LedgerEvent, Order, PaperAccount, Position};

/// A storage error. The engine's reaction is the same whatever the cause (roll back and report),
/// but its owner may retry when another connection held the database, so that kind is kept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct StoreFailure {
    pub kind: StoreFailureKind,
    pub message: String,
}

/// The SQLite result codes a caller can act on. Everything else is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreFailureKind {
    /// SQLITE_BUSY: another connection holds the lock. The same call can succeed later.
    Busy,
    /// SQLITE_LOCKED: a conflict inside this connection, such as a table locked by a statement
    /// still running.
    Locked,
    Other,
}

impl StoreFailure {
    pub fn new(kind: StoreFailureKind, message: impl Into<String>) -> Self {
        StoreFailure {
            kind,
            message: message.into(),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.kind == StoreFailureKind::Busy
    }
}

pub trait PaperStore {
    /// Runs `f` atomically; an `Err` rolls back everything `f` wrote. Nested calls use a
    /// savepoint, so only the inner work is undone when the inner call fails.
    // The generics stay on the method (not the trait) so callers can use their own error type.
    fn transaction<T, E: From<StoreFailure>>(
        &self,
        f: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E>;

    fn insert_account(&self, account: &PaperAccount) -> Result<(), StoreFailure>;
    /// Writes the balances only; name, config and limits never change through here.
    fn save_account(&self, account: &PaperAccount) -> Result<(), StoreFailure>;
    fn get_account(&self, id: &str) -> Result<Option<PaperAccount>, StoreFailure>;
    fn list_accounts(&self) -> Result<Vec<PaperAccount>, StoreFailure>;
    fn delete_account_data(&self, id: &str, keep_account_row: bool) -> Result<(), StoreFailure>;

    fn insert_order(&self, order: &Order) -> Result<(), StoreFailure>;
    fn save_order(&self, order: &Order) -> Result<(), StoreFailure>;
    fn get_order(&self, id: &str) -> Result<Option<Order>, StoreFailure>;
    fn get_order_by_client_id(
        &self,
        account_id: &str,
        client_order_id: &str,
    ) -> Result<Option<Order>, StoreFailure>;
    fn list_open_orders(&self, account_id: &str) -> Result<Vec<Order>, StoreFailure>;
    fn list_open_orders_for_market(
        &self,
        source: DataSource,
        symbol: &str,
    ) -> Result<Vec<Order>, StoreFailure>;
    fn list_orders(&self, account_id: &str, limit: usize) -> Result<Vec<Order>, StoreFailure>;

    fn insert_fill(&self, fill: &Fill) -> Result<(), StoreFailure>;
    fn list_fills_by_order(&self, order_id: &str) -> Result<Vec<Fill>, StoreFailure>;
    fn list_fills(&self, account_id: &str, limit: usize) -> Result<Vec<Fill>, StoreFailure>;

    fn insert_position(&self, position: &Position) -> Result<(), StoreFailure>;
    fn save_position(&self, position: &Position) -> Result<(), StoreFailure>;
    fn get_open_position(
        &self,
        account_id: &str,
        symbol: &str,
    ) -> Result<Option<Position>, StoreFailure>;
    fn list_open_positions(&self, account_id: &str) -> Result<Vec<Position>, StoreFailure>;
    fn list_open_positions_for_market(
        &self,
        source: DataSource,
        symbol: &str,
    ) -> Result<Vec<Position>, StoreFailure>;
    fn list_closed_positions(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<Position>, StoreFailure>;

    /// `event.seq` is ignored: the database assigns it (TS takes `Omit<LedgerEvent, 'seq'>`).
    fn append_ledger(&self, event: &LedgerEvent) -> Result<(), StoreFailure>;
    /// Events with `ts >= since_ts` (0 when `None`), in `seq` order.
    fn list_ledger(
        &self,
        account_id: &str,
        since_ts: Option<i64>,
    ) -> Result<Vec<LedgerEvent>, StoreFailure>;

    fn upsert_equity_point(&self, point: &EquityPoint) -> Result<(), StoreFailure>;
    /// The latest `limit` points, oldest first.
    fn list_equity(&self, account_id: &str, limit: usize)
        -> Result<Vec<EquityPoint>, StoreFailure>;
}
