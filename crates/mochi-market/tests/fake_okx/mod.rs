//! A fake OKX for the feed tests, entirely in memory: REST is an axum router called in-process,
//! and every WebSocket dial gets one end of a pipe whose other end goes to the test. Nothing
//! touches a real socket, so tokio's paused clock only moves when every task is waiting on a
//! timer, and timing tests are exact.

#![allow(dead_code)]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use mochi_market::okx_public::{BoxFuture, Connection, HttpReply, Transport};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tower::ServiceExt;

pub const REST: &str = "https://okx.test";
pub const WS: &str = "ws://okx.test/ws/v5/public";

pub struct FakeOkx {
    rest: Mutex<Router>,
    /// Every REST URL asked for, with when.
    pub gets: Mutex<Vec<(Instant, String)>>,
    /// When each WebSocket dial happened, refused or not.
    pub dials: Mutex<Vec<Instant>>,
    /// While set, dials fail as a refused connection does.
    pub refuse: AtomicBool,
    sockets: mpsc::UnboundedSender<DuplexStream>,
}

impl FakeOkx {
    /// The fake, and where the server ends of accepted dials arrive.
    pub fn new(rest: Router) -> (Arc<Self>, mpsc::UnboundedReceiver<DuplexStream>) {
        let (sockets, incoming) = mpsc::unbounded_channel();
        let fake = Arc::new(FakeOkx {
            rest: Mutex::new(rest),
            gets: Mutex::default(),
            dials: Mutex::default(),
            refuse: AtomicBool::new(false),
            sockets,
        });
        (fake, incoming)
    }

    pub fn set_rest(&self, rest: Router) {
        *self.rest.lock().unwrap() = rest;
    }

    /// The URLs asked for whose path contains `part`, with when.
    pub fn gets_matching(&self, part: &str) -> Vec<(Instant, String)> {
        self.gets
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, url)| url.contains(part))
            .cloned()
            .collect()
    }

    pub fn dial_times(&self) -> Vec<Instant> {
        self.dials.lock().unwrap().clone()
    }
}

impl Transport for FakeOkx {
    fn get(&self, url: String) -> BoxFuture<'static, Result<HttpReply, String>> {
        self.gets
            .lock()
            .unwrap()
            .push((Instant::now(), url.clone()));
        let router = self.rest.lock().unwrap().clone();
        Box::pin(async move {
            let path = url.strip_prefix(REST).unwrap_or(&url).to_string();
            let request = Request::get(path).body(Body::empty()).unwrap();
            let response = router.oneshot(request).await.unwrap();
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            Ok(HttpReply {
                status,
                body: body.to_vec(),
            })
        })
    }

    fn dial(&self, _url: String) -> BoxFuture<'static, io::Result<Box<dyn Connection>>> {
        self.dials.lock().unwrap().push(Instant::now());
        if self.refuse.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"))
            });
        }
        let (client, server) = tokio::io::duplex(1 << 16);
        let _ = self.sockets.send(server);
        Box::pin(async move { Ok(Box::new(client) as Box<dyn Connection>) })
    }
}

/// Completes the WebSocket handshake on the next dial's server end.
pub async fn accept(
    incoming: &mut mpsc::UnboundedReceiver<DuplexStream>,
) -> WebSocketStream<DuplexStream> {
    let stream = incoming.recv().await.expect("the feed dials");
    tokio_tungstenite::accept_async(stream)
        .await
        .expect("handshake")
}
