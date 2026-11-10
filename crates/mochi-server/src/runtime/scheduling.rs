//! Replay batches are a state machine: advance one bar, await its agents, then advance again.
//! Neither pending futures nor idle waiters retain a RuntimeHandle.
use super::{lock, Runtime, RuntimeHandle};
use crate::error::ApiError;
use mochi_domain::{DataSource, Quote, ServerEvent, Topic};
use mochi_store::{rows, set_setting};
use tokio::sync::oneshot;

pub(super) struct ReplayRequest {
    steps: u32,
    reply: Option<oneshot::Sender<Result<(), ApiError>>>,
}
pub(super) struct ReplayBatch {
    request: ReplayRequest,
    quotes: Vec<Quote>,
    waiting: bool,
}
impl RuntimeHandle {
    /// Used by ownership-changing operations before seeking/resetting the replay tape.
    pub async fn wait_replay_idle(&self) -> Result<(), ApiError> {
        let (reply, done) = oneshot::channel();
        self.call(move |r| {
            if r.replay_batch.is_none() && r.replay_queue.is_empty() {
                let _ = reply.send(());
            } else {
                r.replay_idle.push(reply);
            }
        })
        .await?;
        done.await.map_err(|_| super::stopped())
    }
}
impl Runtime {
    pub(super) fn queue_replay(
        &mut self,
        steps: u32,
        reply: Option<oneshot::Sender<Result<(), ApiError>>>,
    ) {
        self.replay_queue.push_back(ReplayRequest { steps, reply });
    }
    pub(super) fn on_replay_tick(&mut self) {
        if let Some(steps) = self.clock.on_tick() {
            self.queue_replay(steps, None);
        }
    }
    fn replay_busy(&self) -> bool {
        self.agents.active.keys().any(|id| {
            rows::get_agent(self.connection(), id)
                .ok()
                .flatten()
                .is_some_and(|a| {
                    self.require_account(&a.account_id)
                        .is_ok_and(|a| a.source == DataSource::Replay)
                })
        })
    }
    pub(super) fn pump_replay(&mut self) {
        loop {
            if self.replay_batch.is_none() {
                let Some(request) = self.replay_queue.pop_front() else {
                    for done in self.replay_idle.drain(..) {
                        let _ = done.send(());
                    }
                    return;
                };
                self.clock.begin_batch();
                self.signal_raised = false;
                self.replay_batch = Some(ReplayBatch {
                    request,
                    quotes: Vec::new(),
                    waiting: false,
                });
            }
            if self.replay_busy() {
                return;
            }
            let mut batch = self.replay_batch.take().unwrap();
            let result = (|| -> Result<bool, ApiError> {
                if batch.waiting {
                    let now = lock(&self.replay).now();
                    if now - self.last_replay_equity_at >= super::REPLAY_EQUITY_EVERY_MS {
                        self.last_replay_equity_at = now;
                        self.record_equity(DataSource::Replay)
                            .map_err(|e| self.api_error(e))?;
                        self.invalidate(&[Topic::Arena, Topic::Accounts]);
                    }
                    batch.waiting = false;
                }
                if batch.request.steps == 0 || self.signal_raised {
                    return Ok(false);
                }
                let next = lock(&self.replay).step();
                if next.is_empty() {
                    return Ok(false);
                }
                batch.quotes = next;
                batch.request.steps -= 1;
                for quote in &batch.quotes {
                    let result = self.engine.on_quote(quote);
                    self.handle_engine_events();
                    result.map_err(|e| self.api_error(e))?;
                }
                self.tick_agents(DataSource::Replay)?;
                batch.waiting = true;
                Ok(true)
            })();
            match result {
                Ok(true) => {
                    self.replay_batch = Some(batch);
                    continue;
                }
                Ok(false) => {}
                Err(e) => self.stop_replay_with(e.message()),
            }
            let position = lock(&self.replay).position();
            let persisted = set_setting(self.connection(), "replay.cursor", &position)
                .map_err(|e| self.api_error(e));
            if let Err(e) = &persisted {
                self.stop_replay_with(e.message());
            }
            for quote in batch.quotes {
                self.emit(ServerEvent::Quote { quote });
            }
            if lock(&self.replay).at_end() {
                self.set_playing(false);
            }
            self.emit(ServerEvent::Replay {
                state: self.replay_state(),
            });
            self.invalidate(&[Topic::Accounts, Topic::Positions]);
            self.clock.end_batch();
            if let Some(reply) = batch.request.reply {
                let _ = reply.send(persisted);
            }
        }
    }
}
