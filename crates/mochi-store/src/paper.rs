//! `PaperStore` over SQLite. The SQL, columns and row converters are the ones in
//! `apps/server/src/db/store.ts`, because both backends read and write the same database file.

use std::cell::Cell;

use mochi_core::paper::{PaperStore, StoreFailure};
use mochi_domain::{
    DataSource, EquityPoint, Fill, LedgerEvent, Order, PaperAccount, Position, RiskSettings,
};
use rusqlite::{params, Connection, Row};
use serde::de::DeserializeOwned;
use serde::Serialize;

pub struct SqlitePaperStore {
    conn: Connection,
    /// Open `transaction` calls: 0 means the next one is a real BEGIN, deeper ones savepoints.
    depth: Cell<u32>,
}

impl SqlitePaperStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn,
            depth: Cell::new(0),
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn exec(&self, sql: &str) -> Result<(), StoreFailure> {
        self.conn.execute_batch(sql).map_err(failure)
    }

    fn query<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        convert: fn(&Row) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>, StoreFailure> {
        let mut stmt = self.conn.prepare_cached(sql).map_err(failure)?;
        let rows = stmt.query_map(params, convert).map_err(failure)?;
        rows.collect::<Result<Vec<T>, _>>().map_err(failure)
    }

    fn query_one<T>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        convert: fn(&Row) -> rusqlite::Result<T>,
    ) -> Result<Option<T>, StoreFailure> {
        Ok(self.query(sql, params, convert)?.into_iter().next())
    }

    fn run(&self, sql: &str, params: impl rusqlite::Params) -> Result<(), StoreFailure> {
        let mut stmt = self.conn.prepare_cached(sql).map_err(failure)?;
        stmt.execute(params).map_err(failure)?;
        Ok(())
    }
}

/// A transaction (or savepoint) that has begun but not ended. If it is dropped unfinished, which
/// happens when the closure panics or when COMMIT/RELEASE fails, it rolls back, so the connection
/// is never left inside a transaction and `depth` always returns to its value before the call.
struct OpenTransaction<'a> {
    store: &'a SqlitePaperStore,
    /// The nesting depth before this transaction began; 0 means a real BEGIN, else a savepoint.
    depth: u32,
    finished: bool,
}

impl OpenTransaction<'_> {
    fn savepoint(&self) -> String {
        format!("sp_{}", self.depth)
    }

    /// Ends successfully with COMMIT at the top level, or RELEASE of the savepoint when nested.
    /// If the statement fails, the transaction stays open and the drop guard rolls it back.
    fn finish(mut self) -> Result<(), StoreFailure> {
        let sql = if self.depth == 0 {
            "COMMIT".to_string()
        } else {
            format!("RELEASE {}", self.savepoint())
        };
        self.store.exec(&sql)?;
        self.end();
        Ok(())
    }

    fn rollback(mut self) -> Result<(), StoreFailure> {
        let result = self.undo();
        self.end();
        result
    }

    fn undo(&self) -> Result<(), StoreFailure> {
        if self.depth == 0 {
            self.store.exec("ROLLBACK")
        } else {
            let sp = self.savepoint();
            self.store.exec(&format!("ROLLBACK TO {sp}; RELEASE {sp}"))
        }
    }

    fn end(&mut self) {
        self.finished = true;
        self.store.depth.set(self.depth);
    }
}

impl Drop for OpenTransaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Nothing useful can be done if this fails while unwinding.
            let _ = self.undo();
            self.store.depth.set(self.depth);
        }
    }
}

fn failure(e: rusqlite::Error) -> StoreFailure {
    StoreFailure(e.to_string())
}

fn json_failure(e: serde_json::Error) -> StoreFailure {
    StoreFailure(e.to_string())
}

/// The text TS stores for an enum: its serde name, e.g. `STOP_MARKET`.
fn enum_text<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        other => panic!("enum did not serialize to a string: {other:?}"),
    }
}

fn conversion_error(column: &str, e: impl std::fmt::Display) -> rusqlite::Error {
    // rusqlite's variant has no slot for a column name, so it goes in the message.
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("column {column}: {e}").into(),
    )
}

fn enum_col<T: DeserializeOwned>(r: &Row, column: &str) -> rusqlite::Result<T> {
    let text: String = r.get(column)?;
    serde_json::from_value(serde_json::Value::String(text)).map_err(|e| conversion_error(column, e))
}

/// A nullable enum column.
fn opt_enum_col<T: DeserializeOwned>(r: &Row, column: &str) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = r.get(column)?;
    text.map(|t| {
        serde_json::from_value(serde_json::Value::String(t))
            .map_err(|e| conversion_error(column, e))
    })
    .transpose()
}

/// A nullable JSON column. NULL and '' both read as none, like the truthiness test in TS.
fn json_col<T: DeserializeOwned>(r: &Row, column: &str) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = r.get(column)?;
    text.filter(|t| !t.is_empty())
        .map(|t| serde_json::from_str(&t).map_err(|e| conversion_error(column, e)))
        .transpose()
}

fn limits_text(limits: &Option<RiskSettings>) -> Result<Option<String>, StoreFailure> {
    limits
        .as_ref()
        .map(|l| serde_json::to_string(l).map_err(json_failure))
        .transpose()
}

fn to_account(r: &Row) -> rusqlite::Result<PaperAccount> {
    // NULL and '' both read as no limits, like the truthiness test on `r.live_limits` in TS.
    let live_limits: Option<String> = r.get("live_limits")?;
    let live_limits = match live_limits.filter(|s| !s.is_empty()) {
        Some(text) => {
            Some(serde_json::from_str(&text).map_err(|e| conversion_error("live_limits", e))?)
        }
        None => None,
    };
    let config: String = r.get("config")?;
    Ok(PaperAccount {
        id: r.get("id")?,
        name: r.get("name")?,
        source: enum_col(r, "source")?,
        venue: enum_col(r, "venue")?,
        exchange_uid: r.get("exchange_uid")?,
        live_limits,
        is_demo: r.get("is_demo")?,
        initial_balance: r.get("initial_balance")?,
        wallet_balance: r.get("wallet_balance")?,
        used_margin: r.get("used_margin")?,
        realized_pnl: r.get("realized_pnl")?,
        fees_paid: r.get("fees_paid")?,
        funding_paid: r.get("funding_paid")?,
        config: serde_json::from_str(&config).map_err(|e| conversion_error("config", e))?,
        created_at: r.get("created_at")?,
    })
}

fn to_order(r: &Row) -> rusqlite::Result<Order> {
    Ok(Order {
        id: r.get("id")?,
        account_id: r.get("account_id")?,
        client_order_id: r.get("client_order_id")?,
        symbol: r.get("symbol")?,
        side: enum_col(r, "side")?,
        order_type: enum_col(r, "type")?,
        quantity: r.get("quantity")?,
        price: r.get("price")?,
        trigger_price: r.get("trigger_price")?,
        leverage: r.get("leverage")?,
        reduce_only: r.get("reduce_only")?,
        close_position: r.get("close_position")?,
        status: enum_col(r, "status")?,
        filled_quantity: r.get("filled_quantity")?,
        avg_fill_price: r.get("avg_fill_price")?,
        reserved_margin: r.get("reserved_margin")?,
        attached_stop_loss: r.get("attached_stop_loss")?,
        attached_take_profit: r.get("attached_take_profit")?,
        trailing_kind: opt_enum_col(r, "trailing_kind")?,
        trailing_callback: r.get("trailing_callback")?,
        trailing_activation_price: r.get("trailing_activation_price")?,
        trailing_extreme: r.get("trailing_extreme")?,
        attached_trailing_stop: json_col(r, "attached_trailing_stop")?,
        reject_reason: r.get("reject_reason")?,
        exchange_order_id: r.get("exchange_order_id")?,
        agent_id: r.get("agent_id")?,
        decision_id: r.get("decision_id")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn to_fill(r: &Row) -> rusqlite::Result<Fill> {
    Ok(Fill {
        id: r.get("id")?,
        order_id: r.get("order_id")?,
        account_id: r.get("account_id")?,
        symbol: r.get("symbol")?,
        side: enum_col(r, "side")?,
        quantity: r.get("quantity")?,
        price: r.get("price")?,
        fee: r.get("fee")?,
        realized_pnl: r.get("realized_pnl")?,
        liquidity: enum_col(r, "liquidity")?,
        ts: r.get("ts")?,
    })
}

fn to_position(r: &Row) -> rusqlite::Result<Position> {
    Ok(Position {
        id: r.get("id")?,
        account_id: r.get("account_id")?,
        symbol: r.get("symbol")?,
        side: enum_col(r, "side")?,
        quantity: r.get("quantity")?,
        entry_price: r.get("entry_price")?,
        leverage: r.get("leverage")?,
        margin: r.get("margin")?,
        liquidation_price: r.get("liquidation_price")?,
        status: enum_col(r, "status")?,
        realized_pnl: r.get("realized_pnl")?,
        fees_paid: r.get("fees_paid")?,
        funding_paid: r.get("funding_paid")?,
        max_quantity: r.get("max_quantity")?,
        exit_price: r.get("exit_price")?,
        last_funding_time: r.get("last_funding_time")?,
        opened_at: r.get("opened_at")?,
        closed_at: r.get("closed_at")?,
    })
}

fn to_ledger(r: &Row) -> rusqlite::Result<LedgerEvent> {
    Ok(LedgerEvent {
        seq: r.get("seq")?,
        id: r.get("id")?,
        account_id: r.get("account_id")?,
        event_type: enum_col(r, "type")?,
        amount: r.get("amount")?,
        margin_delta: r.get("margin_delta")?,
        wallet_after: r.get("wallet_after")?,
        symbol: r.get("symbol")?,
        order_id: r.get("order_id")?,
        fill_id: r.get("fill_id")?,
        position_id: r.get("position_id")?,
        memo: r.get("memo")?,
        ts: r.get("ts")?,
    })
}

fn to_equity(r: &Row) -> rusqlite::Result<EquityPoint> {
    Ok(EquityPoint {
        account_id: r.get("account_id")?,
        ts: r.get("ts")?,
        equity: r.get("equity")?,
        wallet: r.get("wallet")?,
        unrealized: r.get("unrealized")?,
    })
}

fn limit_param(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

impl PaperStore for SqlitePaperStore {
    fn transaction<T, E: From<StoreFailure>>(
        &self,
        f: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let depth = self.depth.get();
        // IMMEDIATE takes the write lock up front, as the Node store does.
        if depth == 0 {
            self.exec("BEGIN IMMEDIATE")?;
        } else {
            self.exec(&format!("SAVEPOINT sp_{depth}"))?;
        }
        self.depth.set(depth + 1);
        let open = OpenTransaction {
            store: self,
            depth,
            finished: false,
        };
        match f() {
            Ok(value) => {
                open.finish()?;
                Ok(value)
            }
            // The closure's own error is the one worth reporting, even if the rollback also fails.
            Err(e) => {
                let _ = open.rollback();
                Err(e)
            }
        }
    }

    fn insert_account(&self, a: &PaperAccount) -> Result<(), StoreFailure> {
        self.run(
            "INSERT INTO accounts (id,name,source,venue,exchange_uid,live_limits,is_demo,initial_balance,wallet_balance,used_margin,realized_pnl,fees_paid,funding_paid,config,created_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                a.id,
                a.name,
                enum_text(&a.source),
                enum_text(&a.venue),
                a.exchange_uid,
                limits_text(&a.live_limits)?,
                a.is_demo,
                a.initial_balance,
                a.wallet_balance,
                a.used_margin,
                a.realized_pnl,
                a.fees_paid,
                a.funding_paid,
                serde_json::to_string(&a.config).map_err(json_failure)?,
                a.created_at,
            ],
        )
    }

    fn save_account(&self, a: &PaperAccount) -> Result<(), StoreFailure> {
        self.run(
            "UPDATE accounts SET wallet_balance=?, used_margin=?, realized_pnl=?, fees_paid=?, funding_paid=? WHERE id=?",
            params![
                a.wallet_balance,
                a.used_margin,
                a.realized_pnl,
                a.fees_paid,
                a.funding_paid,
                a.id
            ],
        )
    }

    fn get_account(&self, id: &str) -> Result<Option<PaperAccount>, StoreFailure> {
        self.query_one("SELECT * FROM accounts WHERE id=?", [id], to_account)
    }

    fn list_accounts(&self) -> Result<Vec<PaperAccount>, StoreFailure> {
        self.query(
            "SELECT * FROM accounts ORDER BY created_at, id",
            [],
            to_account,
        )
    }

    fn delete_account_data(&self, id: &str, keep_account_row: bool) -> Result<(), StoreFailure> {
        for table in [
            "fills",
            "orders",
            "positions",
            "ledger_events",
            "equity_history",
        ] {
            self.run(&format!("DELETE FROM {table} WHERE account_id=?"), [id])?;
        }
        // Decisions belong to the agents tables, but Node clears them here too, and a left-over
        // row would block deleting the account.
        self.run("DELETE FROM decisions WHERE account_id=?", [id])?;
        if !keep_account_row {
            self.run("DELETE FROM accounts WHERE id=?", [id])?;
        }
        Ok(())
    }

    fn insert_order(&self, o: &Order) -> Result<(), StoreFailure> {
        self.run(
            "INSERT INTO orders (id,account_id,client_order_id,symbol,side,type,quantity,price,trigger_price,leverage,reduce_only,close_position,status,
              filled_quantity,avg_fill_price,reserved_margin,attached_stop_loss,attached_take_profit,trailing_kind,trailing_callback,trailing_activation_price,trailing_extreme,
              attached_trailing_stop,reject_reason,exchange_order_id,agent_id,decision_id,created_at,updated_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                o.id,
                o.account_id,
                o.client_order_id,
                o.symbol,
                enum_text(&o.side),
                enum_text(&o.order_type),
                o.quantity,
                o.price,
                o.trigger_price,
                o.leverage,
                o.reduce_only,
                o.close_position,
                enum_text(&o.status),
                o.filled_quantity,
                o.avg_fill_price,
                o.reserved_margin,
                o.attached_stop_loss,
                o.attached_take_profit,
                o.trailing_kind.as_ref().map(enum_text),
                o.trailing_callback,
                o.trailing_activation_price,
                o.trailing_extreme,
                o.attached_trailing_stop
                    .as_ref()
                    .map(|spec| serde_json::to_string(spec).expect("a trailing spec is plain data")),
                o.reject_reason,
                o.exchange_order_id,
                o.agent_id,
                o.decision_id,
                o.created_at,
                o.updated_at,
            ],
        )
    }

    fn save_order(&self, o: &Order) -> Result<(), StoreFailure> {
        self.run(
            "UPDATE orders SET quantity=?, status=?, filled_quantity=?, avg_fill_price=?, reserved_margin=?, trailing_extreme=?, reject_reason=?, exchange_order_id=?, updated_at=? WHERE id=?",
            params![
                o.quantity,
                enum_text(&o.status),
                o.filled_quantity,
                o.avg_fill_price,
                o.reserved_margin,
                o.trailing_extreme,
                o.reject_reason,
                o.exchange_order_id,
                o.updated_at,
                o.id,
            ],
        )
    }

    fn get_order(&self, id: &str) -> Result<Option<Order>, StoreFailure> {
        self.query_one("SELECT * FROM orders WHERE id=?", [id], to_order)
    }

    fn get_order_by_client_id(
        &self,
        account_id: &str,
        client_order_id: &str,
    ) -> Result<Option<Order>, StoreFailure> {
        self.query_one(
            "SELECT * FROM orders WHERE account_id=? AND client_order_id=?",
            [account_id, client_order_id],
            to_order,
        )
    }

    fn list_open_orders(&self, account_id: &str) -> Result<Vec<Order>, StoreFailure> {
        self.query(
            "SELECT * FROM orders WHERE account_id=? AND status='OPEN' ORDER BY created_at, rowid",
            [account_id],
            to_order,
        )
    }

    fn list_open_orders_for_market(
        &self,
        source: DataSource,
        symbol: &str,
    ) -> Result<Vec<Order>, StoreFailure> {
        self.query(
            "SELECT o.* FROM orders o JOIN accounts a ON a.id=o.account_id
             WHERE o.status='OPEN' AND o.symbol=? AND a.source=? ORDER BY o.created_at, o.rowid",
            [symbol, enum_text(&source).as_str()],
            to_order,
        )
    }

    fn list_orders(&self, account_id: &str, limit: usize) -> Result<Vec<Order>, StoreFailure> {
        self.query(
            "SELECT * FROM orders WHERE account_id=? ORDER BY created_at DESC, rowid DESC LIMIT ?",
            params![account_id, limit_param(limit)],
            to_order,
        )
    }

    fn insert_fill(&self, f: &Fill) -> Result<(), StoreFailure> {
        self.run(
            "INSERT INTO fills (id,order_id,account_id,symbol,side,quantity,price,fee,realized_pnl,liquidity,ts) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            params![
                f.id,
                f.order_id,
                f.account_id,
                f.symbol,
                enum_text(&f.side),
                f.quantity,
                f.price,
                f.fee,
                f.realized_pnl,
                enum_text(&f.liquidity),
                f.ts,
            ],
        )
    }

    fn list_fills_by_order(&self, order_id: &str) -> Result<Vec<Fill>, StoreFailure> {
        self.query(
            "SELECT * FROM fills WHERE order_id=? ORDER BY ts, rowid",
            [order_id],
            to_fill,
        )
    }

    fn list_fills(&self, account_id: &str, limit: usize) -> Result<Vec<Fill>, StoreFailure> {
        self.query(
            "SELECT * FROM fills WHERE account_id=? ORDER BY ts DESC, rowid DESC LIMIT ?",
            params![account_id, limit_param(limit)],
            to_fill,
        )
    }

    fn insert_position(&self, p: &Position) -> Result<(), StoreFailure> {
        self.run(
            "INSERT INTO positions (id,account_id,symbol,side,quantity,entry_price,leverage,margin,liquidation_price,status,realized_pnl,fees_paid,
              funding_paid,max_quantity,exit_price,last_funding_time,opened_at,closed_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                p.id,
                p.account_id,
                p.symbol,
                enum_text(&p.side),
                p.quantity,
                p.entry_price,
                p.leverage,
                p.margin,
                p.liquidation_price,
                enum_text(&p.status),
                p.realized_pnl,
                p.fees_paid,
                p.funding_paid,
                p.max_quantity,
                p.exit_price,
                p.last_funding_time,
                p.opened_at,
                p.closed_at,
            ],
        )
    }

    fn save_position(&self, p: &Position) -> Result<(), StoreFailure> {
        self.run(
            "UPDATE positions SET quantity=?, entry_price=?, margin=?, liquidation_price=?, status=?, realized_pnl=?, fees_paid=?, funding_paid=?,
              max_quantity=?, exit_price=?, last_funding_time=?, closed_at=? WHERE id=?",
            params![
                p.quantity,
                p.entry_price,
                p.margin,
                p.liquidation_price,
                enum_text(&p.status),
                p.realized_pnl,
                p.fees_paid,
                p.funding_paid,
                p.max_quantity,
                p.exit_price,
                p.last_funding_time,
                p.closed_at,
                p.id,
            ],
        )
    }

    fn get_open_position(
        &self,
        account_id: &str,
        symbol: &str,
    ) -> Result<Option<Position>, StoreFailure> {
        self.query_one(
            "SELECT * FROM positions WHERE account_id=? AND symbol=? AND status='OPEN'",
            [account_id, symbol],
            to_position,
        )
    }

    fn list_open_positions(&self, account_id: &str) -> Result<Vec<Position>, StoreFailure> {
        self.query(
            "SELECT * FROM positions WHERE account_id=? AND status='OPEN' ORDER BY opened_at",
            [account_id],
            to_position,
        )
    }

    fn list_open_positions_for_market(
        &self,
        source: DataSource,
        symbol: &str,
    ) -> Result<Vec<Position>, StoreFailure> {
        self.query(
            "SELECT p.* FROM positions p JOIN accounts a ON a.id=p.account_id
             WHERE p.status='OPEN' AND p.symbol=? AND a.source=?",
            [symbol, enum_text(&source).as_str()],
            to_position,
        )
    }

    fn list_closed_positions(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<Position>, StoreFailure> {
        self.query(
            "SELECT * FROM positions WHERE account_id=? AND status<>'OPEN' ORDER BY closed_at DESC LIMIT ?",
            params![account_id, limit_param(limit)],
            to_position,
        )
    }

    fn append_ledger(&self, e: &LedgerEvent) -> Result<(), StoreFailure> {
        self.run(
            "INSERT INTO ledger_events (id,account_id,type,amount,margin_delta,wallet_after,symbol,order_id,fill_id,position_id,memo,ts)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                e.id,
                e.account_id,
                enum_text(&e.event_type),
                e.amount,
                e.margin_delta,
                e.wallet_after,
                e.symbol,
                e.order_id,
                e.fill_id,
                e.position_id,
                e.memo,
                e.ts,
            ],
        )
    }

    fn list_ledger(
        &self,
        account_id: &str,
        since_ts: Option<i64>,
    ) -> Result<Vec<LedgerEvent>, StoreFailure> {
        self.query(
            "SELECT * FROM ledger_events WHERE account_id=? AND ts>=? ORDER BY seq",
            params![account_id, since_ts.unwrap_or(0)],
            to_ledger,
        )
    }

    fn upsert_equity_point(&self, p: &EquityPoint) -> Result<(), StoreFailure> {
        self.run(
            "INSERT INTO equity_history (account_id,ts,equity,wallet,unrealized) VALUES (?,?,?,?,?)
             ON CONFLICT(account_id,ts) DO UPDATE SET equity=excluded.equity, wallet=excluded.wallet, unrealized=excluded.unrealized",
            params![p.account_id, p.ts, p.equity, p.wallet, p.unrealized],
        )
    }

    fn list_equity(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<EquityPoint>, StoreFailure> {
        self.query(
            "SELECT * FROM (SELECT * FROM equity_history WHERE account_id=? ORDER BY ts DESC LIMIT ?) ORDER BY ts",
            params![account_id, limit_param(limit)],
            to_equity,
        )
    }
}
