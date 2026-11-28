//! OKX's closed-position history, kept locally, and what the open positions have made so far. The
//! arena reads both to tell what an agent's pair made without counting deposits and transfers,
//! which move the account's equity just as much.

use std::collections::HashMap;

use crate::venue::{
    self, nullish, strict, truthy, VenueAdapter, VenueBill, VenueClosedPosition, VenueField,
    VenueOrder, VenuePosition,
};
use mochi_domain::{ExchangePositionRecord, OrderSide};

use super::{dec, is_leg, EngineError, ExchangeEngine, Registry};

/// How often the history is read.
const HISTORY_EVERY_MS: i64 = 60_000;
/// Pages read per pass. A longer history is read over several passes.
const HISTORY_PAGES: usize = 10;
const EXECUTION_PAGES: usize = 3;

/// An open position's result so far, as OKX reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPositionResult {
    pub symbol: String,
    pub pos_id: String,
    /// OKX's `cTime`; `None` when OKX gave none, and then no agent can be said to own it.
    pub opened_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub opening_side: Option<String>,
    /// OKX's `realizedPnl` on the open position: fees, funding and partial closes so far.
    pub realized_pnl: Option<String>,
    pub unrealized_pnl: Option<String>,
}

/// Clears the in-flight flag however a pass ends.
struct Running<'a>(&'a ExchangeEngine);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.state().history_running = false;
    }
}

impl ExchangeEngine {
    /// One pass, at most once a minute and never two at a time. It takes no part in the order
    /// queue, so it never holds up an order or an exit; the sync loop runs it as a task of its own
    /// (see [`ExchangeEngine::spawn_position_history`]) so it never holds up a sync either.
    ///
    /// The pass reads from the newest close back until it meets what is recorded, then carries on
    /// with any older history still unread, within a page budget. It writes everything, records
    /// and the backfill cursor, in one transaction at the end: a pass that fails part-way writes
    /// nothing and the next one starts over.
    pub async fn sync_position_history(&self) -> Result<(), EngineError> {
        let now = self.now();
        {
            let mut state = self.state();
            if state.history_running || now - state.last_history_at < HISTORY_EVERY_MS {
                return Ok(());
            }
            state.history_running = true;
        }
        let _running = Running(self);
        let account_id = self.deps.account_id.clone();
        let (newest, mut backfill) = {
            let account_id = account_id.clone();
            self.rows(move |rows| {
                Ok((
                    rows.latest_position_record_at(&account_id)?,
                    rows.position_history_backfill(&account_id)?,
                ))
            })
            .await?
        };
        let mut records = Vec::new();
        let mut budget = HISTORY_PAGES;

        // Newest first, down to what is recorded.
        let read = self
            .read_back(None, newest, &mut budget, &mut records, &account_id)
            .await?;
        match read {
            Read::End => backfill = None,
            Read::Met => {}
            // Out of budget before meeting the records: the gap is read as backfill.
            Read::Stopped(oldest) => {
                backfill = Some(backfill.map_or(oldest, |before| before.max(oldest)));
            }
        }
        // Then older history still unread.
        if let Some(before) = backfill {
            if budget > 0 {
                backfill = match self
                    .read_back(Some(before), None, &mut budget, &mut records, &account_id)
                    .await?
                {
                    Read::End | Read::Met => None,
                    Read::Stopped(oldest) => Some(oldest),
                };
            }
        }

        let account_for_records = account_id.clone();
        self.rows(move |rows| {
            rows.transaction(&mut || {
                for record in &records {
                    rows.upsert_position_record(record)?;
                }
                rows.set_position_history_backfill(&account_for_records, backfill)?;
                Ok(())
            })
        })
        .await?;
        let execution_result = self.read_execution_history(&account_id).await;
        self.state().last_history_at = now;
        execution_result
    }

    /// In each pass, continue the active scan until its old watermark or the end of retention.
    /// The cursor commits with each page so an interruption cannot create a gap or repeat a trade.
    async fn read_execution_history(&self, account_id: &str) -> Result<(), EngineError> {
        let account = account_id.to_string();
        let mut cursor = self
            .rows(move |rows| Ok(rows.execution_cursor(&account)?))
            .await?;
        let completed = cursor
            .completed_bill
            .as_deref()
            .and_then(|id| id.parse::<u64>().ok());
        for _ in 0..EXECUTION_PAGES {
            let after = cursor
                .scan_after
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok());
            let page = self.deps.venue.execution_history(after).await?;
            let bills = page
                .iter()
                .map(|row| {
                    strict(&row.bill_id)
                        .and_then(|id| id.parse::<u64>().ok())
                        .ok_or_else(|| {
                            EngineError::message(format!(
                                "{} execution history is missing a bill ID",
                                self.deps.venue.name()
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let oldest = bills.iter().copied().min();
            if let Some(newest) = bills.iter().copied().max() {
                if after.is_some_and(|after| newest >= after) {
                    return Err(EngineError::message(format!(
                        "{} execution history pagination did not advance",
                        self.deps.venue.name()
                    )));
                }
                cursor.scan_head.get_or_insert_with(|| newest.to_string());
            }
            let complete = page.len() < self.deps.venue.execution_history_page()
                || completed
                    .zip(oldest)
                    .is_some_and(|(completed, oldest)| oldest <= completed);
            if complete {
                if let Some(newest) = cursor.scan_head.take() {
                    if completed.is_none_or(|old| newest.parse::<u64>().is_ok_and(|new| new > old))
                    {
                        cursor.completed_bill = Some(newest);
                    }
                }
                cursor.scan_after = None;
            } else {
                cursor.scan_after = oldest.map(|id| id.to_string());
            }

            let account = account_id.to_string();
            let next = cursor.clone();
            let venue = self.deps.venue.clone();
            let progressed = self
                .rows(move |rows| {
                    let mut progressed = false;
                    rows.transaction(&mut || {
                        // The parent is stored before submission. If its venue ID has not arrived,
                        // do not move the cursor past a fill that may be its execution.
                        if !page.is_empty()
                            && rows.list_open_orders(&account)?.iter().any(|order| {
                                order.agent_id.is_some()
                                    && !is_leg(order)
                                    && order.exchange_order_id.as_deref().is_none_or(str::is_empty)
                            })
                        {
                            return Ok(());
                        }
                        let mut ids: Vec<_> = page
                            .iter()
                            .filter_map(|r| nullish(&r.exchange_order_id).map(str::to_owned))
                            .filter(|id| !id.is_empty())
                            .collect();
                        ids.sort_unstable();
                        ids.dedup();
                        let orders = rows.find_orders_by_exchange_ids(&account, &ids)?;
                        let by_identity: HashMap<_, _> = orders
                            .iter()
                            .filter(|order| order.agent_id.is_some() && !is_leg(order))
                            .filter_map(|order| {
                                Some((
                                    (
                                        order.exchange_order_id.as_deref()?,
                                        venue.instrument_id(&order.symbol),
                                    ),
                                    order,
                                ))
                            })
                            .collect();
                        // Reprocess the overlapping page: an order may now have an exchange ID
                        // that it lacked on the previous pass. The execution primary key dedupes it.
                        for fill in &page {
                            let Some(id) = strict(&fill.exchange_order_id) else {
                                continue;
                            };
                            let Some(inst) =
                                nullish(&fill.symbol).map(|id| venue.instrument_id(id))
                            else {
                                continue;
                            };
                            let Some(order) = by_identity.get(&(id, inst)) else {
                                continue;
                            };
                            let side = if order.side == OrderSide::Buy {
                                "buy"
                            } else {
                                "sell"
                            };
                            if strict(&fill.position_side) != Some("net")
                                || strict(&fill.side) != Some(side)
                            {
                                continue;
                            }
                            if let Some((trade_id, at)) = execution_of(fill) {
                                rows.record_exchange_execution(&order.id, &trade_id, at)?;
                            }
                        }
                        rows.save_execution_cursor(&account, &next)?;
                        progressed = true;
                        Ok(())
                    })?;
                    Ok(progressed)
                })
                .await?;
            if complete || !progressed {
                break;
            }
        }
        Ok(())
    }

    /// Reads pages older than `after` (from the newest when `None`) until OKX has no more, a page
    /// reaches `until`, or the budget runs out.
    async fn read_back(
        &self,
        mut after: Option<i64>,
        until: Option<i64>,
        budget: &mut usize,
        records: &mut Vec<ExchangePositionRecord>,
        account_id: &str,
    ) -> Result<Read, EngineError> {
        let venue = self.deps.venue.as_ref();
        loop {
            if *budget == 0 {
                return Ok(match after {
                    Some(after) => Read::Stopped(after),
                    None => Read::Met,
                });
            }
            *budget -= 1;
            let page = self.deps.venue.positions_history(after).await?;
            let mut oldest: Option<i64> = None;
            for row in &page {
                let record = record_of(venue, row, account_id)?;
                oldest = Some(oldest.map_or(record.closed_at, |o| o.min(record.closed_at)));
                records.push(record);
            }
            let Some(oldest) = oldest else {
                return Ok(Read::End);
            };
            if page.len() < self.deps.venue.positions_history_page() {
                return Ok(Read::End);
            }
            if until.is_some_and(|until| oldest <= until) {
                return Ok(Read::Met);
            }
            // `after` is exclusive. One past the oldest also rereads its millisecond, in case the
            // page split it, unless that would read the same page again.
            after = Some(if after == Some(oldest + 1) {
                oldest
            } else {
                oldest + 1
            });
        }
    }
}

/// How a read ended.
enum Read {
    /// OKX has nothing older.
    End,
    /// It reached what is already recorded.
    Met,
    /// The page budget ran out; older history is still to be read before this uTime.
    Stopped(i64),
}

fn execution_of(row: &VenueBill) -> Option<(String, i64)> {
    execution_from(strict(&row.trade_id)?, strict(&row.fill_ms)?)
}

/// [`execution_of`] for an order lookup.
pub(super) fn order_execution(order: &VenueOrder) -> Option<(String, i64)> {
    execution_from(strict(&order.trade_id)?, strict(&order.fill_ms)?)
}

fn execution_from(trade_id: &str, fill_time: &str) -> Option<(String, i64)> {
    let trade_id = trade_id.parse::<u64>().ok()?;
    let executed_at = fill_time.parse::<i64>().ok()?;
    (trade_id > 0 && executed_at > 0).then(|| (trade_id.to_string(), executed_at))
}

/// A whole number of milliseconds from a neutral time field.
fn venue_time(field: &Option<VenueField>) -> Option<i64> {
    whole_ms(venue::js_number(field))
}

fn whole_ms(value: f64) -> Option<i64> {
    (value.is_finite() && value > 0.0 && value.fract() == 0.0).then_some(value as i64)
}

fn isolated_net(row: &VenueClosedPosition) -> bool {
    strict(&row.margin_mode) == Some("isolated") && strict(&row.position_side) == Some("net")
}

/// One history row, kept whether or not its pair is in the registry.
fn record_of(
    venue: &dyn VenueAdapter,
    row: &VenueClosedPosition,
    account_id: &str,
) -> Result<ExchangePositionRecord, EngineError> {
    let missing = |field: &str| {
        EngineError::message(format!(
            "{} reported a closed position without {field}",
            venue.name()
        ))
    };
    let realized = truthy(&row.realized_pnl).ok_or_else(|| missing("realizedPnl"))?;
    Ok(ExchangePositionRecord {
        account_id: account_id.to_string(),
        pos_id: nullish(&row.position_id)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| missing("posId"))?,
        opened_at: venue_time(&row.created_ms).ok_or_else(|| missing("cTime"))?,
        inst_id: nullish(&row.symbol)
            .map(|id| venue.instrument_id(id))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| missing("instId"))?,
        closed_at: venue_time(&row.updated_ms).ok_or_else(|| missing("uTime"))?,
        realized_pnl: dec(realized)?.to_fixed(),
        close_type: nullish(&row.close_type).unwrap_or_default().to_owned(),
        opening_side: match (isolated_net(row), strict(&row.direction)) {
            (true, Some("long")) => Some("BUY".into()),
            (true, Some("short")) => Some("SELL".into()),
            _ => None,
        },
    })
}

/// An open OKX position's result so far. `None` for a pair missing from the registry.
pub(super) fn open_result_of(
    venue: &dyn VenueAdapter,
    row: &VenuePosition,
    registry: &Registry,
) -> Result<Option<OpenPositionResult>, EngineError> {
    let inst_id = nullish(&row.symbol)
        .map(|id| venue.instrument_id(id))
        .unwrap_or_default();
    let Some(spec) = registry.symbol_of_inst_id(venue, &inst_id) else {
        return Ok(None);
    };
    let amount = |field: &Option<VenueField>| -> Result<String, EngineError> {
        Ok(dec(truthy(field).unwrap_or("0"))?.to_fixed())
    };
    Ok(Some(OpenPositionResult {
        symbol: spec.symbol.clone(),
        pos_id: nullish(&row.position_id).unwrap_or_default().to_owned(),
        opened_at: venue_time(&row.created_ms),
        updated_at: venue_time(&row.updated_ms),
        opening_side: if super::sync::isolated_net(row) {
            let size = dec(&amount(&row.size)?)?;
            if size.is_zero() {
                None
            } else if size.is_negative() {
                Some("SELL".into())
            } else {
                Some("BUY".into())
            }
        } else {
            None
        },
        realized_pnl: amount(&row.realized_pnl).ok(),
        unrealized_pnl: amount(&row.unrealized_pnl).ok(),
    }))
}
