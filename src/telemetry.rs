use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use futures_util::{Stream, TryStreamExt};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::AppState;
use crate::error::{SOURCE_HEADER, SOURCE_RELAY};
use crate::store::Store;
use crate::store::stats::Traffic;

pub const REQUEST_ID_HEADER: &str = "x-trout-request-id";

/// Response extension set by handlers so the request log can name the target.
#[derive(Debug, Clone)]
pub struct TargetHost(pub String);

/// Shared with handlers through request extensions; filled in as the request is processed.
#[derive(Debug, Clone, Default)]
pub struct Counters {
    pub wire_in: Arc<AtomicU64>,
    pub decoded_in: Arc<AtomicU64>,
    /// Bytes of the body sent on to the provider, uncompressed: what the client would have had to
    /// upload without the relay.
    pub upstream_sent: Arc<AtomicU64>,
    /// Set once the request has been passed on to a provider.
    pub forwarded: Arc<AtomicBool>,
    /// Answer bytes before compression. Stays zero when the answer is not compressed.
    pub plain_out: Arc<AtomicU64>,
}

impl Counters {
    pub fn mark_forwarded(&self, upstream_body_bytes: usize) {
        self.upstream_sent
            .store(upstream_body_bytes as u64, Ordering::Relaxed);
        self.forwarded.store(true, Ordering::Relaxed);
    }
}

/// Hands per-request traffic figures to a background task that adds them to the store, so the
/// request path never waits on the database.
#[derive(Clone)]
pub struct TrafficRecorder {
    tx: mpsc::UnboundedSender<Traffic>,
}

impl TrafficRecorder {
    /// Must be called inside a Tokio runtime. The task ends when every recorder is dropped.
    pub fn spawn(store: Store) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Traffic>();

        tokio::spawn(async move {
            while let Some(traffic) = rx.recv().await {
                if store.record_traffic(traffic).await.is_err() {
                    tracing::warn!(target: "trout::store", "recording traffic failed");
                }
            }
        });

        Self { tx }
    }

    fn record(&self, traffic: Traffic) {
        let _ = self.tx.send(traffic);
    }
}

fn route_kind(path: &str) -> &'static str {
    if path.starts_with("/v1/responses") || path.starts_with("/v1/conversations") {
        "responses"
    } else if path.starts_with("/v1/") {
        "proxy"
    } else {
        "admin"
    }
}

/// Outermost layer: assigns the request id, counts wire bytes both ways and writes exactly one
/// log line per request. Headers and bodies are never recorded.
pub async fn record(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Response {
    let request_id = Uuid::new_v4().simple().to_string();
    let method = request.method().clone();
    let route = route_kind(request.uri().path());
    let is_turn = method == Method::POST && request.uri().path() == "/v1/responses";
    let started = Instant::now();
    let counters = Counters::default();

    request.extensions_mut().insert(counters.clone());

    let (parts, body) = request.into_parts();
    let wire_in = counters.wire_in.clone();
    let counted = body.into_data_stream().inspect_ok(move |chunk| {
        wire_in.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    });
    let mut response = next
        .run(Request::from_parts(parts, Body::from_stream(counted)))
        .await;

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }

    let entry = LogEntry {
        request_id,
        method,
        route,
        target: response.extensions().get::<TargetHost>().map(|t| t.0.clone()),
        status: response.status().as_u16(),
        source: response
            .headers()
            .get(SOURCE_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(SOURCE_RELAY)
            .to_string(),
        started,
        ttfb_ms: None,
        counters,
        bytes_out: 0,
        is_turn,
        recorder: state.traffic.clone(),
    };
    let (parts, body) = response.into_parts();
    let logged = Logged {
        inner: Box::pin(body.into_data_stream()),
        entry,
    };

    Response::from_parts(parts, Body::from_stream(logged))
}

struct LogEntry {
    request_id: String,
    method: Method,
    route: &'static str,
    target: Option<String>,
    status: u16,
    source: String,
    started: Instant,
    ttfb_ms: Option<u64>,
    counters: Counters,
    bytes_out: u64,
    is_turn: bool,
    recorder: TrafficRecorder,
}

impl LogEntry {
    /// What this request adds to the traffic statistics. Byte figures count only for requests
    /// that reached a provider, so rejected requests cannot distort the saving.
    fn traffic(&self) -> Option<Traffic> {
        if self.route == "admin" {
            return None;
        }

        let forwarded = self.counters.forwarded.load(Ordering::Relaxed);
        let failed = self.status >= 400;
        let plain_out = self.counters.plain_out.load(Ordering::Relaxed);
        let traffic = Traffic {
            proxy_requests: u64::from(self.route == "proxy" && forwarded),
            response_turns: u64::from(self.is_turn && !failed),
            conversations_started: 0,
            failed_requests: u64::from(failed),
            client_up_bytes: if forwarded {
                self.counters.wire_in.load(Ordering::Relaxed)
            } else {
                0
            },
            provider_up_bytes: if forwarded {
                self.counters.upstream_sent.load(Ordering::Relaxed)
            } else {
                0
            },
            client_down_bytes: if forwarded { self.bytes_out } else { 0 },
            plain_down_bytes: if forwarded {
                plain_out.max(self.bytes_out)
            } else {
                0
            },
        };

        (traffic != Traffic::default()).then_some(traffic)
    }
}

struct Logged {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    entry: LogEntry,
}

impl Stream for Logged {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.inner.as_mut().poll_next(cx);

        if let Poll::Ready(Some(Ok(chunk))) = &polled {
            if self.entry.ttfb_ms.is_none() {
                self.entry.ttfb_ms = Some(self.entry.started.elapsed().as_millis() as u64);
            }

            self.entry.bytes_out += chunk.len() as u64;
        }

        polled
    }
}

impl Drop for Logged {
    fn drop(&mut self) {
        let e = &self.entry;

        if let Some(traffic) = e.traffic() {
            e.recorder.record(traffic);
        }

        tracing::info!(
            target: "trout::request",
            request_id = %e.request_id,
            method = %e.method,
            route = e.route,
            target_host = e.target.as_deref().unwrap_or(""),
            status = e.status,
            source = %e.source,
            duration_ms = e.started.elapsed().as_millis() as u64,
            ttfb_ms = e.ttfb_ms.unwrap_or(0),
            bytes_in = e.counters.wire_in.load(Ordering::Relaxed),
            bytes_in_decoded = e.counters.decoded_in.load(Ordering::Relaxed),
            bytes_out = e.bytes_out,
            "request"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_kinds() {
        assert_eq!(route_kind("/healthz"), "admin");
        assert_eq!(route_kind("/nope"), "admin");
        assert_eq!(route_kind("/v1/chat/completions"), "proxy");
        assert_eq!(route_kind("/v1/responses"), "responses");
        assert_eq!(route_kind("/v1/responses/resp_1"), "responses");
        assert_eq!(route_kind("/v1/conversations/conv_1"), "responses");
    }
}
