use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use futures_util::{Stream, TryStreamExt};
use uuid::Uuid;

use crate::error::{SOURCE_HEADER, SOURCE_RELAY};

pub const REQUEST_ID_HEADER: &str = "x-trout-request-id";

/// Response extension set by handlers so the request log can name the target.
#[derive(Debug, Clone)]
pub struct TargetHost(pub String);

/// Shared with handlers through request extensions; filled in as the request is processed.
#[derive(Debug, Clone, Default)]
pub struct Counters {
    pub wire_in: Arc<AtomicU64>,
    pub decoded_in: Arc<AtomicU64>,
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
pub async fn record(mut request: Request, next: Next) -> Response {
    let request_id = Uuid::new_v4().simple().to_string();
    let method = request.method().clone();
    let route = route_kind(request.uri().path());
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
