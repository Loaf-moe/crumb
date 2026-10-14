//! A fake APNs: an in-process HTTP/2 (h2c) server that records each request
//! and answers from a per-token script.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use crumb::apns::ApnsClient;
use crumb::config::{KeyId, Origin, TeamId};
use crumb::dedupe::Dedupe;
use crumb::dispatch::Gateway;
use crumb::jwt::{SystemClock, TokenSource};
use crumb::server::router;
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use p256::ecdsa::SigningKey;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::ServiceExt;

pub const APP_ID: &str = "moe.loaf.chat.ios";

#[derive(Debug, Clone, Copy)]
pub enum Reply {
    Ok,
    Error(u16, &'static str),
    /// Never answer.
    Stall,
}

#[derive(Debug, Clone)]
pub struct Seen {
    pub token: String,
    pub headers: HeaderMap,
    pub body: Value,
}

#[derive(Default)]
struct State {
    seen: Mutex<Vec<Seen>>,
    replies: Mutex<HashMap<String, VecDeque<Reply>>>,
    connections: AtomicUsize,
    /// Aborting these drops every open connection, like an APNs restart.
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

pub struct FakeApns {
    pub addr: SocketAddr,
    state: Arc<State>,
}

impl FakeApns {
    pub async fn start() -> Self {
        Self::start_with_streams(None).await
    }

    /// `streams` caps concurrent streams per connection, as APNs does.
    pub async fn start_with_streams(streams: Option<u32>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(State::default());
        let server_state = state.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                server_state.connections.fetch_add(1, Ordering::SeqCst);
                let state = server_state.clone();
                let task = tokio::spawn(async move {
                    let service = service_fn(move |req| answer(state.clone(), req));
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .max_concurrent_streams(streams)
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
                server_state.tasks.lock().unwrap().push(task.abort_handle());
            }
        });
        Self { addr, state }
    }

    /// Queues replies for a token; once they run out it gets 200.
    pub fn script(&self, token: &str, replies: impl IntoIterator<Item = Reply>) {
        self.state
            .replies
            .lock()
            .unwrap()
            .entry(token.to_owned())
            .or_default()
            .extend(replies);
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }

    pub fn seen_for(&self, token: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.token == token)
            .collect()
    }

    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    pub fn drop_connections(&self) {
        for task in self.state.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

async fn answer(
    state: Arc<State>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let token = req
        .uri()
        .path()
        .strip_prefix("/3/device/")
        .unwrap_or_default()
        .to_owned();
    let headers = req.headers().clone();
    let body = req.into_body().collect().await.unwrap().to_bytes();
    state.seen.lock().unwrap().push(Seen {
        token: token.clone(),
        headers,
        body: serde_json::from_slice(&body).unwrap(),
    });
    let reply = state
        .replies
        .lock()
        .unwrap()
        .get_mut(&token)
        .and_then(VecDeque::pop_front)
        .unwrap_or(Reply::Ok);
    let response = match reply {
        Reply::Ok => Response::new(Full::default()),
        Reply::Error(status, reason) => Response::builder()
            .status(status)
            .header("apns-id", "00000000-0000-0000-0000-000000000000")
            .body(Full::new(format!(r#"{{"reason":"{reason}"}}"#).into()))
            .unwrap(),
        Reply::Stall => {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            unreachable!()
        }
    };
    Ok(response)
}

pub fn gateway(apns: &FakeApns, deadline: Duration) -> Router {
    let tokens = TokenSource::new(
        SigningKey::from_slice(&[7; 32]).unwrap(),
        KeyId::try_from("KEY1234567".to_owned()).unwrap(),
        TeamId::try_from("6W2A5N37N3".to_owned()).unwrap(),
        Arc::new(SystemClock),
    );
    let client = ApnsClient::new(
        Origin::from(apns.addr),
        HeaderValue::from_static(APP_ID),
        tokens,
    );
    let apps = HashMap::from([(APP_ID.to_owned(), client)]);
    let dedupe = Dedupe::new(Duration::from_secs(900), 1000);
    router(Arc::new(Gateway::new(apps, dedupe, deadline)))
}

/// Sends without a `Content-Type`: crumb must not need one.
pub async fn call(app: &Router, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

pub async fn notify(app: &Router, body: &str) -> (StatusCode, Value) {
    call(app, "POST", "/_matrix/push/v1/notify", body).await
}

/// An `event_id_only` notification, as Loaf Chat's pusher gets them.
pub fn event(event_id: &str, tokens: &[&str]) -> String {
    let devices: Vec<String> = tokens
        .iter()
        .map(|t| {
            format!(
                r#"{{"app_id":"{APP_ID}","pushkey":"{t}","data":{{"format":"event_id_only"}}}}"#
            )
        })
        .collect();
    format!(
        r#"{{"notification":{{"event_id":"{event_id}","room_id":"!r:loaf.moe","counts":{{"unread":1}},"devices":[{}]}}}}"#,
        devices.join(",")
    )
}

/// Everything crumb logs on this thread while the guard lives. Tests using
/// it run on tokio's default current-thread runtime, so every task logs
/// here.
pub struct Logs {
    buffer: Arc<Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl Logs {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.buffer.lock().unwrap()).into_owned()
    }
}

pub fn capture_logs() -> Logs {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("crumb=debug")
        .with_writer(move || BufferWriter(writer.clone()))
        .finish();
    Logs {
        buffer,
        _guard: tracing::subscriber::set_default(subscriber),
    }
}

struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
