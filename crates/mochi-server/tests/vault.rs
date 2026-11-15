//! The master password, ported from the `master password` group of
//! `apps/server/test/okx-live.test.ts` (its OKX reconnect cases come with OKX trading) and the
//! vault case of `api-integration.test.ts`. A fast scrypt cost keeps each derivation short.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mochi_ai::{MockProvider, ProviderConfig};
use mochi_domain::{ServerEvent, ToastLevel, VaultStatus};
use mochi_server::config::{config_from, Config};
use mochi_server::{app, AppState, RuntimeHandle, RuntimeOptions};
use mochi_vault::KdfParams;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

const PASSWORD: &str = "mochi-master-password";
const AI_KEY: &str = "sk-ai-key-distinctive-0123456789";
const FAST: KdfParams = KdfParams {
    n: 1024,
    r: 8,
    p: 1,
};
const SCRUB_BLOCKED: &str =
    "Deleted key data may stay in the database file until MochiPrep closes.";

/// A database file that outlives restarts, and the keys every provider was built with.
struct Harness {
    _dir: TempDir,
    path: PathBuf,
    kdf: KdfParams,
    built_with: Arc<Mutex<Vec<String>>>,
    rt: RuntimeHandle,
}

fn start(path: &Path, kdf: KdfParams, built_with: &Arc<Mutex<Vec<String>>>) -> RuntimeHandle {
    let mut options = RuntimeOptions::new(mochi_store::open_database(path).unwrap());
    options.replay_minutes = Some(mochi_market::tape::WARMUP_MINUTES + 30);
    options.kdf = kdf;
    let seen = built_with.clone();
    options.provider_factory = Some(Arc::new(move |_, key| {
        seen.lock().unwrap().push(key.to_string());
        Ok(Arc::new(MockProvider))
    }));
    RuntimeHandle::start(options).unwrap()
}

impl Harness {
    /// Node's `boot()` plus its `beforeEach`: a `remote` provider whose key was saved while OFF.
    async fn new() -> Self {
        Self::with_kdf(FAST).await
    }

    async fn with_kdf(kdf: KdfParams) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let built_with = Arc::default();
        let rt = start(&path, kdf, &built_with);
        rt.call(|r| {
            r.connection()
                .execute_batch(
                    "INSERT INTO providers (id,name,kind,base_url,default_model,input_price,
                       output_price,extra_headers,created_at)
                     VALUES ('remote','r','OPENAI',NULL,'m',NULL,NULL,'{}',1)",
                )
                .unwrap();
            r.set_key("remote", AI_KEY).unwrap();
        })
        .await
        .unwrap();
        Harness {
            _dir: dir,
            path,
            kdf,
            built_with,
            rt,
        }
    }

    async fn restart(&mut self) {
        self.rt.shutdown().await;
        self.rt = start(&self.path, self.kdf, &self.built_with);
    }

    async fn status(&self) -> VaultStatus {
        self.rt.call(|r| r.vault_status()).await.unwrap().unwrap()
    }

    async fn has_key(&self) -> bool {
        self.rt
            .call(|r| {
                r.provider_views()
                    .unwrap()
                    .into_iter()
                    .find(|p| p.id == "remote")
                    .unwrap()
                    .has_key
            })
            .await
            .unwrap()
    }

    async fn provider_for(&self) -> Result<(), String> {
        self.rt
            .call(|r| {
                let config: ProviderConfig =
                    mochi_store::rows::get_provider(r.connection(), "remote")
                        .unwrap()
                        .unwrap();
                r.provider_for(&config).map(|_| ()).map_err(|e| e.message)
            })
            .await
            .unwrap()
    }

    /// Node's `fileHolds`: the bytes appear in the database file or its WAL.
    fn file_holds(&self, value: &str) -> bool {
        let mut wal = self.path.as_os_str().to_owned();
        wal.push("-wal");
        [self.path.clone(), PathBuf::from(wal)].iter().any(|file| {
            std::fs::read(file).is_ok_and(|bytes| {
                bytes
                    .windows(value.len())
                    .any(|window| window == value.as_bytes())
            })
        })
    }

    async fn scrub(&self) {
        self.rt
            .call(|r| mochi_store::scrub(r.connection()).unwrap())
            .await
            .unwrap();
    }

    async fn sql(&self, statements: &'static str) {
        self.rt
            .call(move |r| r.connection().execute_batch(statements).unwrap())
            .await
            .unwrap();
    }

    async fn has_saved_okx(&self, account: &'static str) -> bool {
        self.rt
            .call(move |r| {
                mochi_store::rows::has_sealed_exchange_credentials(r.connection(), account).unwrap()
            })
            .await
            .unwrap()
    }

    /// An OKX demo account with credentials saved under the vault.
    async fn saved_okx_account(&self) {
        self.sql(
            "INSERT INTO accounts (id,name,source,initial_balance,wallet_balance,used_margin,
               realized_pnl,fees_paid,funding_paid,config,created_at,venue)
               VALUES ('acct_okx','OKX','LIVE','0','0','0','0','0','0','{}',1,'OKX_DEMO');
             INSERT INTO sealed_exchange_credentials (account_id,sealed,updated_at)
               VALUES ('acct_okx','v1.aXY=.dGFn.Ym9keQ==',1);",
        )
        .await;
    }
}

fn message<T: std::fmt::Debug>(result: Result<T, mochi_server::ApiError>) -> (u16, String) {
    let error = result.expect_err("expected a refusal");
    (error.status().as_u16(), error.message().to_string())
}

#[tokio::test]
async fn seals_saved_ai_keys_scrubs_the_plaintext_and_loads_them_only_after_unlock() {
    let mut h = Harness::new().await;
    assert!(h.file_holds(AI_KEY));
    assert_eq!(
        h.rt.enable_vault(PASSWORD.into()).await.unwrap(),
        VaultStatus::Unlocked
    );
    assert_eq!(h.status().await, VaultStatus::Unlocked);
    assert!(!h.file_holds(AI_KEY));
    assert!(!h.file_holds(PASSWORD));

    h.restart().await;
    assert_eq!(h.status().await, VaultStatus::Locked);
    let refused = h.provider_for().await.unwrap_err();
    assert!(refused.starts_with("Unlock MochiPrep"), "{refused}");
    let set = h.rt.call(|r| r.set_key("remote", "other")).await.unwrap();
    assert_eq!(
        message(set),
        (
            400,
            "Unlock MochiPrep with your master password first".into()
        )
    );
    assert!(!h.has_key().await);

    let wrong = h.rt.unlock("not the password".into()).await;
    assert_eq!(message(wrong), (403, "Wrong master password".into()));
    assert_eq!(h.status().await, VaultStatus::Locked);
    let outcome = h.rt.unlock(PASSWORD.into()).await.unwrap();
    assert_eq!(outcome.status, VaultStatus::Unlocked);
    assert!(outcome.failed.is_empty());
    assert_eq!(h.status().await, VaultStatus::Unlocked);
    h.provider_for().await.unwrap();
    assert_eq!(h.built_with.lock().unwrap().last().unwrap(), AI_KEY);
    // A loaded key is a secret again: errors and logs have it removed.
    let redacted =
        h.rt.call(|r| r.redact(&format!("echo {AI_KEY}")))
            .await
            .unwrap();
    assert!(!redacted.contains(AI_KEY), "{redacted}");
    h.rt.shutdown().await;
}

#[tokio::test]
async fn keys_saved_while_unlocked_are_sealed_too() {
    let mut h = Harness::new().await;
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    h.rt.call(|r| r.set_key("remote", "sk-replaced-key-abcdef0123456789"))
        .await
        .unwrap()
        .unwrap();
    h.scrub().await;
    assert!(!h.file_holds("sk-replaced-key-abcdef0123456789"));
    h.restart().await;
    h.rt.unlock(PASSWORD.into()).await.unwrap();
    assert!(h.has_key().await);
    h.provider_for().await.unwrap();
    assert_eq!(
        h.built_with.lock().unwrap().last().unwrap(),
        "sk-replaced-key-abcdef0123456789"
    );
    h.rt.shutdown().await;
}

#[tokio::test]
async fn changes_the_password_without_re_entering_keys() {
    let mut h = Harness::new().await;
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    let wrong =
        h.rt.change_password("wrong".into(), "new-master-password".into())
            .await;
    assert_eq!(message(wrong), (403, "Wrong master password".into()));
    assert_eq!(
        h.rt.change_password(PASSWORD.into(), "new-master-password".into())
            .await
            .unwrap(),
        VaultStatus::Unlocked
    );
    h.restart().await;
    let old = h.rt.unlock(PASSWORD.into()).await;
    assert_eq!(message(old), (403, "Wrong master password".into()));
    h.rt.unlock("new-master-password".into()).await.unwrap();
    assert!(h.has_key().await);
    h.rt.shutdown().await;
}

#[tokio::test]
async fn turning_it_off_restores_plaintext_ai_keys_and_deletes_saved_okx_keys() {
    let mut h = Harness::new().await;
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    h.saved_okx_account().await;
    let wrong = h.rt.disable_vault("wrong".into()).await;
    assert_eq!(message(wrong), (403, "Wrong master password".into()));
    assert!(h.has_saved_okx("acct_okx").await);
    assert_eq!(
        h.rt.disable_vault(PASSWORD.into()).await.unwrap(),
        VaultStatus::Off
    );
    assert_eq!(h.status().await, VaultStatus::Off);
    assert!(!h.has_saved_okx("acct_okx").await);
    h.restart().await;
    assert_eq!(h.status().await, VaultStatus::Off);
    assert!(h.has_key().await);
    h.provider_for().await.unwrap();
    h.rt.shutdown().await;
}

#[tokio::test]
async fn disabling_works_while_locked_too() {
    let mut h = Harness::new().await;
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    h.restart().await;
    h.rt.disable_vault(PASSWORD.into()).await.unwrap();
    assert_eq!(h.status().await, VaultStatus::Off);
    assert!(h.has_key().await);
    h.rt.shutdown().await;
}

#[tokio::test]
async fn a_lost_password_can_only_be_reset_by_deleting_what_it_sealed() {
    let mut h = Harness::new().await;
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    h.saved_okx_account().await;
    h.restart().await;
    let refused = h.rt.reset_vault("yes".into()).await;
    assert_eq!(
        message(refused),
        (400, "Type exactly: DELETE SAVED KEYS".into())
    );
    assert_eq!(
        h.rt.reset_vault("DELETE SAVED KEYS".into()).await.unwrap(),
        VaultStatus::Off
    );
    assert_eq!(h.status().await, VaultStatus::Off);
    assert!(!h.has_key().await);
    assert!(!h.has_saved_okx("acct_okx").await);
    let again = h.rt.reset_vault("DELETE SAVED KEYS".into()).await;
    assert_eq!(message(again), (400, "No master password is set".into()));
    h.rt.shutdown().await;
}

#[tokio::test]
async fn refusals_follow_nodes_check_order() {
    let h = Harness::new().await;
    let unlock = h.rt.unlock(PASSWORD.into()).await;
    assert_eq!(message(unlock), (400, "MochiPrep is not locked".into()));
    let change =
        h.rt.change_password(PASSWORD.into(), "new-master-password".into())
            .await;
    assert_eq!(message(change), (400, "No master password is set".into()));
    let disable = h.rt.disable_vault(PASSWORD.into()).await;
    assert_eq!(message(disable), (400, "No master password is set".into()));
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    let twice = h.rt.enable_vault(PASSWORD.into()).await;
    assert_eq!(
        message(twice),
        (400, "A master password is already set".into())
    );
    let unlocked = h.rt.unlock(PASSWORD.into()).await;
    assert_eq!(message(unlocked), (400, "MochiPrep is not locked".into()));
    h.rt.shutdown().await;
}

/// A corrupt sealed key fails the whole unlock, as Node's map-then-assign does.
#[tokio::test]
async fn unlock_loads_every_sealed_key_or_none() {
    let mut h = Harness::new().await;
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    h.sql(
        "INSERT INTO providers (id,name,kind,base_url,default_model,input_price,output_price,
           extra_headers,created_at) VALUES ('other','o','OPENAI',NULL,'m',NULL,NULL,'{}',2);
         INSERT INTO sealed_provider_keys (provider_id,sealed,updated_at)
           VALUES ('other','v1.AAAAAAAAAAAAAAAA.AAAAAAAAAAAAAAAAAAAAAA==.AAAA',1);",
    )
    .await;
    h.restart().await;
    let corrupt = h.rt.unlock(PASSWORD.into()).await;
    assert_eq!(
        message(corrupt),
        (400, "Could not decrypt sealed value".into())
    );
    assert_eq!(h.status().await, VaultStatus::Locked);
    assert!(!h.has_key().await);
    let leaked =
        h.rt.call(|r| r.secrets().iter().any(|s| s == AI_KEY))
            .await
            .unwrap();
    assert!(!leaked);
    h.rt.shutdown().await;
}

/// Node's `exclusive`: one master-password action at a time, the flag held across the slow
/// derivation and released however the action ends.
#[tokio::test]
async fn one_master_password_action_at_a_time() {
    // A slower cost, so the first action is still deriving while the others arrive.
    let h = Harness::with_kdf(KdfParams {
        n: 1 << 14,
        r: 8,
        p: 1,
    })
    .await;
    let first = {
        let rt = h.rt.clone();
        tokio::spawn(async move { rt.enable_vault(PASSWORD.into()).await })
    };
    let mut busy = false;
    for _ in 0..2000 {
        if h.rt.call(|r| r.vault_busy()).await.unwrap() {
            busy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(busy, "the first action never took the flag");
    const BUSY: &str = "Another master password action is in progress";
    assert_eq!(
        message(h.rt.enable_vault(PASSWORD.into()).await),
        (400, BUSY.into())
    );
    assert_eq!(
        message(h.rt.unlock(PASSWORD.into()).await),
        (400, BUSY.into())
    );
    assert_eq!(
        message(
            h.rt.change_password(PASSWORD.into(), "new-master-password".into())
                .await
        ),
        (400, BUSY.into())
    );
    assert_eq!(
        message(h.rt.disable_vault(PASSWORD.into()).await),
        (400, BUSY.into())
    );
    // Reset checks the phrase first, then the flag, and never takes it.
    assert_eq!(
        message(h.rt.reset_vault("no".into()).await),
        (400, "Type exactly: DELETE SAVED KEYS".into())
    );
    assert_eq!(
        message(h.rt.reset_vault("DELETE SAVED KEYS".into()).await),
        (400, BUSY.into())
    );
    assert_eq!(first.await.unwrap().unwrap(), VaultStatus::Unlocked);
    assert!(!h.rt.call(|r| r.vault_busy()).await.unwrap());
    h.rt.change_password(PASSWORD.into(), "new-master-password".into())
        .await
        .unwrap();
    h.rt.shutdown().await;
}

#[tokio::test]
async fn a_failed_action_releases_the_flag() {
    // Not a power of two: the derivation itself fails, off the actor.
    let h = Harness::with_kdf(KdfParams {
        n: 1000,
        r: 8,
        p: 1,
    })
    .await;
    let failed = h.rt.enable_vault(PASSWORD.into()).await;
    assert_eq!(
        message(failed),
        (400, "Invalid key derivation parameters".into())
    );
    assert!(!h.rt.call(|r| r.vault_busy()).await.unwrap());
    assert_eq!(h.status().await, VaultStatus::Off);
    assert!(h.file_holds(AI_KEY), "nothing was sealed");
    h.rt.shutdown().await;
}

fn toasts(events: &mut tokio::sync::broadcast::Receiver<ServerEvent>) -> Vec<(ToastLevel, String)> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ServerEvent::Toast { level, message } = event {
            out.push((level, message));
        }
    }
    out
}

#[tokio::test]
async fn warns_and_still_succeeds_when_another_reader_keeps_the_old_plaintext_in_the_wal() {
    let h = Harness::new().await;
    let mut events = h.rt.subscribe();
    // A reader holding a snapshot pins the WAL, so the scrub's checkpoint cannot truncate it.
    let reader = rusqlite::Connection::open(&h.path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
        .unwrap();
    let enabled = h.rt.enable_vault(PASSWORD.into()).await;
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    assert_eq!(enabled.unwrap(), VaultStatus::Unlocked);
    assert_eq!(
        toasts(&mut events),
        vec![(ToastLevel::Warn, SCRUB_BLOCKED.to_string())]
    );
    h.rt.shutdown().await;
}

#[tokio::test]
async fn does_not_warn_when_the_scrub_truncates_the_wal() {
    let h = Harness::new().await;
    let mut events = h.rt.subscribe();
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    assert_eq!(toasts(&mut events), vec![]);
    h.rt.shutdown().await;
}

// ---- HTTP -------------------------------------------------------------------------------------

const HOST: &str = "localhost:8787";

fn config() -> Config {
    let mut config = config_from(|_| None);
    config.data_dir = PathBuf::from(".");
    config
}

async fn post(state: &AppState, path: &str, body: Value) -> (u16, String) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {}", state.token))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app(state.clone()).oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn get(state: &AppState, path: &str) -> Value {
    let request = Request::builder()
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {}", state.token))
        .body(Body::empty())
        .unwrap();
    let response = app(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn manages_the_master_password_over_http_without_echoing_it() {
    let h = Harness::new().await;
    let state = AppState::new(Arc::new(config()), h.rt.clone());
    assert_eq!(get(&state, "/api/state").await["vault"], "OFF");
    assert_eq!(get(&state, "/api/vault").await, json!({ "status": "OFF" }));

    let (status, body) = post(&state, "/api/vault", json!({ "password": "short" })).await;
    assert_eq!(
        (status, body.as_str()),
        (400, r#"{"error":"password: Use at least 10 characters"}"#)
    );
    let created = post(
        &state,
        "/api/vault",
        json!({ "password": "long-enough-password" }),
    )
    .await;
    assert_eq!(created, (200, r#"{"status":"UNLOCKED"}"#.to_string()));

    let (status, body) = post(
        &state,
        "/api/vault/password",
        json!({ "current": "guess-123456", "next": "another-long-password" }),
    )
    .await;
    // 403, not 401: the browser client treats 401 as a stale session token and retries.
    assert_eq!(status, 403);
    assert_eq!(body, r#"{"error":"Wrong master password"}"#);
    assert!(!body.contains("guess-123456"));
    let changed = post(
        &state,
        "/api/vault/password",
        json!({ "current": "long-enough-password", "next": "another-long-password" }),
    )
    .await;
    assert_eq!(changed, (200, r#"{"status":"UNLOCKED"}"#.to_string()));
    let not_locked = post(
        &state,
        "/api/vault/unlock",
        json!({ "password": "another-long-password" }),
    )
    .await;
    assert_eq!(
        not_locked,
        (400, r#"{"error":"MochiPrep is not locked"}"#.to_string())
    );

    let (status, _) = post(
        &state,
        "/api/vault/reset",
        json!({ "confirmation": "nope" }),
    )
    .await;
    assert_eq!(status, 400);
    let reset = post(
        &state,
        "/api/vault/reset",
        json!({ "confirmation": "DELETE SAVED KEYS" }),
    )
    .await;
    assert_eq!(reset, (200, r#"{"status":"OFF"}"#.to_string()));
    h.rt.shutdown().await;
}

#[tokio::test]
async fn unlocks_and_turns_off_over_http() {
    let mut h = Harness::new().await;
    h.rt.enable_vault(PASSWORD.into()).await.unwrap();
    h.restart().await;
    let state = AppState::new(Arc::new(config()), h.rt.clone());
    assert_eq!(
        get(&state, "/api/vault").await,
        json!({ "status": "LOCKED" })
    );
    let (status, body) = post(&state, "/api/vault/unlock", json!({ "password": "wrong" })).await;
    assert_eq!((status, body.contains("wrong\"")), (403, false));
    let unlocked = post(&state, "/api/vault/unlock", json!({ "password": PASSWORD })).await;
    assert_eq!(
        unlocked,
        (200, r#"{"status":"UNLOCKED","failed":[]}"#.to_string())
    );
    let providers = get(&state, "/api/providers").await;
    let remote = providers
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "remote")
        .unwrap();
    assert_eq!(remote["hasKey"], true);
    let (status, _) = post(&state, "/api/vault/disable", json!({ "password": "" })).await;
    assert_eq!(status, 400, "an empty password fails the schema");
    let off = post(
        &state,
        "/api/vault/disable",
        json!({ "password": PASSWORD }),
    )
    .await;
    assert_eq!(off, (200, r#"{"status":"OFF"}"#.to_string()));
    h.rt.shutdown().await;
}
