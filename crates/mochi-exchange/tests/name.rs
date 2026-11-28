//! The engine's texts name the venue it trades through, so a second exchange never reads "OKX".

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mochi_core::paper::PaperStore;
use mochi_domain::{
    default_account_config, AccountVenue, DataSource, EntryOrderType, InstrumentSpecification,
    OrderRequest, OrderSide, OrderStatus, PaperAccount,
};
use mochi_exchange::engine::{EngineDeps, ExchangeEngine, Rows, StoreAccess, StoreWork};
use mochi_exchange::venue::{
    AlgoKind, BoxFuture, VenueAccountConfig, VenueAdapter, VenueAlgo, VenueBalance, VenueBill,
    VenueCancelAlgo, VenueClosedPosition, VenueContract, VenueError, VenueFill, VenueOrder,
    VenuePlaceAlgo, VenuePlaceOrder, VenuePosition,
};
use mochi_store::{open_in_memory, SqlitePaperStore};

const ACCOUNT: &str = "acct_testx";

/// An exchange that refuses every call, so nothing can ever sync.
struct Stub;

fn refused<T: Send + 'static>() -> BoxFuture<'static, Result<T, VenueError>> {
    Box::pin(async { Err(VenueError::Invalid("stub".into())) })
}

impl VenueAdapter for Stub {
    fn name(&self) -> &str {
        "TESTX"
    }
    fn account_config(&self) -> BoxFuture<'_, Result<VenueAccountConfig, VenueError>> {
        refused()
    }
    fn contract(&self, _: &str) -> BoxFuture<'_, Result<VenueContract, VenueError>> {
        refused()
    }
    fn balance(&self) -> BoxFuture<'_, Result<Option<VenueBalance>, VenueError>> {
        refused()
    }
    fn positions(&self) -> BoxFuture<'_, Result<Vec<VenuePosition>, VenueError>> {
        refused()
    }
    fn positions_history(
        &self,
        _: Option<i64>,
    ) -> BoxFuture<'_, Result<Vec<VenueClosedPosition>, VenueError>> {
        refused()
    }
    fn execution_history(
        &self,
        _: Option<u64>,
    ) -> BoxFuture<'_, Result<Vec<VenueBill>, VenueError>> {
        refused()
    }
    fn recent_fills(&self, _: &str) -> BoxFuture<'_, Result<Vec<VenueFill>, VenueError>> {
        refused()
    }
    fn set_leverage(&self, _: &str, _: u32) -> BoxFuture<'_, Result<(), VenueError>> {
        refused()
    }
    fn place_order(&self, _: VenuePlaceOrder) -> BoxFuture<'_, Result<String, VenueError>> {
        refused()
    }
    fn get_order(&self, _: &str, _: &str) -> BoxFuture<'_, Result<Option<VenueOrder>, VenueError>> {
        refused()
    }
    fn cancel_order(&self, _: &str, _: &str) -> BoxFuture<'_, Result<(), VenueError>> {
        refused()
    }
    fn place_algo(&self, _: VenuePlaceAlgo) -> BoxFuture<'_, Result<String, VenueError>> {
        refused()
    }
    fn pending_algos(&self, _: AlgoKind) -> BoxFuture<'_, Result<Vec<VenueAlgo>, VenueError>> {
        refused()
    }
    fn get_algo(&self, _: &str) -> BoxFuture<'_, Result<Option<VenueAlgo>, VenueError>> {
        refused()
    }
    fn cancel_algos(&self, _: Vec<VenueCancelAlgo>) -> BoxFuture<'_, Result<(), VenueError>> {
        refused()
    }
    fn client_order_id(&self, order_id: &str) -> String {
        order_id.to_string()
    }
    fn client_algo_id(&self, order_id: &str) -> String {
        order_id.to_string()
    }
    fn instrument_id(&self, symbol: &str) -> String {
        symbol.to_string()
    }
    fn recent_fills_page(&self) -> usize {
        100
    }
    fn execution_history_page(&self) -> usize {
        100
    }
    fn positions_history_page(&self) -> usize {
        100
    }
}

/// The store as the server's actor runs it: every piece of work alone, to completion.
struct Local(Mutex<(SqlitePaperStore, Vec<InstrumentSpecification>)>);

impl StoreAccess for Local {
    fn run(&self, work: StoreWork) -> BoxFuture<'static, ()> {
        {
            let inner = self.0.lock().unwrap();
            work(&Rows {
                store: &inner.0,
                instruments: &inner.1,
            });
        }
        Box::pin(async {})
    }
}

fn account() -> PaperAccount {
    PaperAccount {
        id: ACCOUNT.into(),
        name: "stub account".into(),
        source: DataSource::Live,
        venue: AccountVenue::okx_live(),
        exchange_uid: Some("1".into()),
        live_limits: None,
        is_demo: false,
        initial_balance: "1000".into(),
        wallet_balance: "0".into(),
        used_margin: "0".into(),
        realized_pnl: "0".into(),
        fees_paid: "0".into(),
        funding_paid: "0".into(),
        config: default_account_config(),
        created_at: 0,
    }
}

#[tokio::test]
async fn engine_texts_use_venue_name() {
    let store = SqlitePaperStore::new(open_in_memory().unwrap());
    store.insert_account(&account()).unwrap();
    let ids = AtomicU64::new(0);
    let engine = ExchangeEngine::new(EngineDeps {
        account_id: ACCOUNT.into(),
        venue: Arc::new(Stub),
        store: Arc::new(Local(Mutex::new((
            store,
            mochi_domain::default_instruments(),
        )))),
        get_quote: Arc::new(|_| None),
        now: Arc::new(|| 1_800_000_000_000),
        is_armed: Arc::new(|| true),
        new_id: Arc::new(move |prefix| {
            format!("{prefix}_{}", ids.fetch_add(1, Ordering::SeqCst) + 1)
        }),
    });

    let result = engine
        .place_order(OrderRequest {
            account_id: ACCOUNT.into(),
            client_order_id: "key-1".into(),
            symbol: "BTCUSDT".into(),
            side: OrderSide::Buy,
            order_type: EntryOrderType::Market,
            quantity: "0.001".into(),
            price: None,
            leverage: 3,
            reduce_only: None,
            stop_loss: None,
            take_profit: None,
            trailing_stop: None,
            agent_id: None,
            decision_id: None,
        })
        .await
        .unwrap();

    assert_eq!(result.order.status, OrderStatus::Rejected);
    assert_eq!(
        result.order.reject_reason.as_deref(),
        Some("EXCHANGE_NOT_SYNCED: no recent sync with TESTX")
    );
}
