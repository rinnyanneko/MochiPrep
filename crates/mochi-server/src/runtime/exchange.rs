//! Exchange accounts (`runtime.ts` "exchange accounts"): connecting a key through every check,
//! disconnecting, arming and disarming, the arming rules agents follow, and the status read
//! model. The sync loop and the unlock reconnect are in `exchange_sync.rs`.
//!
//! Network work never runs on the runtime task: a connect checks and snapshots on the task,
//! talks to OKX off it, and comes back to write what it learned.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mochi_core::paper::PaperStore;
use mochi_domain::jsnum::{js_string_to_number, js_trim};
use mochi_domain::{
    default_account_config, default_live_limits, AccountVenue, InstrumentSpecification,
    PaperAccount, RiskSettings, Topic, VaultStatus,
};
use mochi_okx::engine::OkxEngine;
use mochi_okx::{Credentials, OkxClient, Venue};
use mochi_store::rows;
use mochi_vault::{exchange_context, seal};
use serde::Serialize;
use serde_json::Value;
use zeroize::Zeroizing;

use super::execution::{okx_engine, okx_venue, ExchangeSession, SessionLink};
use super::redact::redact_secrets;
use super::{internal_error, unix_ms, Runtime, RuntimeHandle};
use crate::error::ApiError;
use crate::validate::OkxConnectInput;

/// The words that arm a real-money account. Typed, never stored, never implied by unlocking.
pub const ARM_PHRASE: &str = "TRADE REAL MONEY";

/// What a connect, a reconnect or a sync can change in the web app.
const CONNECTED: &[Topic] = &[
    Topic::Accounts,
    Topic::Positions,
    Topic::Orders,
    Topic::Exchange,
    Topic::Arena,
];

/// `ExchangeStatus`, keys in Node's order.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeStatus {
    pub account_id: String,
    pub name: String,
    pub venue: AccountVenue,
    pub connected: bool,
    pub saved: bool,
    pub armed: bool,
    pub healthy: bool,
    pub last_sync_at: Option<i64>,
    pub error: Option<String>,
    pub limits: RiskSettings,
}

/// Node's `SavedOkxCredentials`, in its key order, as `JSON.stringify` writes it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedCredentials<'a> {
    api_key: &'a str,
    secret_key: &'a str,
    passphrase: &'a str,
    environment: AccountVenue,
}

/// The key, secret and passphrase, kept for redaction and wiped when dropped.
pub(super) type Secrets = [Zeroizing<String>; 3];

fn secrets_of(input: &OkxConnectInput) -> Secrets {
    [
        Zeroizing::new(input.api_key.clone()),
        Zeroizing::new(input.secret_key.clone()),
        Zeroizing::new(input.passphrase.clone()),
    ]
}

/// A config field as Node reads it (`config.x ?? ''` and `===` against a string).
fn config_text<'a>(config: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    config.get(key).and_then(Value::as_str)
}

/// The connect checks that ask OKX, in Node's order (steps 2-6). Returns the account's uid.
async fn check_key(
    client: &OkxClient,
    specs: &[InstrumentSpecification],
) -> Result<String, String> {
    let config = client.get_config().await.map_err(|e| e.to_string())?;
    let permissions: Vec<&str> = config_text(&config, "perm")
        .unwrap_or("")
        .split(',')
        .map(js_trim)
        .collect();
    if permissions.contains(&"withdraw") {
        return Err("This API key has withdrawal permission. Create a key without it.".into());
    }
    if !permissions.contains(&"trade") {
        return Err("This API key has no trade permission.".into());
    }
    if config_text(&config, "posMode") != Some("net_mode") {
        return Err(
            "Set OKX position mode to one-way (net) first. MochiPrep does not support hedge mode."
                .into(),
        );
    }
    if config_text(&config, "acctLv") == Some("1") {
        return Err("This OKX account is in Spot mode, which cannot trade perpetuals. Switch it to Futures mode.".into());
    }
    // Every registered pair, added ones included: sizing reads these values on every order.
    for spec in specs {
        let venue = client
            .get_contract_value(&spec.symbol)
            .await
            .map_err(|e| e.to_string())?;
        // `Number(a) !== Number(b)`: NaN never matches, so a missing field refuses too.
        if js_string_to_number(&venue.ct_val) != js_string_to_number(&spec.venue_contract_value)
            || js_string_to_number(&venue.lot_sz) != js_string_to_number(&spec.venue_lot_size)
        {
            return Err(format!(
                "OKX contract specs for {} changed. Refusing to size orders from stale values.",
                spec.symbol
            ));
        }
    }
    Ok(match config.get("uid") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(uid)) => uid.clone(),
        Some(other) => other.to_string(),
    })
}

impl RuntimeHandle {
    /// Node's `connectOkx`: verifies a key before anything can trade on it, then registers the
    /// session. `expected_account_id` is set when reconnecting saved credentials, which must
    /// still belong to that account. Runs as its own task, so a client that goes away cannot
    /// stop it between writing the account and registering its session.
    pub async fn connect_okx(
        &self,
        input: OkxConnectInput,
        expected_account_id: Option<String>,
    ) -> Result<PaperAccount, ApiError> {
        let handle = self.clone();
        tokio::spawn(async move { handle.connect_now(input, expected_account_id, None).await })
            .await
            .unwrap_or_else(|_| Err(internal_error()))
    }

    /// `epoch` is the session count the connect counts from (see `Runtime::session_seq`); `None`
    /// takes the current one in step 1. A reconnect passes the one it read its saved key under,
    /// so a disconnect, manual connect or forgotten key after that read is never undone.
    pub(super) async fn connect_now(
        &self,
        input: OkxConnectInput,
        expected_account_id: Option<String>,
        epoch: Option<u64>,
    ) -> Result<PaperAccount, ApiError> {
        let venue = okx_venue(input.environment)
            .ok_or_else(|| ApiError::bad_request("Not an exchange environment"))?;
        let remember = input.remember;
        // 1. Before any network call.
        let (options, live, specs, epoch, vault) = self
            .call(move |r| {
                if remember && r.vault_status()? != VaultStatus::Unlocked {
                    return Err(ApiError::bad_request(
                        "Set a master password and unlock it to save OKX keys",
                    ));
                }
                let epoch = epoch.unwrap_or(r.session_seq);
                Ok((
                    r.okx.clone(),
                    r.live(),
                    r.instruments().to_vec(),
                    epoch,
                    r.vault.generation,
                ))
            })
            .await??;
        let secrets = secrets_of(&input);
        let saved = Zeroizing::new(
            serde_json::to_string(&SavedCredentials {
                api_key: &input.api_key,
                secret_key: &input.secret_key,
                passphrase: &input.passphrase,
                environment: input.environment,
            })
            .map_err(|_| internal_error())?,
        );
        let credentials = Credentials::new(&*input.api_key, &*input.secret_key, &*input.passphrase);
        let client = options.client(credentials, venue);

        // 2-6. The checks that ask OKX.
        let uid = match check_key(&client, &specs).await {
            Ok(uid) => uid,
            Err(text) => return Err(self.refused(text, &secrets).await),
        };

        // 7. The account row. A new one is written before the first sync, as Node writes it; an
        // existing one keeps its limits until registration, so a connect refused there leaves
        // them as they were.
        let (name, environment, limits) =
            (input.name.clone(), input.environment, input.limits.clone());
        let reconnect = expected_account_id.is_some();
        let account = self
            .call(move |r| {
                let account = r.okx_account(name, environment, uid, limits, expected_account_id)?;
                // Checked before the first sync too, so a connect that will be refused does not
                // reconcile an account the owner has just disconnected or connected anew.
                r.registration_refusal(&account, reconnect, epoch)?;
                r.restore_retired_order(&account.id)
                    .map_err(|error| r.api_error(error))?;
                Ok::<_, ApiError>(account)
            })
            .await??;

        // 8-9. A verified position snapshot can support reduce-only exits even if protection
        // reconciliation failed. The engine remains unhealthy until a full sync succeeds.
        let armed = Arc::new(AtomicBool::new(false));
        let link = SessionLink::new(&account.id, epoch);
        let engine = okx_engine(
            self.downgrade(),
            &account.id,
            client,
            &options,
            live,
            armed.clone(),
            link.clone(),
        );
        let sync = engine
            .sync_before_registration()
            .await
            .map_err(|error| error.to_string());
        if let Err(error) = &sync {
            if engine.last_sync().is_none() {
                // A sync whose writes were refused because the account changed meanwhile says why.
                let changed = account.clone();
                let refusal = self
                    .call(move |r| r.registration_refusal(&changed, reconnect, epoch).err())
                    .await?;
                return Err(match refusal {
                    Some(refusal) => refusal,
                    None => self.refused(error.clone(), &secrets).await,
                });
            }
        }
        let session = ExchangeSession {
            was_healthy: true,
            engine: engine.clone(),
            armed,
            secrets,
            link,
        };
        // 10-12. Register, save or forget the key, and take the first balance.
        let remember = remember.then_some(vault);
        self.call(move |r| {
            let account_id = account.id.clone();
            let registered =
                r.register_session(account, session, remember, &saved, reconnect, epoch);
            if sync.is_err() {
                r.after_sync(&account_id, &engine, sync);
            }
            registered
        })
        .await?
    }

    /// A refusal, as Node's error handler sends it: 400, the key's own values removed too.
    async fn refused(&self, text: String, secrets: &Secrets) -> ApiError {
        let text = redact_secrets(&text, secrets.iter().map(|s| s.as_str()));
        match self.call(move |r| r.api_error(text)).await {
            Ok(error) | Err(error) => error,
        }
    }
}

impl Runtime {
    /// Step 7: the account this key belongs to, by venue and OKX uid, or a new one. An existing
    /// account's limits are written at registration.
    fn okx_account(
        &mut self,
        name: String,
        environment: AccountVenue,
        uid: String,
        limits: RiskSettings,
        expected_account_id: Option<String>,
    ) -> Result<PaperAccount, ApiError> {
        let store = self.engine().store();
        let existing = store
            .list_accounts()
            .map_err(|e| self.api_error(e))?
            .into_iter()
            .find(|a| a.venue == environment && a.exchange_uid.as_deref() == Some(uid.as_str()));
        if let Some(expected) = &expected_account_id {
            if existing.as_ref().map(|a| &a.id) != Some(expected) {
                return Err(ApiError::bad_request(
                    "The saved API key now belongs to a different OKX account. Reconnect it by hand.",
                ));
            }
        }
        match existing {
            Some(mut account) => {
                // The name stays: only the limits come from the connect.
                account.live_limits = Some(limits);
                Ok(account)
            }
            None => {
                let zero = || "0".to_string();
                let account = PaperAccount {
                    id: format!("acct_{}", uuid::Uuid::new_v4()),
                    name,
                    source: mochi_domain::DataSource::Live,
                    venue: environment,
                    exchange_uid: Some(uid),
                    live_limits: Some(limits),
                    is_demo: false,
                    initial_balance: zero(),
                    wallet_balance: zero(),
                    used_margin: zero(),
                    realized_pnl: zero(),
                    fees_paid: zero(),
                    funding_paid: zero(),
                    config: default_account_config(),
                    created_at: unix_ms(),
                };
                store
                    .insert_account(&account)
                    .map_err(|e| self.api_error(e))?;
                Ok(account)
            }
        }
    }

    /// Whether `account_id`'s session or saved key changed (a session registered, a disconnect,
    /// a key forgotten) after a connect counting from `epoch` started.
    pub(super) fn session_changed_since(&self, account_id: &str, epoch: u64) -> bool {
        self.session_changed_at
            .get(account_id)
            .is_some_and(|&at| at > epoch)
    }

    /// Notes a change to `account_id`'s session or saved key, so a connect still out on OKX
    /// cannot undo it.
    pub(super) fn bump_session(&mut self, account_id: &str) {
        self.session_seq += 1;
        self.session_changed_at
            .insert(account_id.to_string(), self.session_seq);
    }

    /// Why a connect counting from `epoch` may not register: its account changed meanwhile, or,
    /// for an unlock reconnect, the account is already connected (by hand, so the owner's session
    /// and arming stand).
    fn registration_refusal(
        &self,
        account: &PaperAccount,
        reconnect: bool,
        epoch: u64,
    ) -> Result<(), ApiError> {
        if self.session_changed_since(&account.id, epoch) {
            return Err(ApiError::bad_request(format!(
                "{} was connected, disconnected or had its saved key changed while connecting to OKX. Connect it again if you want it.",
                account.name
            )));
        }
        if reconnect && self.exchanges.contains_key(&account.id) {
            return Err(ApiError::bad_request(format!(
                "{} is already connected.",
                account.name
            )));
        }
        Ok(())
    }

    /// Steps 10-12, after the first position and balance snapshot. A manual session replaces the old one,
    /// which resets arming to the venue's default. A connect whose account changed while it was
    /// out (see [`Self::registration_refusal`]) registers nothing and saves nothing. `remember`
    /// is the vault generation the owner asked to save the key under.
    fn register_session(
        &mut self,
        mut account: PaperAccount,
        session: ExchangeSession,
        remember: Option<u64>,
        saved: &str,
        reconnect: bool,
        epoch: u64,
    ) -> Result<PaperAccount, ApiError> {
        if let Err(error) = self.registration_refusal(&account, reconnect, epoch) {
            let _ = session.engine.retire();
            return Err(error);
        }
        self.bump_session(&account.id);
        if let Err(error) = self.retire_exchange_session(&account.id) {
            let _ = session.engine.retire();
            self.pause_after_failed_replacement(&account.id);
            self.invalidate(CONNECTED);
            return Err(self.api_error(error));
        }
        // The limits this connect brought, written only once the old session's orders are
        // finalized, so a failed replacement leaves the previous limits in place. The old
        // session is already gone here, so a failure still publishes the disconnect.
        if let Err(error) = self.engine().store().save_account_profile(&account) {
            let _ = session.engine.retire();
            self.pause_after_failed_replacement(&account.id);
            self.invalidate(CONNECTED);
            return Err(self.api_error(error));
        }
        session.link.registered.store(true, Ordering::SeqCst);
        let engine = session.engine.clone();
        session
            .armed
            .store(engine.venue() == Venue::OkxDemo, Ordering::SeqCst);
        self.exchanges.insert(account.id.clone(), session);
        let now = unix_ms();
        if let Some(vault) = remember {
            // Checked again: the vault may have been turned off or reset while OKX was
            // answering, and perhaps set up anew, with a password the box was not ticked for.
            let data_key = self.data_key().filter(|_| self.vault.generation == vault);
            let Some(data_key) = data_key else {
                return Err(ApiError::bad_request(
                    "Connected, but the key was not saved: the master password was removed meanwhile",
                ));
            };
            let sealed = seal(data_key, saved, &exchange_context(&account.id));
            mochi_store::save_sealed_exchange_credentials(
                self.connection(),
                &account.id,
                &sealed,
                now,
            )
            .map_err(|e| self.api_error(e))?;
        } else if self.data_key().is_some() {
            // While locked the save box cannot be ticked, so an unticked box is not a choice to
            // forget the key.
            mochi_store::delete_sealed_exchange_credentials(self.connection(), &account.id)
                .map_err(|e| self.api_error(e))?;
        }
        if account.initial_balance == "0" {
            account.initial_balance = engine.balance().map_err(|e| self.api_error(e))?.equity;
            self.engine()
                .store()
                .save_account_profile(&account)
                .map_err(|e| self.api_error(e))?;
        }
        self.invalidate(CONNECTED);
        Ok(account)
    }

    /// The old session is gone and no new one registered, so the account is disconnected.
    /// Its agents stop as on a disconnect; a later reconnect never restarts them unasked.
    /// A pause that fails stays owed, which keeps them unscheduled until it succeeds.
    fn pause_after_failed_replacement(&mut self, account_id: &str) {
        match self.pause_account_agents(account_id, "OKX disconnected") {
            Ok(()) => {
                self.pause_owed.remove(account_id);
            }
            Err(error) => {
                tracing::warn!(error = %error.message(), "could not pause agents after a failed reconnect");
                self.pause_owed.insert(account_id.to_string());
            }
        }
    }

    /// Node's `disconnectExchange`. Also forgets saved credentials, so a disconnected account
    /// does not return on the next unlock.
    pub fn disconnect_exchange(&mut self, account_id: &str) -> Result<(), ApiError> {
        // First, before anything fallible: a connect still out must not undo this.
        self.bump_session(account_id);
        let finalized = self.retire_exchange_session(account_id);
        mochi_store::delete_sealed_exchange_credentials(self.connection(), account_id)
            .map_err(|e| self.api_error(e))?;
        self.pause_account_agents(account_id, "OKX disconnected")?;
        // Every running agent is paused now, so no lost-sync pause is owed any more.
        self.pause_owed.remove(account_id);
        self.invalidate(&[Topic::Accounts, Topic::Orders, Topic::Exchange]);
        finalized.map_err(|error| self.api_error(error))
    }

    /// Node's `arm`, its checks in Node's order.
    pub fn arm(&mut self, account_id: &str, confirmation: &str) -> Result<(), ApiError> {
        let Some(session) = self.exchanges.get(account_id) else {
            return Err(ApiError::bad_request("Connect this account first"));
        };
        if confirmation != ARM_PHRASE {
            return Err(ApiError::bad_request(format!("Type exactly: {ARM_PHRASE}")));
        }
        if !session.engine.healthy() {
            return Err(ApiError::bad_request(
                "Cannot arm until sync with OKX succeeds",
            ));
        }
        session.armed.store(true, Ordering::SeqCst);
        self.invalidate(&[Topic::Exchange]);
        Ok(())
    }

    /// Node's `disarm`: real accounts only, since demo funds need no safety catch. Exits stay
    /// possible when disarmed.
    pub fn disarm(&mut self, account_id: Option<&str>) {
        for (id, session) in &self.exchanges {
            if account_id.is_some_and(|wanted| wanted != id) {
                continue;
            }
            // The venue is fixed for the session's life, so this needs no store read: a read that
            // failed would leave real money armed.
            if session.engine.venue() == Venue::OkxLive {
                session.armed.store(false, Ordering::SeqCst);
            }
        }
        self.invalidate(&[Topic::Exchange]);
    }

    pub(super) fn is_armed(&self, account_id: &str) -> bool {
        self.exchanges
            .get(account_id)
            .is_some_and(|session| session.armed.load(Ordering::SeqCst))
    }

    /// Node's `assertCanRun`: an agent may only be scheduled on an exchange account that could
    /// accept its orders.
    pub fn assert_can_run(&self, agent: &rows::Agent) -> Result<(), ApiError> {
        let account = self
            .engine()
            .store()
            .get_account(&agent.account_id)
            .map_err(|e| self.api_error(e))?;
        let Some(account) = account.filter(|a| a.venue != AccountVenue::Paper) else {
            return Ok(());
        };
        let Some(session) = self.exchanges.get(&account.id) else {
            return Err(ApiError::bad_request(format!(
                "{} is not connected to OKX",
                account.name
            )));
        };
        if !session.engine.healthy() {
            return Err(ApiError::bad_request(format!(
                "{} must sync with OKX before its agents can start",
                account.name
            )));
        }
        // A suggestion places nothing, so analysis does not need real orders switched on.
        if agent.execution_mode == "AUTO" && !self.is_armed(&account.id) {
            return Err(ApiError::bad_request(format!(
                "{} is not armed. Arm it in Settings first.",
                account.name
            )));
        }
        Ok(())
    }

    /// Node's `assertCanExecuteSignal`, checked before a suggestion is used: an order refused
    /// later would burn its idempotency key. Exits need no arming, and neither does PROTECT:
    /// the OKX protection gate refuses, while disarmed, any change that adds risk.
    pub fn assert_can_execute_signal(&self, signal: &Value) -> Result<(), ApiError> {
        let account_id = signal["accountId"].as_str().unwrap_or_default();
        let account = self
            .engine()
            .store()
            .get_account(account_id)
            .map_err(|e| self.api_error(e))?;
        let agent = rows::get_agent(self.connection(), signal["agentId"].as_str().unwrap_or(""))
            .map_err(|e| self.api_error(e))?;
        // The runner expires mismatched suggestions before checking the execution venue.
        let Some(account) = account else {
            return Ok(());
        };
        if agent.map(|a| a.account_id).as_deref() != Some(account_id)
            || serde_json::to_value(account.source).ok().as_ref() != Some(&signal["source"])
        {
            return Ok(());
        }
        let action = signal["decision"]["action"].as_str();
        if account.venue != AccountVenue::OkxLive || matches!(action, Some("CLOSE" | "PROTECT")) {
            return Ok(());
        }
        if !self.is_armed(&account.id) {
            return Err(ApiError::bad_request(format!(
                "{} is not armed. Arm it in Settings first.",
                account.name
            )));
        }
        Ok(())
    }

    /// Node's `exchangeStatuses()`: every exchange account, connected or not.
    pub fn exchange_statuses(&self) -> Result<Vec<ExchangeStatus>, ApiError> {
        let accounts = self
            .engine()
            .store()
            .list_accounts()
            .map_err(|e| self.api_error(e))?;
        let mut statuses = Vec::new();
        for account in accounts {
            if account.venue == AccountVenue::Paper {
                continue;
            }
            let saved = rows::has_sealed_exchange_credentials(self.connection(), &account.id)
                .map_err(|e| self.api_error(e))?;
            let session = self.exchanges.get(&account.id);
            statuses.push(ExchangeStatus {
                account_id: account.id.clone(),
                name: account.name,
                venue: account.venue,
                connected: session.is_some(),
                saved,
                armed: self.is_armed(&account.id),
                healthy: session.is_some_and(|s| s.engine.healthy()),
                last_sync_at: session.and_then(|s| s.engine.last_sync()),
                error: session
                    .and_then(|s| s.engine.error())
                    .map(|error| self.redact(&error)),
                limits: account.live_limits.unwrap_or_else(default_live_limits),
            });
        }
        Ok(statuses)
    }

    /// Retry local finalization on the sync cadence even for disconnected accounts.
    pub(super) fn prepare_exchange_sync(&mut self) -> Vec<(String, Arc<OkxEngine>)> {
        let pending: Vec<_> = self.retired_unsent_orders.keys().cloned().collect();
        for id in &pending {
            let _ = self.restore_retired_order(id);
        }
        self.exchanges
            .iter()
            .map(|(id, session)| (id.clone(), session.engine.clone()))
            .collect()
    }
}
