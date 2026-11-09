//! `GET /api/events`: the server-sent event stream the web app listens on (Node's route,
//! `apps/server/src/app.ts:112-131`).
//!
//! Every message is `data: <JSON>\n\n`, with no `event:`, `id:` or `retry:` field: the web app
//! reads them all through `EventSource.onmessage`. The guard has already checked the token,
//! which only this path also accepts as `?token=`, since an `EventSource` cannot send headers.
//!
//! Each connection gets its own task that copies runtime events into a bounded buffer, which
//! the response body drains. A client that stops reading fills its buffer and is dropped; the
//! runtime and the other clients never wait for it. Node writes to every socket synchronously
//! and lets them buffer without limit; ending the stream instead costs the client nothing it
//! cannot recover, because the web app reconnects and refetches everything when a stream ends.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::header::CACHE_CONTROL;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream};
use mochi_domain::ServerEvent;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{interval_at, Instant, Interval, MissedTickBehavior};

use crate::error::ApiError;
use crate::AppState;

/// Node's per-connection `setInterval(..., 15_000)`. The web app counts 40 s of silence as a
/// dead stream, so an idle stream must still say something.
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// Messages waiting for one client. Far more than a reading client ever has queued: the
/// runtime sends a handful of events per replay batch and per live second.
pub const CONNECTION_BUFFER: usize = 256;

/// Ends every open stream at once, as Node's `preClose` hook does, so a graceful shutdown is
/// not kept waiting by streams that would otherwise never finish.
#[derive(Clone)]
pub struct SseStreams {
    closed: Arc<watch::Sender<bool>>,
}

impl Default for SseStreams {
    fn default() -> Self {
        let (closed, _) = watch::channel(false);
        SseStreams {
            closed: Arc::new(closed),
        }
    }
}

impl SseStreams {
    /// Ends the open streams, and any opened later.
    pub fn close_all(&self) {
        self.closed.send_replace(true);
    }

    fn watch(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }
}

pub async fn events(State(state): State<AppState>) -> Result<Response, ApiError> {
    // Subscribe before taking the snapshot, so nothing emitted in between is lost. Something
    // emitted in between arrives after the snapshot, which only repeats what it says.
    let updates = state.runtime.subscribe();
    let closed = state.streams.watch();
    // The first beat is one interval away, as with setInterval: none straight away.
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let (feeds, replay) = state
        .runtime
        .call(|runtime| (runtime.feeds(), runtime.replay_state()))
        .await?;
    let (buffer, queued) = mpsc::channel(CONNECTION_BUFFER);
    // REPLAY, then LIVE, then the replay state. Three messages always fit the empty buffer.
    for status in feeds {
        let _ = buffer.try_send(ServerEvent::Feed { status });
    }
    let _ = buffer.try_send(ServerEvent::Replay { state: replay });
    tokio::spawn(forward(updates, buffer, heartbeat, closed.clone()));

    // Axum's Sse sends `cache-control: no-cache`; Node sends `no-cache, no-transform`, which
    // also keeps a proxy from compressing (and so holding back) the stream.
    let headers = [(CACHE_CONTROL, "no-cache, no-transform")];
    Ok((headers, Sse::new(body(queued, closed))).into_response())
}

/// Copies runtime events and heartbeats into one client's buffer until the client goes away,
/// falls behind, or the server shuts down.
async fn forward(
    mut updates: broadcast::Receiver<ServerEvent>,
    buffer: mpsc::Sender<ServerEvent>,
    mut heartbeat: Interval,
    mut closed: watch::Receiver<bool>,
) {
    loop {
        let event = tokio::select! {
            update = updates.recv() => match update {
                Ok(event) => event,
                // Lagged: this task fell behind the runtime and events were lost, so the
                // client's picture is wrong. Closed: the runtime is gone. Either way the stream
                // ends, and the web app reconnects and refetches.
                Err(_) => return,
            },
            _ = heartbeat.tick() => ServerEvent::Heartbeat,
            // The response body was dropped: the client disconnected.
            () = buffer.closed() => return,
            _ = closed.wait_for(|closed| *closed) => return,
        };
        // A full buffer means the client has stopped reading. Dropping the sender ends its
        // stream once what is already queued has gone out.
        if buffer.try_send(event).is_err() {
            return;
        }
    }
}

/// The response body: each queued event as one `data:` message. Ends when the forwarding task
/// stops, or straight away on shutdown, even with messages still queued.
fn body(
    queued: mpsc::Receiver<ServerEvent>,
    closed: watch::Receiver<bool>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold((queued, closed), |(mut queued, mut closed)| async move {
        let event = tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => return None,
            event = queued.recv() => event?,
        };
        Some((Ok(message(&event)), (queued, closed)))
    })
}

/// `data: <JSON>\n\n`. Compact JSON has no newline, so it is always one `data:` line.
fn message(event: &ServerEvent) -> Event {
    let json = serde_json::to_string(event).expect("a ServerEvent always serializes");
    Event::default().data(json)
}
