//! The master password (`runtime.ts` "master password" section): turning it on, unlocking,
//! changing, turning off and resetting it, with Node's check orders and texts.
//!
//! scrypt takes hundreds of ms at the production cost, so no action derives on the runtime task:
//! the task checks and snapshots what the action needs, the derivation runs in
//! `spawn_blocking`, and the task applies the result. A busy flag, Node's `exclusive`, is held
//! for the whole action so two of them never interleave.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mochi_core::paper::{PaperStore, StoreFailure, StoreFailureKind};
use mochi_domain::{ServerEvent, ToastLevel, Topic, VaultStatus};
use mochi_store::{has_vault, rows, StoreError};
use mochi_vault::{
    new_data_key, open, provider_context, seal, unwrap_with_password, wrap_with_password, DataKey,
    KdfParams, PasswordWrap, VaultError,
};
use serde::Serialize;
use zeroize::Zeroizing;

use super::{internal_error, unix_ms, Runtime, RuntimeHandle};
use crate::error::ApiError;

pub const VAULT_RESET_PHRASE: &str = "DELETE SAVED KEYS";
/// The only `vault_keys` row Node writes.
const PASSWORD_WRAP_ID: &str = "password";
const PASSWORD_KIND: &str = "PASSWORD";
const BUSY: &str = "Another master password action is in progress";
/// A reader kept the WAL from being truncated; SQLite checkpoints it again when it closes.
const SCRUB_BLOCKED: &str =
    "Deleted key data may stay in the database file until MochiPrep closes.";
/// rusqlite's default, which `Connection::open` sets and the scrub briefly lifts.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// What the runtime holds for the master password. The data key lives only here, in memory.
pub(super) struct VaultState {
    pub(super) data_key: Option<DataKey>,
    busy: Arc<AtomicBool>,
    kdf: KdfParams,
}

impl VaultState {
    pub(super) fn new(kdf: KdfParams) -> Self {
        VaultState {
            data_key: None,
            busy: Arc::default(),
            kdf,
        }
    }
}

/// The busy flag, taken for one action. Dropping it frees the flag, so every way an action can
/// end (success, a refusal, a failed derivation, a panic) releases it.
pub(super) struct Exclusive(Arc<AtomicBool>);

impl Drop for Exclusive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// `POST /api/vault/unlock`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnlockOutcome {
    pub status: VaultStatus,
    /// Names of saved OKX accounts that could not reconnect.
    pub failed: Vec<String>,
}

/// Node's error handler for a vault failure: a wrong password is 403, since the browser reads
/// 401 as an expired session token; anything else is a 400.
fn vault_error(error: VaultError) -> ApiError {
    match error {
        VaultError::WrongPassword => ApiError::forbidden(error.to_string()),
        _ => ApiError::bad_request(error.to_string()),
    }
}

fn store_failure(error: StoreError) -> StoreFailure {
    StoreFailure::new(StoreFailureKind::Other, error.to_string())
}

/// Runs the derivation on the blocking pool. A panic there is a 500, and the caller's
/// `Exclusive` is dropped with the error.
async fn off_actor<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, VaultError> + Send + 'static,
) -> Result<T, ApiError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result.map_err(vault_error),
        Err(_) => Err(internal_error()),
    }
}

/// Runs a whole action as its own task, so a client that disconnects mid-way cannot cancel it
/// between the derivation and applying the result.
async fn detached<T: Send + 'static>(
    action: impl std::future::Future<Output = Result<T, ApiError>> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::spawn(action)
        .await
        .unwrap_or_else(|_| Err(internal_error()))
}

impl RuntimeHandle {
    /// Node's `enableVault`: seals the AI keys already saved and removes their plaintext, in one
    /// transaction, then scrubs the WAL.
    pub async fn enable_vault(&self, password: String) -> Result<VaultStatus, ApiError> {
        let password = Zeroizing::new(password);
        let handle = self.clone();
        detached(async move {
            let (guard, kdf) = handle
                .call(|r| {
                    let guard = r.exclusive()?;
                    if r.vault_status()? != VaultStatus::Off {
                        return Err(ApiError::bad_request("A master password is already set"));
                    }
                    Ok((guard, r.vault.kdf))
                })
                .await??;
            let (data_key, wrap) = off_actor(move || {
                let data_key = new_data_key();
                let wrap = wrap_with_password(&data_key, &password, kdf)?;
                Ok((data_key, wrap))
            })
            .await?;
            let status = handle
                .call(move |r| r.apply_enable(data_key, &wrap))
                .await??;
            drop(guard);
            Ok(status)
        })
        .await
    }

    /// Node's `unlock`: loads every sealed AI key, then reconnects saved OKX accounts.
    pub async fn unlock(&self, password: String) -> Result<UnlockOutcome, ApiError> {
        let password = Zeroizing::new(password);
        let handle = self.clone();
        detached(async move {
            let (guard, wrap) = handle
                .call(|r| {
                    let guard = r.exclusive()?;
                    if r.vault_status()? != VaultStatus::Locked {
                        return Err(ApiError::bad_request("MochiPrep is not locked"));
                    }
                    Ok((guard, r.password_wrap()?))
                })
                .await??;
            let data_key = off_actor(move || unwrap_with_password(&wrap, &password)).await?;
            let loaded = data_key.clone();
            handle.call(move |r| r.apply_unlock(loaded)).await??;
            // Reconnecting runs every check a manual connect does, one account at a time.
            let failed = handle.reconnect_saved_okx(&data_key).await?;
            let status = handle.call(|r| r.vault_status()).await??;
            drop(guard);
            Ok(UnlockOutcome { status, failed })
        })
        .await
    }

    /// Node's `changePassword`: rewraps the same data key, so nothing sealed changes. Works
    /// locked or unlocked.
    pub async fn change_password(
        &self,
        current: String,
        next: String,
    ) -> Result<VaultStatus, ApiError> {
        let (current, next) = (Zeroizing::new(current), Zeroizing::new(next));
        let handle = self.clone();
        detached(async move {
            let (guard, wrap, kdf) = handle
                .call(|r| {
                    let guard = r.exclusive()?;
                    Ok((guard, r.password_wrap()?, r.vault.kdf))
                })
                .await??;
            let rewrapped = off_actor(move || {
                let data_key = unwrap_with_password(&wrap, &current)?;
                wrap_with_password(&data_key, &next, kdf)
            })
            .await?;
            let status = handle
                .call(move |r| {
                    rows_result(r, |conn| {
                        mochi_store::save_vault_key(
                            conn,
                            PASSWORD_WRAP_ID,
                            PASSWORD_KIND,
                            &rewrapped,
                            unix_ms(),
                        )
                    })?;
                    r.vault_status()
                })
                .await??;
            drop(guard);
            Ok(status)
        })
        .await
    }

    /// Node's `disableVault`: back to unencrypted AI keys. Saved OKX credentials are deleted,
    /// since they never exist in plaintext. Works locked or unlocked.
    pub async fn disable_vault(&self, password: String) -> Result<VaultStatus, ApiError> {
        let password = Zeroizing::new(password);
        let handle = self.clone();
        detached(async move {
            let (guard, wrap) = handle
                .call(|r| {
                    let guard = r.exclusive()?;
                    Ok((guard, r.password_wrap()?))
                })
                .await??;
            let data_key = off_actor(move || unwrap_with_password(&wrap, &password)).await?;
            let status = handle.call(move |r| r.apply_disable(&data_key)).await??;
            drop(guard);
            Ok(status)
        })
        .await
    }

    /// Node's `resetVault`, for a lost password. Synchronous: it derives nothing.
    pub async fn reset_vault(&self, confirmation: String) -> Result<VaultStatus, ApiError> {
        self.call(move |r| {
            r.reset_vault(&confirmation)?;
            r.vault_status()
        })
        .await?
    }
}

/// A store write on the runtime's connection, its failure as a redacted 400.
fn rows_result(
    r: &Runtime,
    write: impl FnOnce(&rusqlite::Connection) -> Result<(), StoreError>,
) -> Result<(), ApiError> {
    write(r.connection()).map_err(|e| r.api_error(e))
}

impl Runtime {
    /// Node's `vaultStatus()`.
    pub fn vault_status(&self) -> Result<VaultStatus, ApiError> {
        if self.vault.data_key.is_some() {
            return Ok(VaultStatus::Unlocked);
        }
        Ok(
            if has_vault(self.connection()).map_err(|e| self.api_error(e))? {
                VaultStatus::Locked
            } else {
                VaultStatus::Off
            },
        )
    }

    /// Whether a master-password action is running.
    pub fn vault_busy(&self) -> bool {
        self.vault.busy.load(Ordering::SeqCst)
    }

    /// The data key, while unlocked.
    pub(super) fn data_key(&self) -> Option<&DataKey> {
        self.vault.data_key.as_ref()
    }

    fn exclusive(&self) -> Result<Exclusive, ApiError> {
        self.vault
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| ApiError::bad_request(BUSY))?;
        Ok(Exclusive(self.vault.busy.clone()))
    }

    /// Node's `openVault` before the derivation: the stored wrap, or a refusal.
    fn password_wrap(&self) -> Result<PasswordWrap, ApiError> {
        mochi_store::vault_key(self.connection(), PASSWORD_WRAP_ID)
            .map_err(|e| self.api_error(e))?
            .ok_or_else(|| ApiError::bad_request("No master password is set"))
    }

    /// Every sealed AI key, opened. One that fails to open fails them all.
    fn open_sealed_keys(&self, data_key: &DataKey) -> Result<Vec<(String, String)>, ApiError> {
        let sealed =
            mochi_store::sealed_provider_keys(self.connection()).map_err(|e| self.api_error(e))?;
        sealed
            .into_iter()
            .map(|(id, value)| {
                let key = open(data_key, &value, &provider_context(&id)).map_err(vault_error)?;
                Ok((id, key))
            })
            .collect()
    }

    fn apply_enable(
        &mut self,
        data_key: DataKey,
        wrap: &PasswordWrap,
    ) -> Result<VaultStatus, ApiError> {
        let now = unix_ms();
        let store = self.engine.store();
        store
            .transaction(|| -> Result<(), StoreFailure> {
                let conn = store.connection();
                mochi_store::save_vault_key(conn, PASSWORD_WRAP_ID, PASSWORD_KIND, wrap, now)
                    .map_err(store_failure)?;
                for (id, key) in rows::get_provider_keys(conn).map_err(store_failure)? {
                    let sealed = seal(&data_key, &key, &provider_context(&id));
                    mochi_store::save_sealed_provider_key(conn, &id, &sealed, now)
                        .map_err(store_failure)?;
                }
                Ok(())
            })
            .map_err(|e| self.api_error(e))?;
        let blocked = self.scrub()?;
        self.vault.data_key = Some(data_key);
        self.invalidate(&[Topic::Vault, Topic::Providers, Topic::Exchange]);
        // The keys are sealed either way; only the old plaintext pages outlive this call.
        if blocked {
            self.emit(ServerEvent::Toast {
                level: ToastLevel::Warn,
                message: SCRUB_BLOCKED.to_string(),
            });
        }
        Ok(VaultStatus::Unlocked)
    }

    /// Folds the WAL into the database and truncates it; `true` when a reader blocked that.
    /// Node's driver does not wait for readers, so neither does this: the runtime task must not
    /// sit out rusqlite's busy timeout.
    fn scrub(&self) -> Result<bool, ApiError> {
        let conn = self.connection();
        conn.busy_timeout(Duration::ZERO)
            .map_err(|e| self.api_error(e))?;
        let result = mochi_store::scrub(conn);
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| self.api_error(e))?;
        match result {
            Ok(()) => Ok(false),
            Err(StoreError::CheckpointBlocked) => Ok(true),
            Err(error) => Err(self.api_error(error)),
        }
    }

    fn apply_unlock(&mut self, data_key: DataKey) -> Result<(), ApiError> {
        let keys = self.open_sealed_keys(&data_key)?;
        self.vault.data_key = Some(data_key);
        self.agents.keys.extend(keys);
        self.invalidate(&[Topic::Vault, Topic::Providers, Topic::Exchange]);
        Ok(())
    }

    fn apply_disable(&mut self, data_key: &DataKey) -> Result<VaultStatus, ApiError> {
        let keys = self.open_sealed_keys(data_key)?;
        let now = unix_ms();
        let store = self.engine.store();
        store
            .transaction(|| -> Result<(), StoreFailure> {
                let conn = store.connection();
                for (id, key) in &keys {
                    rows::save_provider_key(conn, id, key, now).map_err(store_failure)?;
                }
                // Also deletes saved OKX credentials, which have no plaintext form.
                mochi_store::delete_vault(conn).map_err(store_failure)
            })
            .map_err(|e| self.api_error(e))?;
        self.agents.keys.extend(keys);
        self.vault.data_key = None;
        self.invalidate(&[Topic::Vault, Topic::Providers, Topic::Exchange]);
        Ok(VaultStatus::Off)
    }

    /// Node's `resetVault`. Nothing sealed can be recovered, so all of it is deleted. Checks the
    /// busy flag but does not take it: it derives nothing.
    pub fn reset_vault(&mut self, confirmation: &str) -> Result<(), ApiError> {
        if confirmation != VAULT_RESET_PHRASE {
            return Err(ApiError::bad_request(format!(
                "Type exactly: {VAULT_RESET_PHRASE}"
            )));
        }
        if self.vault_busy() {
            return Err(ApiError::bad_request(BUSY));
        }
        if self.vault_status()? == VaultStatus::Off {
            return Err(ApiError::bad_request("No master password is set"));
        }
        let sealed =
            mochi_store::sealed_provider_keys(self.connection()).map_err(|e| self.api_error(e))?;
        let store = self.engine.store();
        store
            .transaction(|| -> Result<(), StoreFailure> {
                mochi_store::delete_vault(store.connection()).map_err(store_failure)
            })
            .map_err(|e| self.api_error(e))?;
        for (id, _) in sealed {
            self.agents.keys.remove(&id);
        }
        self.vault.data_key = None;
        self.invalidate(&[Topic::Vault, Topic::Providers, Topic::Exchange]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn a_panicking_derivation_is_a_500_and_frees_the_flag() {
        let busy = Arc::new(AtomicBool::new(true));
        let guard = Exclusive(busy.clone());
        let result = async move {
            let _guard = guard;
            off_actor(|| -> Result<(), VaultError> { panic!("derivation blew up") }).await
        }
        .await;
        assert_eq!(
            result.unwrap_err().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(!busy.load(Ordering::SeqCst));
    }

    #[test]
    fn a_wrong_password_is_403_and_nothing_else_is() {
        assert_eq!(
            vault_error(VaultError::WrongPassword).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            vault_error(VaultError::Kdf).status(),
            StatusCode::BAD_REQUEST
        );
    }
}
