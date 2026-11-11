use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::response::Response;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{RelayError, SOURCE_HEADER, SOURCE_RELAY};
use crate::store::stats::Traffic;

/// Days of history shown.
const DAYS: u32 = 30;

const PAGE: &str = include_str!("dashboard.html");

/// The page loads nothing from anywhere else and may only call back to this origin.
const PAGE_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                           connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// `GET /dashboard`: one static, self-contained page. It carries no data; it asks for the access
/// key and reads `/stats` with it.
pub async fn page() -> Response {
    let mut response = Response::new(Body::from(PAGE));
    let headers = response.headers_mut();

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(PAGE_POLICY));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(SOURCE_HEADER, HeaderValue::from_static(SOURCE_RELAY));

    response
}

fn with_savings(traffic: &Traffic) -> Value {
    let mut value = serde_json::to_value(traffic).unwrap_or_else(|_| json!({}));

    value["upload_saved_bytes"] = json!(traffic.upload_saved_bytes());
    value["download_saved_bytes"] = json!(traffic.download_saved_bytes());

    value
}

/// `GET /stats`: traffic totals, recent days and what the store holds. Counts and sizes only,
/// never content.
pub async fn stats(State(state): State<Arc<AppState>>) -> Result<Response, RelayError> {
    let stats = state.store.stats(DAYS).await?;
    let days: Vec<Value> = stats
        .days
        .iter()
        .map(|day| {
            let mut value = with_savings(&day.traffic);

            value["day"] = json!(day.day);

            value
        })
        .collect();
    let body = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at_ms": SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        "uptime_secs": state.started.elapsed().as_secs(),
        "totals": with_savings(&stats.totals),
        "days": days,
        "store": {
            "conversations": stats.store.conversations,
            "responses": stats.store.responses,
            "bytes": stats.store.bytes,
            "max_bytes": state.config.store.max_bytes,
            "ttl_secs": state.config.store.ttl_secs,
        },
    });
    let mut response = Response::new(Body::from(body.to_string()));
    let headers = response.headers_mut();

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(SOURCE_HEADER, HeaderValue::from_static(SOURCE_RELAY));

    Ok(response)
}
