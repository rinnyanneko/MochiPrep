//! Keeping exchange accounts in step with OKX: the sync every 5 s inside the live tick, which
//! pauses agents when sync is lost, and unlock's reconnect of saved keys.

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use mochi_core::paper::PaperStore;
use mochi_domain::{default_live_limits, ServerEvent, ToastLevel, Topic};
use mochi_okx::engine::OkxEngine;
use mochi_vault::{exchange_context, open, DataKey};
use serde::Deserialize;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::execution::WeakRuntimeHandle;
use super::{Command, Runtime, RuntimeHandle};
use crate::error::ApiError;
use crate::validate::OkxConnectInput;
use mochi_domain::AccountVenue;

/// Node's `EXCHANGE_SYNC_MS`.
pub(super) const EXCHANGE_SYNC: Duration = Duration::from_secs(5);

/// What a sync pass changes in the web app, after each account.
const SYNCED: &[Topic] = &[
    Topic::Accounts,
    Topic::Positions,
    Topic::Orders,
    Topic::Exchange,
    Topic::Arena,
];

/// Node's `SavedOkxCredentials`, read back. Wiped when dropped.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(rename_all = "camelCase")]
struct Saved {
    api_key: String,
    secret_key: String,
    passphrase: String,
    #[zeroize(skip)]
    environment: AccountVenue,
}

/// Node's `syncExchanges`: one full sync per session, one after another. The work reaches the
/// runtime only through short calls, and holds no strong handle.
async fn sync_all(runtime: WeakRuntimeHandle, sessions: Vec<(String, Arc<OkxEngine>)>) {
    for (id, engine) in sessions {
        let result = engine.sync().await.map_err(|error| error.to_string());
        let Some(handle) = runtime.upgrade() else {
            return;
        };
        let _ = handle
            .call(move |r| r.after_sync(&id, &engine, result))
            .await;
    }
}

impl RuntimeHandle {
    /// One sync pass now, as the live tick runs it every 5 s.
    pub async fn sync_exchanges(&self) -> Result<(), ApiError> {
        let sessions = self.call(|r| r.session_engines()).await?;
        sync_all(self.downgrade(), sessions).await;
        Ok(())
    }

    /// Node's unlock reconnect: each saved key, one after another, through every connect check.
    /// It never arms: a reconnected real account waits for the typed phrase. Returns the names of
    /// accounts that could not reconnect.
    pub(super) async fn reconnect_saved_okx(
        &self,
        data_key: &DataKey,
    ) -> Result<Vec<String>, ApiError> {
        let saved = self
            .call(|r| {
                mochi_store::list_sealed_exchange_credentials(r.connection())
                    .map_err(|e| r.api_error(e))
            })
            .await??;
        let mut failed = Vec::new();
        for (account_id, sealed) in saved {
            let id = account_id.clone();
            let account = self
                .call(move |r| {
                    let account = r.engine().store().get_account(&id).ok().flatten();
                    account.filter(|_| r.exchange_session(&id).is_none())
                })
                .await?;
            // Gone, or already connected by hand: nothing to reconnect.
            let Some(account) = account else {
                continue;
            };
            let result = match read_saved(data_key, &sealed, &account_id) {
                Ok(saved) => {
                    let input = OkxConnectInput {
                        api_key: saved.api_key.clone(),
                        secret_key: saved.secret_key.clone(),
                        passphrase: saved.passphrase.clone(),
                        name: account.name.clone(),
                        environment: saved.environment,
                        limits: account
                            .live_limits
                            .clone()
                            .unwrap_or_else(default_live_limits),
                        remember: true,
                    };
                    self.connect_now(input, Some(account_id)).await.map(|_| ())
                }
                Err(text) => Err(ApiError::bad_request(text)),
            };
            if let Err(error) = result {
                let name = account.name.clone();
                let text = error.message().to_string();
                self.call(move |r| {
                    let message =
                        format!("{name}: could not reconnect to OKX. {}", r.redact(&text));
                    r.emit(ServerEvent::Toast {
                        level: ToastLevel::Error,
                        message,
                    });
                })
                .await?;
                failed.push(account.name);
            }
        }
        Ok(failed)
    }
}

/// Opens a saved key. Any failure fails that account only.
fn read_saved(data_key: &DataKey, sealed: &str, account_id: &str) -> Result<Saved, String> {
    let text = Zeroizing::new(
        open(data_key, sealed, &exchange_context(account_id)).map_err(|e| e.to_string())?,
    );
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

impl Runtime {
    /// Records one account's sync. Only the change from healthy to unhealthy pauses its agents
    /// and says so; agents are never un-paused automatically. A session replaced or removed
    /// while it synced is not the one this result belongs to.
    pub(super) fn after_sync(
        &mut self,
        account_id: &str,
        engine: &Arc<OkxEngine>,
        result: Result<(), String>,
    ) {
        let lost = match self.exchanges.get_mut(account_id) {
            Some(session) if Arc::ptr_eq(&session.engine, engine) => match result {
                Ok(()) => {
                    session.was_healthy = true;
                    None
                }
                Err(text) if session.was_healthy => {
                    session.was_healthy = false;
                    Some(text)
                }
                Err(_) => None,
            },
            _ => None,
        };
        if let Some(text) = lost {
            let name = self
                .engine()
                .store()
                .get_account(account_id)
                .ok()
                .flatten()
                .map(|a| a.name)
                .unwrap_or_else(|| "undefined".to_string());
            if let Err(error) = self.pause_account_agents(account_id, "Lost sync with OKX") {
                tracing::warn!(error = %error.message(), "could not pause agents after a lost sync");
            }
            let message = format!(
                "{name}: lost sync with OKX. Agents paused. {}",
                self.redact(&text)
            );
            self.emit(ServerEvent::Toast {
                level: ToastLevel::Error,
                message,
            });
        }
        self.invalidate(SYNCED);
    }

    /// Node's `liveTick`: every 5 s it syncs every exchange account first, then runs the agents.
    /// While a sync pass is out, later ticks are skipped, as Node's `liveTicking` guard does.
    pub(super) fn live_tick(&mut self) {
        if self.live_ticking {
            return;
        }
        let now = tokio::time::Instant::now();
        let due = self
            .last_exchange_sync_at
            .is_none_or(|at| now - at >= EXCHANGE_SYNC);
        if due {
            self.last_exchange_sync_at = Some(now);
            let sessions = self.session_engines();
            if let (false, Some(runtime)) = (sessions.is_empty(), self.handle.clone()) {
                self.live_ticking = true;
                self.tasks.push(Box::pin(async move {
                    let pass = std::panic::AssertUnwindSafe(sync_all(runtime, sessions));
                    let _ = pass.catch_unwind().await;
                    Box::new(|r: &mut Runtime| {
                        r.live_ticking = false;
                        r.after_live_sync();
                    }) as Command
                }));
                return;
            }
        }
        self.after_live_sync();
    }
}
